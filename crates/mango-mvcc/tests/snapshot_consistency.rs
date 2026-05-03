//! Snapshot consistency property test (ROADMAP:847).
//!
//! Under concurrent reads + writes, every reader must see a
//! [`mango_mvcc::Snapshot`] that is either "the snapshot at
//! read-time" or "a snapshot that committed before read-time."
//! No torn snapshots.
//!
//! # Relationship to the in-source test
//!
//! `crates/mango-mvcc/src/store/mod.rs::snapshot_publish_is_coherent_pair`
//! covers two of the four invariants below at high pressure
//! (`5_000` puts, 3 readers, single hardcoded case):
//!
//! - **(I1) No torn pair**: `compacted <= rev`.
//! - **(I2) Per-reader monotonicity**: `rev` and `compacted`
//!   non-decreasing across one reader's observations.
//!
//! What the in-source test cannot express, and this file adds:
//!
//! - **(I3) Read-after-write visibility**: if `put().await`
//!   returns `Ok(rev=M)` at wall-clock `t_ack`, every subsequent
//!   reader observation at `t_obs >= t_ack` must satisfy
//!   `snap.rev >= M`.
//! - **(I4) Read-after-compact visibility** (symmetric of I3):
//!   if `compact(F).await` returns Ok at `t_ack_c`, every
//!   subsequent reader observation at `t_obs >= t_ack_c` must
//!   satisfy `snap.compacted >= F`.
//! - **(I5) Range / snapshot coherence**: a `range(req)` with
//!   `req.revision = None` returns `header_revision >= snap.rev`
//!   for the snapshot loaded immediately before the call.
//! - **(I6) Snapshot ↔ data coherence (end-of-case)**: the
//!   final snapshot's `range(revision = Some(snap.rev))` over
//!   the full keyspace returns exactly the writer log's
//!   reconstructed live-key set.
//!
//! The in-source test runs (I1) + (I2) at high pressure on the
//! `cargo test` hot path. This file shrinks I1+I2 to a post-test
//! sanity walk and makes I3+I4+I5+I6 the headline.
//!
//! # Soundness of the wall-clock cross-correlation
//!
//! The chain that makes I3 sound:
//!
//! 1. Writer publishes a fresh `Arc<Snapshot>` via
//!    [`arc_swap::ArcSwap::store`] (Release). See
//!    `store/mod.rs:381` (the `put` publish point) — the
//!    `store` call is the LAST observable side-effect before
//!    the `await` returns `Ok(rev)` to the caller.
//! 2. Caller (the writer task in this test) immediately
//!    captures `t_ack = Instant::now()`.
//! 3. Reader (a different task on a different thread) captures
//!    `t_obs = Instant::now()`. `Instant` wraps
//!    `clock_gettime(CLOCK_MONOTONIC)` on Unix and
//!    `QueryPerformanceCounter` on Windows — globally monotonic
//!    across threads on Linux/macOS (Mango's CI matrix). Any
//!    `t_obs >= t_ack` is sequenced after `t_ack` in real time.
//! 4. Reader does `self.snapshot.load_full()` (Acquire).
//!
//! Because the publish in step 1 happens-before step 2 in the
//! writer thread's program order, and steps 2 → 3 are sequenced
//! by `Instant` global monotonicity, and step 4's Acquire pairs
//! with step 1's Release in `ArcSwap`'s modification order, the
//! reader observes a snapshot with `rev >= M`. The publish
//! protocol in `MvccStore` is monotonic (writers only advance
//! `rev`; `next_main` is allocated under the writer mutex), so
//! "rev >= M" cannot regress.
//!
//! If a future refactor changed `put` to publish *after* the
//! `Ok(rev)` return (or to use a CAS that could lose), this
//! test would catch the regression.
//!
//! # Miri / madsim
//!
//! - **Miri**: not run. Multi-thread Tokio + `spawn_blocking`
//!   are unsupported under Miri's `-Zmiri-disable-isolation`
//!   mode. The single-threaded subset of these invariants is
//!   covered by `snapshot_publish_is_coherent_pair`, which runs
//!   under the standard `cargo test` path.
//! - **madsim**: excluded via `#![cfg(not(madsim))]`. The test
//!   depends on real `Instant` for the wall-clock correlation
//!   in I3/I4; under madsim's virtual time the correlation
//!   degenerates to single-threaded sequencing, which adds no
//!   value over the in-source test.

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
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    missing_docs,
    reason = "test code: panics are the assertion mechanism, arithmetic is bounded by loop counters"
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mango_mvcc::store::range::RangeRequest;
use mango_mvcc::store::MvccStore;
use mango_mvcc::Snapshot;
use mango_storage::{Backend, BackendConfig, InMemBackend};
use parking_lot::Mutex;
use proptest::prelude::*;

/// Per-case workload spec. Proptest generates and shrinks this.
///
/// Bounds chosen tight (per rust-expert review): the
/// cross-correlation invariants fail at small case sizes if they
/// fail at all, and tight bounds make 32 cases fit under the
/// 60s test budget.
#[derive(Debug, Clone)]
struct WorkloadSpec {
    num_writers: usize,
    num_readers: usize,
    puts_per_writer: usize,
    /// `0` disables compaction. Otherwise: writer #0 issues
    /// `compact(current_rev / 2)` every Nth iteration.
    compact_every: usize,
}

fn workload_spec() -> impl Strategy<Value = WorkloadSpec> {
    (
        1usize..=3,                             // num_writers
        1usize..=3,                             // num_readers
        32usize..=64,                           // puts_per_writer
        prop_oneof![Just(0usize), 8usize..=32], // compact_every (0 disables)
    )
        .prop_map(
            |(num_writers, num_readers, puts_per_writer, compact_every)| WorkloadSpec {
                num_writers,
                num_readers,
                puts_per_writer,
                compact_every,
            },
        )
}

/// One reader observation. Per-reader log; no shared lock on
/// the hot path.
#[derive(Clone, Copy, Debug)]
struct ReaderObs {
    obs_time: Instant,
    rev: i64,
    compacted: i64,
    /// `Some(header_rev)` on iterations where the reader also
    /// did a `range()` probe (every 16th iter, for I5).
    range_header_rev: Option<i64>,
}

/// Writer ack: `(rev, t_ack)` for `put` ack log; `(floor, t_ack)`
/// for compact ack log. Same shape, different semantics.
type AckEntry = (i64, Instant);

/// Writer key log entry: `(key, mod_revision)`. Used for I6
/// reconstruction.
type KeyLogEntry = (Vec<u8>, i64);

fn fresh_store() -> Arc<MvccStore<InMemBackend>> {
    let backend = InMemBackend::open(BackendConfig::new("/unused".into(), false))
        .expect("inmem open never fails");
    Arc::new(MvccStore::open(backend).expect("fresh open"))
}

/// Build the per-writer key set. Disjoint across writers so
/// writer i's puts don't compete with writer j's on
/// `KeyHistory::put`'s monotonic constraint (the writer mutex
/// already serializes them; this is for a clearer model).
fn key(writer_id: usize, idx: usize) -> Vec<u8> {
    format!("w{writer_id:02}/k{idx:08}").into_bytes()
}

fn value(writer_id: usize, idx: usize) -> Vec<u8> {
    format!("v{writer_id}/{idx}").into_bytes()
}

/// Writer task body. Iterates `puts_per_writer` ops, logging
/// each successful ack into `put_acks` and `key_log`. Optionally
/// issues `compact()` every `compact_every` iters (writer #0
/// only, to avoid two writers racing on the floor).
async fn run_writer(
    writer_id: usize,
    spec: WorkloadSpec,
    store: Arc<MvccStore<InMemBackend>>,
    put_acks: Arc<Mutex<Vec<AckEntry>>>,
    compact_acks: Arc<Mutex<Vec<AckEntry>>>,
    key_log: Arc<Mutex<Vec<KeyLogEntry>>>,
) {
    for i in 0..spec.puts_per_writer {
        let k = key(writer_id, i);
        let v = value(writer_id, i);
        let rev = store.put(&k, &v).await.expect("put");
        // Capture t_ack AFTER the await returns. The publish
        // (via ArcSwap::store at store/mod.rs:381) is the last
        // side-effect inside `put` before `Ok(rev)` returns,
        // so by the time Instant::now() runs here, the snapshot
        // is globally visible.
        let t_ack = Instant::now();
        put_acks.lock().push((rev.main(), t_ack));
        key_log.lock().push((k, rev.main()));

        if writer_id == 0
            && spec.compact_every > 0
            && i % spec.compact_every == spec.compact_every - 1
        {
            let target = store.current_revision() / 2;
            if target > 0 {
                store.compact(target).await.expect("compact");
                let t_ack_c = Instant::now();
                compact_acks.lock().push((target, t_ack_c));
            }
        }
    }
}

/// Reader task body — runs on `spawn_blocking` so it sits on a
/// dedicated thread, racing the writers concurrently.
fn run_reader(store: Arc<MvccStore<InMemBackend>>, stop: Arc<AtomicBool>) -> Vec<ReaderObs> {
    let mut log: Vec<ReaderObs> = Vec::with_capacity(8192);
    let mut iter: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        let t = Instant::now();
        let snap = store.current_snapshot();
        let mut obs = ReaderObs {
            obs_time: t,
            rev: snap.rev,
            compacted: snap.compacted,
            range_header_rev: None,
        };

        // I5 probe: every 16th iter, also do a Range and pin
        // header_revision against the snapshot we just loaded.
        if iter.is_multiple_of(16) {
            let req = RangeRequest::default(); // revision = None
            if let Ok(resp) = store.range(req) {
                obs.range_header_rev = Some(resp.header_revision);
            }
        }

        log.push(obs);
        iter = iter.wrapping_add(1);
        std::hint::spin_loop();
    }
    log
}

/// Run one proptest case. Returns `Result<(), TestCaseError>`
/// so proptest can shrink on failure.
async fn run_one_case(spec: WorkloadSpec) -> Result<(), TestCaseError> {
    let store = fresh_store();
    let stop = Arc::new(AtomicBool::new(false));
    let put_acks: Arc<Mutex<Vec<AckEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let compact_acks: Arc<Mutex<Vec<AckEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let key_log: Arc<Mutex<Vec<KeyLogEntry>>> = Arc::new(Mutex::new(Vec::new()));

    // Spawn readers first so they're already polling when the
    // writers begin — exercises the early-rev publication path.
    let mut reader_handles = Vec::new();
    for _ in 0..spec.num_readers {
        let s = Arc::clone(&store);
        let stop = Arc::clone(&stop);
        reader_handles.push(tokio::task::spawn_blocking(move || run_reader(s, stop)));
    }

    let mut writer_handles = Vec::new();
    for w_id in 0..spec.num_writers {
        let s = Arc::clone(&store);
        let put_acks = Arc::clone(&put_acks);
        let compact_acks = Arc::clone(&compact_acks);
        let key_log = Arc::clone(&key_log);
        let spec_w = spec.clone();
        writer_handles.push(tokio::spawn(async move {
            run_writer(w_id, spec_w, s, put_acks, compact_acks, key_log).await;
        }));
    }

    for h in writer_handles {
        h.await.expect("writer task");
    }
    stop.store(true, Ordering::Relaxed);

    let mut reader_logs: Vec<Vec<ReaderObs>> = Vec::with_capacity(spec.num_readers);
    for h in reader_handles {
        reader_logs.push(h.await.expect("reader task"));
    }

    // Capture final snapshot for I6 BEFORE we drop anything.
    let final_snap: Arc<Snapshot> = store.current_snapshot();

    // ---- Post-test invariant checks ----

    // Sanity: writer log non-empty, every reader saw at least
    // one sample. A reader with zero samples means the case
    // ran so fast the readers never got polled — skip the
    // cross-correlation for that reader (degenerate case).
    let put_acks_snapshot = put_acks.lock().clone();
    prop_assert!(
        !put_acks_snapshot.is_empty(),
        "writer ack log is empty — no puts completed"
    );

    // I1 + I2: per-reader sanity walk.
    for (i, log) in reader_logs.iter().enumerate() {
        let mut prev_rev: i64 = 0;
        let mut prev_compacted: i64 = 0;
        for (j, obs) in log.iter().enumerate() {
            prop_assert!(
                obs.compacted <= obs.rev,
                "reader {i} sample {j}: torn pair — compacted={} > rev={}",
                obs.compacted,
                obs.rev,
            );
            prop_assert!(
                obs.rev >= prev_rev,
                "reader {i} sample {j}: rev went backwards: {prev_rev} -> {}",
                obs.rev,
            );
            prop_assert!(
                obs.compacted >= prev_compacted,
                "reader {i} sample {j}: compacted went backwards: {prev_compacted} -> {}",
                obs.compacted,
            );
            prev_rev = obs.rev;
            prev_compacted = obs.compacted;
        }
    }

    // I3: read-after-write visibility. Build a sorted ack log
    // and use binary search to find the committed prefix at
    // each reader observation time.
    let mut sorted_put_acks = put_acks_snapshot.clone();
    sorted_put_acks.sort_by_key(|&(_, t)| t);

    for (i, log) in reader_logs.iter().enumerate() {
        for (j, obs) in log.iter().enumerate() {
            // Largest rev with ack_time <= obs.obs_time.
            let committed_prefix = committed_prefix_at(&sorted_put_acks, obs.obs_time);
            prop_assert!(
                obs.rev >= committed_prefix,
                "reader {i} sample {j}: I3 violated — \
                 committed_prefix={committed_prefix} but snap.rev={}",
                obs.rev,
            );
        }
    }

    // I4: read-after-compact visibility (symmetric to I3).
    let mut sorted_compact_acks = compact_acks.lock().clone();
    sorted_compact_acks.sort_by_key(|&(_, t)| t);

    if !sorted_compact_acks.is_empty() {
        for (i, log) in reader_logs.iter().enumerate() {
            for (j, obs) in log.iter().enumerate() {
                let committed_floor = committed_prefix_at(&sorted_compact_acks, obs.obs_time);
                prop_assert!(
                    obs.compacted >= committed_floor,
                    "reader {i} sample {j}: I4 violated — \
                     committed_floor={committed_floor} but snap.compacted={}",
                    obs.compacted,
                );
            }
        }
    }

    // I5: range coherence. For every range probe the reader
    // did, header_revision must be >= the snap.rev observed
    // immediately before the range. (>= because range loads
    // its own snapshot, which could be strictly later.)
    for (i, log) in reader_logs.iter().enumerate() {
        for (j, obs) in log.iter().enumerate() {
            if let Some(hdr) = obs.range_header_rev {
                prop_assert!(
                    hdr >= obs.rev,
                    "reader {i} sample {j}: I5 violated — \
                     range header_revision={hdr} < snap.rev={}",
                    obs.rev,
                );
            }
        }
    }

    // I6: snapshot ↔ data coherence at end of case. Every
    // distinct key in the writer log with mod_revision <=
    // final_snap.rev should be live at final_snap.rev (this
    // test never deletes, so all puts are live).
    let key_log_snapshot = key_log.lock().clone();
    let mut expected_keys: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    for (k, m) in &key_log_snapshot {
        if *m <= final_snap.rev {
            expected_keys.insert(k.clone());
        }
    }

    // Range over the full keyspace at final_snap.rev. Bounds
    // chosen to cover every possible key the writer produced
    // (writer prefix is "w%02u/", so [b"w", b"x") covers
    // every writer's keys).
    let mut req = RangeRequest::default();
    req.key = b"w".to_vec();
    req.end = b"x".to_vec();
    req.revision = Some(final_snap.rev);
    let resp = store.range(req).expect("final range");

    let actual_keys: std::collections::BTreeSet<Vec<u8>> =
        resp.kvs.iter().map(|kv| kv.key.to_vec()).collect();

    prop_assert_eq!(
        &actual_keys,
        &expected_keys,
        "I6 violated — final range key set mismatches writer log reconstruction"
    );

    Ok(())
}

/// Largest `value` in `sorted_acks` (sorted by `Instant`) whose
/// `t_ack <= cutoff`. `0` if no ack precedes `cutoff`.
fn committed_prefix_at(sorted_acks: &[AckEntry], cutoff: Instant) -> i64 {
    // Binary search the rightmost index with t_ack <= cutoff.
    let pp = sorted_acks.partition_point(|&(_, t)| t <= cutoff);
    if pp == 0 {
        0
    } else {
        // The max value among `sorted_acks[0..pp]`. For puts
        // this is monotonic in time (writer mutex serializes,
        // rev increases), so the max is at index pp-1. For
        // compacts the same monotonicity holds (writer mutex
        // + idempotent compact rejects rev <= floor with Ok).
        // Belt-and-suspenders: take the max anyway in case a
        // future refactor breaks the monotonic-in-time
        // assumption.
        sorted_acks[..pp]
            .iter()
            .map(|&(rev, _)| rev)
            .max()
            .unwrap_or(0)
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        max_shrink_iters: 256,
        // Project convention (matches mango-storage's
        // btreemap_oracle.rs and differential_vs_bbolt.rs):
        // the shrinker output to stdout suffices for repro.
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn snapshot_consistency(spec in workload_spec()) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("rt");
        // 15s hard timeout per case: the budget is ~200ms,
        // so 15s catches a stuck reader without holding up
        // the harness.
        let result = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(15), run_one_case(spec.clone())).await
        });
        // Bound the runtime drop so a lingering spawn_blocking
        // reader doesn't block the next case.
        rt.shutdown_timeout(Duration::from_secs(2));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(TestCaseError::fail(format!(
                    "case timed out after 15s: {spec:?}"
                )))
            }
        }
    }
}

/// Hand-written smoke case: tiny config, runs first under
/// `cargo test snapshot_consistency_smoke`. Catches harness
/// wiring bugs before the proptest harness burns 32 cases.
#[test]
fn snapshot_consistency_smoke() {
    let spec = WorkloadSpec {
        num_writers: 1,
        num_readers: 1,
        puts_per_writer: 16,
        compact_every: 0,
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async {
        run_one_case(spec).await.expect("smoke case passes");
    });
    rt.shutdown_timeout(Duration::from_secs(2));
}

/// Smoke case with compaction enabled — separate from the no-
/// compact smoke so a regression in I4 surfaces independently
/// of a regression in I3.
#[test]
fn snapshot_consistency_smoke_with_compact() {
    let spec = WorkloadSpec {
        num_writers: 2,
        num_readers: 2,
        puts_per_writer: 32,
        compact_every: 8,
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async {
        run_one_case(spec).await.expect("smoke-with-compact passes");
    });
    rt.shutdown_timeout(Duration::from_secs(2));
}
