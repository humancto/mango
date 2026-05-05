//! Phase 3 plan §11 integration tests for the `WatchableStore`
//! (ROADMAP.md:862, plan v3).
//!
//! Tests cross the writer hot path → registry dispatch → per-watcher
//! channel → `WatchStream::recv` boundary. The unit-level surface
//! checks (range membership, error variants, double-attach,
//! lifecycle) live in `crates/mango-mvcc/src/watchable_store.rs`'s
//! `#[cfg(test)] mod tests`.
//!
//! Test #14 (observer-double-attach smoke) lives in the module
//! `#[cfg(test)] mod tests` block, not here.
//!
//! Test #5 cross-watcher fanout uses 5 watchers (not 10) and 12 puts
//! (not 50) — the 10×50 figure in the plan was an upper-bound guide;
//! the invariant we assert (commit-revision ordering per watcher,
//! range filter correctness across watchers) holds at any size and
//! 5×12 keeps the test fast (< 50 ms) while still exercising
//! >1 watcher per dispatch and >1 dispatch per watcher.
//!
//! Under `--cfg madsim` this file is excluded — the writer's
//! `tokio::sync::Mutex` is the workspace `madsim-tokio` alias which,
//! under the simulator, has different scheduling guarantees than
//! the production tokio. Watch dispatch correctness under madsim is
//! Phase 5's concern.

#![cfg(not(madsim))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::arithmetic_side_effects,
    missing_docs,
    reason = "test code: panics are the assertion mechanism, arithmetic is bounded by loop counters"
)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mango_mvcc::store::range::RangeRequest;
use mango_mvcc::store::MvccStore;
use mango_mvcc::{DisconnectReason, MvccError, WatchError, WatchEventKind, WatchableStore};
use mango_storage::{Backend, BackendConfig, InMemBackend};
use tokio::time::timeout;

/// Open an `MvccStore<InMemBackend>` wrapped in `Arc` for use with
/// `WatchableStore::new`.
fn open() -> Arc<MvccStore<InMemBackend>> {
    let backend = InMemBackend::open(BackendConfig::new("/unused".into(), false))
        .expect("inmem open never fails");
    Arc::new(MvccStore::open(backend).expect("fresh open"))
}

/// Phase 3 plan §11 test #1. One `Put` on a watched key delivers
/// exactly one `Ok(WatchEvent { kind: Put, key, value, revision })`.
#[tokio::test(flavor = "current_thread")]
async fn single_put_delivered() {
    let store = open();
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 0)
        .expect("watch");

    let rev = store.put(b"k", b"v").await.expect("put");
    assert_eq!(rev.main(), 1);

    let ev = timeout(Duration::from_millis(500), s.recv())
        .await
        .expect("event delivered before timeout")
        .expect("channel open")
        .expect("Ok event");
    assert_eq!(ev.kind, WatchEventKind::Put);
    assert_eq!(ev.key, Bytes::from_static(b"k"));
    assert_eq!(ev.value, Bytes::from_static(b"v"));
    assert_eq!(ev.revision.main(), 1);
    assert_eq!(ev.revision.sub(), 0);
    assert!(ev.prev.is_none());
}

/// Phase 3 plan §11 test #2. One `DeleteRange` on a watched key
/// delivers one `Ok(WatchEvent { kind: Delete, value: empty,
/// revision })`.
#[tokio::test(flavor = "current_thread")]
async fn single_delete_delivered() {
    let store = open();
    // Pre-seed the key so DeleteRange has a tombstone to produce.
    let put_rev = store.put(b"k", b"v").await.expect("seed put");
    assert_eq!(put_rev.main(), 1);

    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 0)
        .expect("watch");

    let (deleted, rev) = store.delete_range(b"k", b"\xff").await.expect("delete");
    assert_eq!(deleted, 1);
    assert_eq!(rev.main(), 2);

    let ev = timeout(Duration::from_millis(500), s.recv())
        .await
        .expect("event delivered")
        .expect("channel open")
        .expect("Ok event");
    assert_eq!(ev.kind, WatchEventKind::Delete);
    assert_eq!(ev.key, Bytes::from_static(b"k"));
    assert!(ev.value.is_empty());
    assert_eq!(ev.revision.main(), 2);
    assert_eq!(ev.revision.sub(), 0);
}

/// Phase 3 plan §11 test #3. Watch `[a, b)`; Put on `c` → no event
/// delivered (timeout).
#[tokio::test(flavor = "current_thread")]
async fn range_filter_excludes() {
    let store = open();
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"a"), Bytes::from_static(b"b"), 0)
        .expect("watch [a, b)");

    let _rev = store.put(b"c", b"v").await.expect("put c");
    // 50 ms is plenty for a missed dispatch — channel ops are
    // microseconds. Timeout: Elapsed.
    let r = timeout(Duration::from_millis(50), s.recv()).await;
    assert!(r.is_err(), "no event expected, got {r:?}");
}

/// Phase 3 plan §11 test #4. Watch `[a, c)`; Put on `a`, `b`, `c` →
/// only `a` and `b` deliver.
#[tokio::test(flavor = "current_thread")]
async fn range_filter_includes_endpoints() {
    let store = open();
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"a"), Bytes::from_static(b"c"), 0)
        .expect("watch [a, c)");

    store.put(b"a", b"av").await.expect("put a");
    store.put(b"b", b"bv").await.expect("put b");
    store.put(b"c", b"cv").await.expect("put c");

    // First two deliver (a, b).
    let ev1 = timeout(Duration::from_millis(500), s.recv())
        .await
        .expect("ev1")
        .expect("open")
        .expect("ok");
    assert_eq!(ev1.key, Bytes::from_static(b"a"));
    let ev2 = timeout(Duration::from_millis(500), s.recv())
        .await
        .expect("ev2")
        .expect("open")
        .expect("ok");
    assert_eq!(ev2.key, Bytes::from_static(b"b"));

    // Third (c) is filtered out — channel is empty.
    let r = timeout(Duration::from_millis(50), s.recv()).await;
    assert!(r.is_err(), "expected timeout, got {r:?}");
}

/// Phase 3 plan §11 test #5 (renamed `multi_watcher_ordering_invariant`
/// per reviewer Missing #8). 5 watchers on overlapping ranges; 12 puts;
/// each watcher receives the puts that fall in its range, in
/// commit-revision order.
#[tokio::test(flavor = "current_thread")]
async fn multi_watcher_ordering_invariant() {
    let store = open();
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");

    // Five overlapping ranges across the alphabet a..=l (12 keys):
    //   w0: [a, c)    — covers a, b
    //   w1: [b, e)    — covers b, c, d
    //   w2: [d, h)    — covers d, e, f, g
    //   w3: [a, h)    — covers a..g
    //   w4: [k, m)    — covers k, l (disjoint from w0..w3 except via w3-not)
    let cases: &[(Bytes, Bytes)] = &[
        (Bytes::from_static(b"a"), Bytes::from_static(b"c")),
        (Bytes::from_static(b"b"), Bytes::from_static(b"e")),
        (Bytes::from_static(b"d"), Bytes::from_static(b"h")),
        (Bytes::from_static(b"a"), Bytes::from_static(b"h")),
        (Bytes::from_static(b"k"), Bytes::from_static(b"m")),
    ];
    let mut streams = Vec::with_capacity(cases.len());
    for (s, e) in cases {
        streams.push(ws.watch(s.clone(), e.clone(), 0).expect("watch"));
    }
    assert_eq!(ws.watcher_count(), cases.len());

    // 12 puts, keys a..=l, in alphabetic order. Revisions 1..=12.
    let keys: &[&[u8]] = &[
        b"a", b"b", b"c", b"d", b"e", b"f", b"g", b"h", b"i", b"j", b"k", b"l",
    ];
    for (i, k) in keys.iter().enumerate() {
        let rev = store.put(k, b"v").await.expect("put");
        assert_eq!(rev.main(), i64::try_from(i).expect("idx fits") + 1);
    }

    // Per-watcher expected key sets and rev sets.
    let expected_per_watcher: Vec<Vec<&[u8]>> = vec![
        vec![b"a", b"b"],
        vec![b"b", b"c", b"d"],
        vec![b"d", b"e", b"f", b"g"],
        vec![b"a", b"b", b"c", b"d", b"e", b"f", b"g"],
        vec![b"k", b"l"],
    ];

    for (i, s) in streams.iter_mut().enumerate() {
        let exp = &expected_per_watcher[i];
        let mut got = Vec::with_capacity(exp.len());
        for _ in 0..exp.len() {
            let ev = timeout(Duration::from_millis(500), s.recv())
                .await
                .unwrap_or_else(|_| panic!("watcher {i} stalled"))
                .expect("open")
                .expect("ok");
            got.push((ev.key.clone(), ev.revision.main()));
        }
        // Channel must be empty for all keys outside `exp`.
        let r = timeout(Duration::from_millis(50), s.recv()).await;
        assert!(
            r.is_err(),
            "watcher {i} got extra event after expected {} events: {r:?}",
            exp.len()
        );

        // Order: revs strictly monotonic (commit order).
        let mut last = 0i64;
        for (key, rev) in &got {
            assert!(
                *rev > last,
                "watcher {i} got non-monotonic rev: prev={last}, got={rev}",
            );
            last = *rev;
            // Key matches one of `exp` (in expected order — keys.iter()
            // is alphabetic and `exp` is filtered alphabetic, so the
            // sequence aligns).
            assert!(
                exp.contains(&key.as_ref()),
                "watcher {i} got unexpected key {key:?}",
            );
        }

        // Sequence equality (same keys in the same order as the alphabetic
        // commit sequence intersected with the watcher's range).
        let got_keys: Vec<&[u8]> = got.iter().map(|(k, _)| k.as_ref()).collect();
        assert_eq!(
            got_keys, *exp,
            "watcher {i}: got keys {got_keys:?} != expected {exp:?}",
        );
    }
}

/// Phase 3 plan §11 test #6. Drop `WatchStream` → `watcher_count()`
/// decrements; subsequent puts don't push to a stale channel.
#[tokio::test(flavor = "current_thread")]
async fn drop_stream_deregisters() {
    let store = open();
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 0)
        .expect("watch");
    assert_eq!(ws.watcher_count(), 1);

    drop(s);
    assert_eq!(ws.watcher_count(), 0);

    // Subsequent put completes without error and produces no
    // dispatch (registry is empty). The store has no other watchers,
    // so this is also a smoke test that the writer hot path does not
    // crash on an empty registry.
    store.put(b"k", b"v").await.expect("put after drop");
    assert_eq!(ws.watcher_count(), 0);
}

/// Phase 3 plan §11 test #8. `watch(.., 999)` on empty store; commit
/// puts up to rev 1000 → only the put at rev 1000 delivers (rev
/// 1..999 do not).
///
/// 999 is over the 1024 channel slot but well under any system limit;
/// we run with a small key so each put is microseconds. Total runtime
/// budget: < 5 s.
#[tokio::test(flavor = "current_thread")]
async fn start_rev_far_future_registers_verbatim() {
    let store = open();
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 999)
        .expect("watch at rev 999");

    // 1000 puts. The first 998 (revs 1..998) precede start_rev = 999
    // and must NOT be delivered. The 999th put has main = 999 — also
    // < 999 is false, so 999 >= 999 means it IS delivered. The 1000th
    // put has main = 1000, also delivered. Asserting the "only the
    // put at rev 1000" wording from the plan is a slight lie — both
    // 999 and 1000 satisfy `>= 999`; we actually expect both. Update
    // the assertion to match the eligibility filter (the plan's
    // wording was loose; the math is what we assert).
    for i in 1..=1000 {
        store.put(b"k", b"v").await.expect("put");
        let _ = i;
    }

    // First delivered event: rev 999.
    let ev = timeout(Duration::from_millis(500), s.recv())
        .await
        .expect("rev 999 event")
        .expect("open")
        .expect("ok");
    assert_eq!(ev.revision.main(), 999);

    // Second delivered event: rev 1000.
    let ev = timeout(Duration::from_millis(500), s.recv())
        .await
        .expect("rev 1000 event")
        .expect("open")
        .expect("ok");
    assert_eq!(ev.revision.main(), 1000);

    // No further events queued.
    let r = timeout(Duration::from_millis(50), s.recv()).await;
    assert!(r.is_err(), "expected timeout, got {r:?}");
}

/// Phase 3 plan §11 test #11 (NEW per reviewer Missing #4).
/// `range_query_after_event_delivery_is_consistent` — verifies the
/// dispatch-after-snapshot-store ordering invariant: a watcher that
/// receives an event for revision `R` is guaranteed to see
/// `current_revision() >= R` from any subsequent read on the same
/// store, and `range(k, None)` returns the value at `R`.
#[tokio::test(flavor = "current_thread")]
async fn range_query_after_event_delivery_is_consistent() {
    let store = open();
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 0)
        .expect("watch");

    let put_rev = store.put(b"k", b"hello").await.expect("put");
    assert_eq!(put_rev.main(), 1);

    let ev = timeout(Duration::from_millis(500), s.recv())
        .await
        .expect("event delivered")
        .expect("open")
        .expect("ok");
    assert_eq!(ev.revision.main(), 1);

    // (a) current_revision() on the store >= R.
    assert!(
        store.current_revision() >= 1,
        "current_revision() < 1 after event delivery — dispatch-after-store invariant violated",
    );

    // (b) range(k, None) returns the value at R.
    // RangeRequest is `#[non_exhaustive]` from outside the crate, so
    // we can't use struct-expression init; build via `default()` +
    // per-field mutation (matches `tests/redb_kv_smoke.rs` idiom).
    let mut req = RangeRequest::default();
    req.key = b"k".to_vec();
    let res = store.range(req).expect("range at current");
    assert_eq!(res.kvs.len(), 1);
    assert_eq!(res.kvs[0].key, Bytes::from_static(b"k"));
    assert_eq!(res.kvs[0].value, Bytes::from_static(b"hello"));
    assert_eq!(res.kvs[0].mod_revision.main(), 1);
    assert_eq!(res.header_revision, 1);

    // (c) range_at_rev(k, R) — range with explicit revision = R —
    // returns the same value/rev.
    let mut req = RangeRequest::default();
    req.key = b"k".to_vec();
    req.revision = Some(1);
    let res = store.range(req).expect("range at rev 1");
    assert_eq!(res.kvs.len(), 1);
    assert_eq!(res.kvs[0].value, Bytes::from_static(b"hello"));
    assert_eq!(res.kvs[0].mod_revision.main(), 1);
}

/// Phase 3 plan §11 test #7 redux at integration level — the unit
/// test exercises the same path but goes through the public API
/// surface here for completeness.
#[tokio::test(flavor = "current_thread")]
async fn start_rev_below_current_returns_unsupported_integration() {
    let store = open();
    store.put(b"a", b"v").await.expect("seed");
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    // current_revision() is 1; start_rev = 1 satisfies `<= current`.
    let err = ws
        .watch(Bytes::from_static(b"a"), Bytes::new(), 1)
        .expect_err("rejected");
    assert!(matches!(err, MvccError::Unsupported(_)), "got {err:?}");
}

/// Phase 3 plan §11 test #12 (`slow_consumer_disconnect_emits_signal`).
///
/// A watcher that does not poll its stream while the writer floods
/// more events than `EVENT_CAPACITY` (1024) eventually receives
/// a terminal `Err(WatchError::Disconnected(
/// DisconnectReason::SlowConsumer))`. The reserved permit (slot
/// 1025 of the 1025-cap channel) makes that terminal send
/// infallible. After the terminal item, the channel closes (the
/// writer's registry-write deregistration drops the `Sender`)
/// and the receiver returns `None`.
#[tokio::test(flavor = "current_thread")]
async fn slow_consumer_disconnect_emits_signal() {
    let store = open();
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 0)
        .expect("watch");

    // Each `put` produces one event. After 1024 puts the channel is
    // full; the 1025th put trips `try_send → Full`, consumes the
    // reserved permit (sending `Err(Disconnected(SlowConsumer))`),
    // and removes the watcher from the registry. Subsequent puts
    // skip dispatch entirely.
    for _ in 0..1100u32 {
        store.put(b"k", b"v").await.expect("put");
    }

    // Drain: 1024 `Ok(_)` events, then exactly one terminal
    // `Err(Disconnected(SlowConsumer))`.
    let mut ok_count = 0u32;
    let terminal = loop {
        let next = timeout(Duration::from_millis(500), s.recv())
            .await
            .expect("recv before timeout")
            .expect("channel still open until terminal");
        match next {
            Ok(_) => ok_count += 1,
            Err(e) => break e,
        }
    };

    assert_eq!(
        ok_count, 1024,
        "expected EVENT_CAPACITY = 1024 Ok events before disconnect, got {ok_count}",
    );
    assert!(
        matches!(
            terminal,
            WatchError::Disconnected(DisconnectReason::SlowConsumer),
        ),
        "got {terminal:?}",
    );

    // After the terminal item the Sender is dropped (registry
    // deregistration) and the channel closes.
    let after = timeout(Duration::from_millis(200), s.recv())
        .await
        .expect("recv before timeout");
    assert!(
        after.is_none(),
        "expected channel close after terminal Err, got {after:?}",
    );

    // Watcher fully removed from the registry by the dispatch path
    // (no separate user drop required).
    assert_eq!(ws.watcher_count(), 0);
}

/// Regression for the Arc-cycle / `StoreDropped` pair (rust-expert
/// PR #83 Showstoppers S2 + S3).
///
/// Dropping the last `Arc<WatchableStore>` must:
///
/// - Run `Drop for WatchableStore` (proves the cycle is broken — if
///   the trampoline observer held a strong Arc back to the
///   `WatchableStore` the user's last Arc would never be the last,
///   and Drop would never fire).
/// - Emit a terminal `Err(WatchError::Disconnected(
///   DisconnectReason::StoreDropped))` to every active watcher.
/// - Close the channel after the terminal Err (Sender dropped via
///   registry drain).
#[tokio::test(flavor = "current_thread")]
async fn dropping_watchable_store_emits_store_dropped_to_active_watchers() {
    let store = open();
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 0)
        .expect("watch");

    // Dropping the WatchableStore is what we're testing. The
    // underlying MvccStore stays alive (we hold `store`).
    drop(ws);

    let terminal = timeout(Duration::from_millis(500), s.recv())
        .await
        .expect("terminal Err delivered before timeout")
        .expect("channel still open until terminal")
        .expect_err("StoreDropped is the Err arm");

    assert!(
        matches!(
            terminal,
            WatchError::Disconnected(DisconnectReason::StoreDropped),
        ),
        "got {terminal:?}",
    );

    let after = timeout(Duration::from_millis(200), s.recv())
        .await
        .expect("recv before timeout");
    assert!(
        after.is_none(),
        "expected channel close after StoreDropped, got {after:?}",
    );
}
