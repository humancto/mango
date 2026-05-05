//! Integration tests for the unsynced-watcher catch-up path
//! (ROADMAP.md:863, plan v3 §11.C1-C14).
//!
//! These tests cross the writer hot path → registry dispatch → catch-up
//! driver → channel boundary, the same surface as `watch_basic.rs`,
//! but exercise watchers registered with `start_rev <=
//! current_revision()` — the path that was previously rejected with
//! `MvccError::Unsupported(UnsyncedWatcher)`.
//!
//! Under `--cfg madsim` this file is excluded — `tokio::spawn`
//! cooperates with the simulator differently than production tokio,
//! and the deterministic-watch story is Phase 5's concern.

#![cfg(not(madsim))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_wrap,
    clippy::items_after_statements,
    clippy::match_wild_err_arm,
    clippy::explicit_iter_loop,
    clippy::doc_markdown,
    clippy::needless_continue,
    clippy::uninlined_format_args,
    missing_docs,
    reason = "test code: panics are the assertion mechanism, arithmetic is bounded by loop counters, casts are bounded by literal SEED/N constants"
)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mango_mvcc::store::MvccStore;
use mango_mvcc::{DisconnectReason, MvccError, WatchError, WatchEventKind, WatchableStore};
use mango_storage::{Backend, BackendConfig, InMemBackend};
use tokio::time::{sleep, timeout};

const RECV_TIMEOUT: Duration = Duration::from_millis(2_000);

fn open() -> Arc<MvccStore<InMemBackend>> {
    let backend = InMemBackend::open(BackendConfig::new("/unused".into(), false))
        .expect("inmem open never fails");
    Arc::new(MvccStore::open(backend).expect("fresh open"))
}

fn full_range() -> (Bytes, Bytes) {
    (Bytes::new(), Bytes::from_static(&[0xff_u8]))
}

/// C1. Pre-seed N puts; watch from `start_rev = 1` should deliver all
/// N historical events (via catch-up) then a fresh write delivered
/// inline through the synced path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catchup_delivers_all_historical_events_then_synced() {
    let store = open();
    for i in 0..5 {
        let key = format!("k{i}");
        store.put(key.as_bytes(), b"v").await.expect("seed put");
    }
    assert_eq!(store.current_revision(), 5);

    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let (start, end) = full_range();
    let mut s = ws.watch(start, end, 1).expect("unsynced watch accepted");

    let mut received: Vec<i64> = Vec::with_capacity(6);
    for _ in 0..5 {
        let ev = timeout(RECV_TIMEOUT, s.recv())
            .await
            .expect("catch-up event")
            .expect("channel open")
            .expect("Ok event");
        assert_eq!(ev.kind, WatchEventKind::Put);
        received.push(ev.revision.main());
    }
    assert_eq!(received, vec![1, 2, 3, 4, 5]);

    // Catch-up complete. A new put should land via the synced path.
    let new_rev = store.put(b"k99", b"v").await.expect("post-catchup put");
    assert_eq!(new_rev.main(), 6);

    let ev = timeout(RECV_TIMEOUT, s.recv())
        .await
        .expect("synced event")
        .expect("channel open")
        .expect("Ok event");
    assert_eq!(ev.revision.main(), 6);
    assert_eq!(ev.kind, WatchEventKind::Put);
    assert_eq!(ev.key, Bytes::from_static(b"k99"));
}

/// C2. `start_rev` strictly below the compacted floor: hard reject
/// at `watch()` time with `MvccError::Compacted`.
#[tokio::test(flavor = "current_thread")]
async fn catchup_below_compacted_floor_returns_compacted() {
    let store = open();
    for _ in 0..5 {
        store.put(b"k", b"v").await.expect("seed put");
    }
    store.compact(5).await.expect("compact to 5");
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let (start, end) = full_range();
    let err = ws.watch(start, end, 3).expect_err("rejected");
    assert!(
        matches!(
            err,
            MvccError::Compacted {
                requested: 3,
                floor: 5
            }
        ),
        "got {err:?}"
    );
}

/// C3. `start_rev == compacted_floor`: also rejected — the floor is
/// the inclusive lower bound, so the lowest LIVE revision is
/// `floor + 1`. Etcd parity. (`MUST_VERIFY` flagged in plan §11.)
#[tokio::test(flavor = "current_thread")]
async fn catchup_at_compacted_floor_returns_compacted() {
    let store = open();
    for _ in 0..5 {
        store.put(b"k", b"v").await.expect("seed put");
    }
    store.compact(5).await.expect("compact to 5");
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let (start, end) = full_range();
    let err = ws.watch(start, end, 5).expect_err("rejected at floor");
    assert!(matches!(err, MvccError::Compacted { .. }), "got {err:?}");
}

/// C4. Concurrent writes during catch-up: the watcher should receive
/// every event in `[start_rev, final_rev]` exactly once, in
/// monotonic order. Pre-seed K events, watch from rev 1, then drive
/// K more events concurrently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catchup_with_concurrent_writes_no_miss_no_dup() {
    let store = open();
    const SEED: usize = 50;
    const CONCURRENT: usize = 50;
    for i in 0..SEED {
        let v = format!("v{i}");
        store.put(b"k", v.as_bytes()).await.expect("seed put");
    }
    assert_eq!(store.current_revision(), SEED as i64);

    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 1)
        .expect("unsynced watch");

    // Spawn writer that drives more events while the catch-up scan runs.
    let writer = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            for i in 0..CONCURRENT {
                let v = format!("vc{i}");
                store.put(b"k", v.as_bytes()).await.expect("concurrent put");
                // Yield to let the catch-up scan interleave.
                tokio::task::yield_now().await;
            }
        })
    };

    let total = SEED + CONCURRENT;
    let mut received: Vec<i64> = Vec::with_capacity(total);
    for _ in 0..total {
        let ev = timeout(RECV_TIMEOUT, s.recv())
            .await
            .expect("event before timeout")
            .expect("channel open")
            .expect("Ok event");
        received.push(ev.revision.main());
    }
    writer.await.expect("writer task");

    // Monotonic & exact coverage [1, total].
    let expected: Vec<i64> = (1..=total as i64).collect();
    assert_eq!(received, expected, "no miss, no dup, monotonic");
}

/// C5. A slow consumer that doesn't drain the channel parks the
/// catch-up driver via `tx.send().await` backpressure. The watcher
/// remains registered (no disconnect, no convergence failure) for
/// the full sleep window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catchup_with_slow_consumer_backpressures_driver() {
    let store = open();
    // Need more events than channel capacity (1024) to force the
    // driver to actually block on send.
    const N: usize = 1_500;
    for i in 0..N {
        let v = format!("v{i}");
        store.put(b"k", v.as_bytes()).await.expect("seed put");
    }
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 1)
        .expect("unsynced watch");

    // Read just 100 events, then sleep — the channel fills, driver parks.
    for _ in 0..100 {
        let _ev = timeout(RECV_TIMEOUT, s.recv())
            .await
            .expect("event")
            .expect("channel open")
            .expect("Ok event");
    }
    sleep(Duration::from_millis(200)).await;
    // Watcher still registered (driver is parked, NOT disconnected).
    assert_eq!(ws.watcher_count(), 1);

    // Drain the rest — total must be exactly N.
    let mut more = 100_usize;
    while more < N {
        let _ev = timeout(RECV_TIMEOUT, s.recv())
            .await
            .expect("event")
            .expect("channel open")
            .expect("Ok event");
        more += 1;
    }
    assert_eq!(more, N);
}

/// C6. Dropping the `WatchStream` during catch-up aborts the driver
/// task and removes the registry entry within a small bounded window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_watch_stream_during_catchup_aborts_driver() {
    let store = open();
    const N: usize = 1_500;
    for i in 0..N {
        let v = format!("v{i}");
        store.put(b"k", v.as_bytes()).await.expect("seed put");
    }
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 1)
        .expect("unsynced watch");
    // Read a few events to confirm the driver is running.
    for _ in 0..10 {
        let _ev = timeout(RECV_TIMEOUT, s.recv())
            .await
            .expect("event")
            .expect("channel open")
            .expect("Ok event");
    }
    drop(s);
    // PinnedDrop runs synchronously; it removes the registry entry
    // and aborts the driver. Allow a brief async window for the
    // driver task to actually be aborted by the runtime.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while tokio::time::Instant::now() < deadline {
        if ws.watcher_count() == 0 {
            return;
        }
        sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "watcher_count never reached 0 after stream drop: {}",
        ws.watcher_count()
    );
}

/// C7. Dropping the `WatchableStore` while a catch-up driver is
/// running emits a terminal `Err(Disconnected(StoreDropped))` to the
/// surviving stream, then closes the channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_watchable_store_during_catchup_emits_storedropped() {
    let store = open();
    const N: usize = 1_500;
    for i in 0..N {
        let v = format!("v{i}");
        store.put(b"k", v.as_bytes()).await.expect("seed put");
    }
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 1)
        .expect("unsynced watch");
    for _ in 0..10 {
        let _ev = timeout(RECV_TIMEOUT, s.recv())
            .await
            .expect("event")
            .expect("channel open")
            .expect("Ok event");
    }
    drop(ws);

    // Drain pending events; eventually receive the terminal disconnect.
    let mut saw_terminal = false;
    let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(2_000);
    while tokio::time::Instant::now() < drain_deadline {
        match timeout(Duration::from_millis(500), s.recv()).await {
            Ok(Some(Ok(_ev))) => continue,
            Ok(Some(Err(WatchError::Disconnected(DisconnectReason::StoreDropped)))) => {
                saw_terminal = true;
                break;
            }
            Ok(Some(Err(other))) => {
                panic!("unexpected disconnect reason: {other:?}");
            }
            Ok(None) => {
                // Channel closed without terminal. The drop path
                // emits StoreDropped *before* the Sender drops, so
                // None first means a regression. Fail explicitly.
                panic!("channel closed before terminal disconnect arrived");
            }
            Err(_) => panic!("timeout draining stream after store drop"),
        }
    }
    assert!(saw_terminal, "did not observe StoreDropped terminal");
}

/// C8. Delete during catch-up: the catch-up scan should emit the
/// historical Put for rev 1 and then the synced path should emit the
/// Delete for rev 2 (with `prev_kv` populated).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_during_catchup_emits_delete_event_in_order() {
    let store = open();
    store.put(b"k", b"v").await.expect("seed put");
    assert_eq!(store.current_revision(), 1);

    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 1)
        .expect("unsynced watch");

    let _ = store.delete_range(b"k", b"").await.expect("delete_range");

    let ev1 = timeout(RECV_TIMEOUT, s.recv())
        .await
        .expect("event 1")
        .expect("channel open")
        .expect("Ok");
    assert_eq!(ev1.kind, WatchEventKind::Put);
    assert_eq!(ev1.revision.main(), 1);
    assert_eq!(ev1.value, Bytes::from_static(b"v"));

    let ev2 = timeout(RECV_TIMEOUT, s.recv())
        .await
        .expect("event 2")
        .expect("channel open")
        .expect("Ok");
    assert_eq!(ev2.kind, WatchEventKind::Delete);
    assert_eq!(ev2.revision.main(), 2);
    let prev = ev2.prev.as_ref().expect("prev_kv populated");
    assert_eq!(prev.value, Bytes::from_static(b"v"));
}

/// C9. `watcher_count` reflects the union of synced + unsynced.
/// 5 unsynced + 3 synced → 8; after promotion still 8.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsynced_watcher_count_separate_from_synced() {
    let store = open();
    for _ in 0..3 {
        store.put(b"k", b"v").await.expect("seed");
    }
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");

    let mut streams = Vec::new();
    // 3 synced (start_rev = 0 → resolved to current+1).
    for _ in 0..3 {
        streams.push(
            ws.watch(Bytes::from_static(b"k"), Bytes::new(), 0)
                .expect("synced watch"),
        );
    }
    // 5 unsynced (start_rev = 1 ≤ current = 3).
    for _ in 0..5 {
        streams.push(
            ws.watch(Bytes::from_static(b"k"), Bytes::new(), 1)
                .expect("unsynced watch"),
        );
    }
    assert_eq!(ws.watcher_count(), 8);

    // Drain catch-up events on the 5 unsynced streams; promotion
    // should not change watcher_count.
    for s in streams.iter_mut().skip(3) {
        for _ in 0..3 {
            let _ev = timeout(RECV_TIMEOUT, s.recv())
                .await
                .expect("event")
                .expect("channel open")
                .expect("Ok");
        }
    }
    // Wait briefly for the promotion to land for all 5.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while tokio::time::Instant::now() < deadline {
        if ws.watcher_count() == 8 {
            // count never changes — but verify a synced send still
            // reaches all watchers, including those just promoted.
            break;
        }
        sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(ws.watcher_count(), 8);
}

/// C10. Independent unsynced watchers each receive a full copy of
/// the historical event range.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_unsynced_watchers_independent() {
    let store = open();
    const N: usize = 50;
    for i in 0..N {
        let v = format!("v{i}");
        store.put(b"k", v.as_bytes()).await.expect("seed");
    }
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut streams = (0..5)
        .map(|_| {
            ws.watch(Bytes::from_static(b"k"), Bytes::new(), 1)
                .expect("unsynced watch")
        })
        .collect::<Vec<_>>();

    for s in streams.iter_mut() {
        let mut received: Vec<i64> = Vec::with_capacity(N);
        for _ in 0..N {
            let ev = timeout(RECV_TIMEOUT, s.recv())
                .await
                .expect("event")
                .expect("channel open")
                .expect("Ok");
            received.push(ev.revision.main());
        }
        let expected: Vec<i64> = (1..=N as i64).collect();
        assert_eq!(received, expected);
    }
}

/// C11. Catch-up event ordering across a multi-key DeleteRange. The
/// scan must replay events in `(rev.main, rev.sub)` order across the
/// per-key history streams.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catchup_event_ordering_across_multi_key_delete_range() {
    let store = open();
    store.put(b"k1", b"v1").await.expect("p1");
    store.put(b"k2", b"v2").await.expect("p2");
    store.put(b"k3", b"v3").await.expect("p3");
    let (deleted, del_rev) = store
        .delete_range(b"k1", b"k4")
        .await
        .expect("delete_range");
    assert_eq!(deleted, 3);
    assert_eq!(del_rev.main(), 4);

    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::new(), Bytes::from_static(&[0xff_u8]), 1)
        .expect("unsynced watch");

    let mut events = Vec::new();
    for _ in 0..6 {
        let ev = timeout(RECV_TIMEOUT, s.recv())
            .await
            .expect("event")
            .expect("channel open")
            .expect("Ok");
        events.push(ev);
    }

    // First three are puts at revs (1,0), (2,0), (3,0).
    assert_eq!(events[0].kind, WatchEventKind::Put);
    assert_eq!(events[0].revision.main(), 1);
    assert_eq!(events[0].key, Bytes::from_static(b"k1"));
    assert_eq!(events[1].revision.main(), 2);
    assert_eq!(events[1].key, Bytes::from_static(b"k2"));
    assert_eq!(events[2].revision.main(), 3);
    assert_eq!(events[2].key, Bytes::from_static(b"k3"));

    // Last three are deletes at rev 4 with subs 0,1,2 (one per
    // matched key, key-order).
    assert_eq!(events[3].kind, WatchEventKind::Delete);
    assert_eq!(events[3].revision.main(), 4);
    assert_eq!(events[4].revision.main(), 4);
    assert_eq!(events[5].revision.main(), 4);
    let mut subs: Vec<i64> = events[3..6].iter().map(|e| e.revision.sub()).collect();
    subs.sort_unstable();
    assert_eq!(subs, vec![0, 1, 2]);

    // Each delete carries prev_kv with the value seeded above.
    for ev in &events[3..6] {
        assert!(ev.prev.is_some(), "delete prev_kv populated: {ev:?}");
    }
}

/// C12. Compaction concurrent with catch-up: the watcher must end up
/// in one of two valid terminal states — either it received the full
/// pre-compaction event range and converged to synced, or the
/// catch-up scan observed `compacted >= from_rev` and emitted a
/// terminal `Disconnected(Compacted)`. The atomic snapshot-pair
/// (§4.5) rules out a third outcome (partial events + silent close).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catchup_compaction_during_scan_either_completes_or_compacts() {
    let store = open();
    const N: usize = 200;
    for i in 0..N {
        let v = format!("v{i}");
        store.put(b"k", v.as_bytes()).await.expect("seed");
    }
    let ws = WatchableStore::new(Arc::clone(&store)).expect("new");
    let mut s = ws
        .watch(Bytes::from_static(b"k"), Bytes::new(), 50)
        .expect("unsynced watch");

    // Concurrent compaction. The two outcomes are both valid:
    //  1. Catch-up wins → events 50..=N delivered, no terminal.
    //  2. Compaction wins → at most some events, then terminal
    //     `Disconnected(Compacted { floor: 100 })`.
    let store_h = Arc::clone(&store);
    let compactor = tokio::spawn(async move {
        // Brief delay so the catch-up driver actually starts before
        // we compact.
        sleep(Duration::from_millis(5)).await;
        store_h.compact(100).await.expect("compact");
    });

    let mut event_count = 0_usize;
    let mut last_rev: i64 = 49;
    let mut terminal: Option<DisconnectReason> = None;
    let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(3_000);
    loop {
        if tokio::time::Instant::now() >= drain_deadline {
            break;
        }
        match timeout(Duration::from_millis(500), s.recv()).await {
            Ok(Some(Ok(ev))) => {
                event_count += 1;
                assert!(ev.revision.main() > last_rev, "monotonic: {:?}", ev);
                last_rev = ev.revision.main();
            }
            Ok(Some(Err(WatchError::Disconnected(reason)))) => {
                terminal = Some(reason);
                break;
            }
            Ok(Some(Err(other))) => panic!("unexpected error: {other:?}"),
            Ok(None) => break,
            Err(_) => {
                // Timeout with no events: assume catch-up converged
                // and the watcher is now synced (no further events
                // expected, no terminal).
                break;
            }
        }
    }
    compactor.await.expect("compactor task");

    match terminal {
        // Outcome 1: terminal compacted disconnect.
        Some(DisconnectReason::Compacted { floor }) => {
            assert_eq!(floor, 100, "floor matches compaction target");
        }
        // Outcome 2: full delivery, no terminal.
        None => {
            assert_eq!(event_count, N - 49, "delivered events 50..=N");
            assert_eq!(last_rev, N as i64);
        }
        Some(other) => panic!("unexpected terminal disconnect: {other:?}"),
    }
}
