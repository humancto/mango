//! BTreeMap proptest oracle for `RedbBackend` (ROADMAP:824).
//!
//! Complements the bbolt subprocess harness in
//! `differential_vs_bbolt.rs`. The BTreeMap oracle is in-process
//! (no subprocess, no JSON IPC), ~200× cheaper per case than the
//! bbolt harness, and catches **wrapper bugs** at high case rates:
//! key-range conversion errors, batch-state machine off-by-ones,
//! snapshot-iterator bugs that observe uncommitted state.
//!
//! It does NOT model engine-level semantics — fsync, copy-on-write,
//! page layout, freelist, on-disk size — those are the bbolt
//! harness's job. The two harnesses are complementary, not
//! redundant: a wrapper bug that survives 256 bbolt cases will be
//! flushed by 10 000 BTreeMap cases; an engine quirk the BTreeMap
//! doesn't model is invisible to it.
//!
//! ## Default vs thorough run
//!
//! Default (no env var): **256 cases** — runs in ~20 s on debug
//! builds, gates every PR via the `test` job in
//! `.github/workflows/ci.yml`. Each case opens a fresh `RedbBackend`
//! against a `tempfile::TempDir`, which dominates wall-clock; the
//! per-op cost is negligible.
//!
//! Thorough (`MANGO_BTREEMAP_THOROUGH=1`): **10 000 cases** — runs in
//! ~12 min on debug, intended for nightly / on-demand. Same shape as
//! the bbolt harness's `MANGO_DIFFERENTIAL_THOROUGH=1` so contributors
//! only have to learn one knob.
//!
//! ## Op surface
//!
//! `Op::Put / Get / Delete / DeleteRange / RangeScan / Snapshot /
//! Commit`. The single staging buffer (`staged_puts`, `staged_dels`,
//! `staged_range_dels`) mirrors `WriteBatch`'s sequential semantics
//! and flushes on every `Op::Commit`. Empty commits are intentional
//! — they exercise the no-op commit path that
//! `redb_backend.rs::commit_group_empty_still_increments_stamp`
//! gates.
//!
//! ## Out of scope (vs the bbolt harness)
//!
//! - `CommitGroup` — engine-internal Raft fsync batching primitive;
//!   BTreeMap has no group concept.
//! - `Defragment` — engine-specific.
//! - `CloseReopen` — engine-specific durability check; BTreeMap has
//!   no reopen semantics.
//! - Concurrency — BTreeMap is `!Sync`; concurrency lives in
//!   `redb_backend.rs::concurrent_committers_get_distinct_stamps`.
//!
//! ## Excluded by design
//!
//! - Empty key (`Op::Put(b"", _)` / `Op::Get(b"")`) — covered by
//!   `redb_backend.rs::put_with_empty_key_returns_other` and the
//!   bbolt harness's `PutNilKey` op. Strategy generates 1..=16-byte
//!   keys over a 16-symbol alphabet.
//! - Inverted range (`start > end`) — covered by
//!   `redb_backend.rs::snapshot_range_invalid_range_errors`. Range
//!   strategy sorts the two keys so `start <= end` always.

#![cfg(not(madsim))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::OnceLock;

use mango_storage::{Backend, BackendConfig, BucketId, ReadSnapshot, RedbBackend, WriteBatch};
use proptest::prelude::*;
use proptest::test_runner::TestCaseResult;
use tempfile::TempDir;
use tokio::runtime::Runtime;

const KV: BucketId = BucketId::new(1);

// 16-symbol alphabet (matches the bbolt harness for cross-comparable
// key-shape coverage; seeds are NOT cross-replayable because the
// `Op` enums and weights differ — see plan §M2).
const ALPHABET: &[u8] = b"0123456789abcdef";

/// Process-global, single-threaded tokio runtime reused across
/// proptest cases.
///
/// **Why `current_thread`:** `commit_batch` calls
/// `tokio::task::spawn_blocking` for the redb fsync, which works on
/// either flavor. We don't need worker threads (proptest serializes
/// cases anyway), so the cheaper flavor wins.
///
/// **Why a leak-on-process-exit `OnceLock` instead of `Lazy<Runtime>`
/// with `Drop`:** dropping a runtime synchronously waits for tasks
/// and cannot run from inside one of its own worker threads. Leaking
/// it until process exit is intentional — the OS reclaims everything
/// on test-binary exit. Do not "fix" this with `Drop`; doing so
/// reintroduces a teardown hazard.
fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread tokio runtime")
    })
}

/// One operation in a generated sequence.
///
/// Each variant carries its own arguments — no shared "current key"
/// state. Keys/values are always 1..=16 bytes; range bounds are
/// pre-sorted so `start <= end` (see `range_pair_strat`).
#[derive(Debug, Clone)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Get(Vec<u8>),
    Delete(Vec<u8>),
    DeleteRange { start: Vec<u8>, end: Vec<u8> },
    RangeScan { start: Vec<u8>, end: Vec<u8> },
    Snapshot,
    Commit,
}

/// Key strategy, skewed toward short keys.
///
/// **Why skewed:** the harness's bug-finding power on point ops
/// (`Delete`, `Get`) depends on collisions between an `Op::Put(k, _)`
/// and a later `Op::Delete(k)` or `Op::Get(k)` for **the same `k`**.
/// A flat 1..=16-byte distribution over a 16-symbol alphabet makes
/// such collisions astronomically unlikely (≈ 2⁻³² per pair on
/// average), so a bug like "`Delete` is a no-op" survives 256 cases
/// with probability indistinguishable from 1. Skewing 60 % to length
/// 1 (16-key universe) and 25 % to length 2 (256-key universe) gives
/// virtually guaranteed collisions inside a length-50 op sequence
/// without sacrificing the long-key coverage that range ops need.
///
/// Validated by DoD M4: replacing `table.remove(...)` with a no-op
/// in `apply_staged` is now caught by the proptest within seconds.
fn key_strat() -> impl Strategy<Value = Vec<u8>> {
    let alpha = (0u8..ALPHABET.len() as u8).prop_map(|i| ALPHABET[i as usize]);
    prop_oneof![
        60 => proptest::collection::vec(alpha.clone(), 1..=1),
        25 => proptest::collection::vec(alpha.clone(), 2..=2),
        15 => proptest::collection::vec(alpha, 3..=16),
    ]
}

fn val_strat() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 1..=16)
}

/// Two keys, sorted so `start <= end`. The inverted-range path is
/// covered by `redb_backend.rs::snapshot_range_invalid_range_errors`;
/// we exercise the well-formed range here.
fn range_pair_strat() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    (key_strat(), key_strat()).prop_map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
}

/// Strategy weights:
/// - Put 35 — dominates writes; matches the wrapper's hot path.
/// - Get 15 — point-lookup coverage of `snapshot.get`.
/// - Delete 10 — single-key delete.
/// - DeleteRange 10 — half-open interval delete, the most
///   wrapper-heavy mutation (where empty-end / empty-start bugs hide
///   — see `db5c76d`).
/// - RangeScan 15 — exercises `snapshot.range` iterator.
/// - Snapshot 5 — full state diff vs committed oracle (catches
///   "snapshot accidentally observes uncommitted state" bugs).
/// - Commit 10 — flushes the staging buffer, then advances the
///   committed oracle. Empty commits intentional (no-op commit path).
fn op_strat() -> impl Strategy<Value = Op> {
    prop_oneof![
        35 => (key_strat(), val_strat()).prop_map(|(k, v)| Op::Put(k, v)),
        15 => key_strat().prop_map(Op::Get),
        10 => key_strat().prop_map(Op::Delete),
        10 => range_pair_strat().prop_map(|(start, end)| Op::DeleteRange { start, end }),
        15 => range_pair_strat().prop_map(|(start, end)| Op::RangeScan { start, end }),
        5 => Just(Op::Snapshot),
        10 => Just(Op::Commit),
    ]
}

fn ops_strat() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(op_strat(), 1..=50)
}

fn open_backend(dir: &TempDir) -> RedbBackend {
    let backend = RedbBackend::open(BackendConfig::new(dir.path().to_path_buf(), false))
        .expect("open RedbBackend");
    backend.register_bucket("kv", KV).expect("register kv");
    backend
}

/// Apply `Vec<Op>` against a fresh `RedbBackend` and a `BTreeMap`
/// oracle in lockstep, asserting equality at every observation
/// point.
///
/// Returns `TestCaseResult` so the proptest body can use
/// `prop_assert_eq!` for shrinker-friendly diagnostics. Reserved
/// `String` errors are for `BackendError` round-trips that don't
/// fit `prop_assert_eq!`'s value-pair shape.
///
/// **Oracle semantics.** The `oracle: BTreeMap<Vec<u8>, Vec<u8>>`
/// reflects the **committed** state only. `Op::Get`, `Op::RangeScan`,
/// and `Op::Snapshot` go through `Backend::snapshot()`, which also
/// returns committed state — so the comparison is apples-to-apples.
/// Do NOT model staged writes in a "shadow" oracle: that would
/// diverge from `snapshot()` and is exactly the wrapper bug we want
/// to find. (The bbolt harness has a TLA+-shaped staged-buffer
/// model for the same reason.)
///
/// **Batch lifecycle.** `RedbBatch` is `!Send` (its internal
/// `WriteTransaction` carries a `PhantomData<*const ()>`); we hold
/// it across no `.await` boundaries here, so `block_on` is sound.
fn run_case(ops: &[Op]) -> TestCaseResult {
    let tmp = TempDir::new().expect("tempdir");
    let backend = open_backend(&tmp);
    let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    let mut staged_puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut staged_dels: Vec<Vec<u8>> = Vec::new();
    let mut staged_range_dels: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

    for (i, op) in ops.iter().enumerate() {
        match op {
            Op::Put(k, v) => {
                staged_puts.push((k.clone(), v.clone()));
            }
            Op::Delete(k) => {
                staged_dels.push(k.clone());
            }
            Op::DeleteRange { start, end } => {
                staged_range_dels.push((start.clone(), end.clone()));
            }
            Op::Get(k) => {
                let snap = backend.snapshot().expect("snapshot");
                let got = snap.get(KV, k).expect("get").map(|b| b.to_vec());
                let want = oracle.get(k).cloned();
                prop_assert_eq!(
                    &got,
                    &want,
                    "op #{}: Get({:?}) — backend={:?}, oracle={:?}",
                    i,
                    k,
                    got,
                    want
                );
            }
            Op::RangeScan { start, end } => {
                let snap = backend.snapshot().expect("snapshot");
                let mut got: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                let iter = snap.range(KV, start, end).expect("range");
                for entry in iter {
                    let (k, v) = entry.expect("range item");
                    got.push((k.to_vec(), v.to_vec()));
                }
                let want: Vec<(Vec<u8>, Vec<u8>)> = oracle
                    .range::<[u8], _>((
                        Bound::Included(start.as_slice()),
                        Bound::Excluded(end.as_slice()),
                    ))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                prop_assert_eq!(
                    &got,
                    &want,
                    "op #{}: RangeScan([{:?}, {:?})) — backend len={}, oracle len={}",
                    i,
                    start,
                    end,
                    got.len(),
                    want.len()
                );
            }
            Op::Snapshot => {
                // Full diff: both sides should report the same set of
                // committed keys. Catches "snapshot leaks uncommitted
                // batch state" bugs that point Get / partial Range
                // would miss.
                let snap = backend.snapshot().expect("snapshot");
                let mut got: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                let iter = snap.range(KV, b"", b"\xff").expect("full range");
                for entry in iter {
                    let (k, v) = entry.expect("range item");
                    got.push((k.to_vec(), v.to_vec()));
                }
                let want: Vec<(Vec<u8>, Vec<u8>)> =
                    oracle.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                prop_assert_eq!(
                    &got,
                    &want,
                    "op #{}: Snapshot — backend len={}, oracle len={}",
                    i,
                    got.len(),
                    want.len()
                );
            }
            Op::Commit => {
                // Empty commits are intentional — they exercise the
                // no-op commit path. Mirror's
                // redb_backend.rs::commit_group_empty_still_increments_stamp.
                let mut batch = backend.begin_batch().expect("begin_batch");
                for (k, v) in &staged_puts {
                    batch.put(KV, k, v).expect("batch put");
                }
                for k in &staged_dels {
                    batch.delete(KV, k).expect("batch delete");
                }
                for (start, end) in &staged_range_dels {
                    batch
                        .delete_range(KV, start, end)
                        .expect("batch delete_range");
                }
                let _stamp = runtime()
                    .block_on(backend.commit_batch(batch, true))
                    .expect("commit_batch");

                for (k, v) in staged_puts.drain(..) {
                    oracle.insert(k, v);
                }
                for k in staged_dels.drain(..) {
                    oracle.remove(&k);
                }
                for (start, end) in staged_range_dels.drain(..) {
                    let to_remove: Vec<Vec<u8>> = oracle
                        .range::<[u8], _>((
                            Bound::Included(start.as_slice()),
                            Bound::Excluded(end.as_slice()),
                        ))
                        .map(|(k, _)| k.clone())
                        .collect();
                    for k in to_remove {
                        oracle.remove(&k);
                    }
                }
            }
        }
    }

    // Final implicit Snapshot — guards against "everything passed
    // mid-stream but the final committed state diverges" cases. No
    // explicit Op::Commit at end is required; we compare what's
    // actually committed.
    let snap = backend.snapshot().expect("final snapshot");
    let mut got: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let iter = snap.range(KV, b"", b"\xff").expect("final full range");
    for entry in iter {
        let (k, v) = entry.expect("final range item");
        got.push((k.to_vec(), v.to_vec()));
    }
    let want: Vec<(Vec<u8>, Vec<u8>)> =
        oracle.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    prop_assert_eq!(
        &got,
        &want,
        "final state mismatch — backend len={}, oracle len={}",
        got.len(),
        want.len()
    );

    Ok(())
}

// ---------- smoke ----------------------------------------------------

/// Hardcoded short sequence — smokes the harness wiring without
/// proptest seeding luck. If this fails, the proptest config is
/// almost certainly broken (env-var parsing, runtime init, bucket
/// registration), not the engine.
#[test]
fn smoke_btreemap_oracle_short_seq() {
    let ops = vec![
        Op::Put(b"a".to_vec(), b"1".to_vec()),
        Op::Put(b"b".to_vec(), b"2".to_vec()),
        Op::Commit,
        Op::Get(b"a".to_vec()),
        Op::Get(b"b".to_vec()),
        Op::Get(b"missing".to_vec()),
        Op::Snapshot,
        Op::Put(b"c".to_vec(), b"3".to_vec()),
        Op::Delete(b"a".to_vec()),
        Op::Commit,
        Op::Snapshot,
        Op::DeleteRange {
            start: b"b".to_vec(),
            end: b"d".to_vec(),
        },
        Op::Commit,
        Op::RangeScan {
            start: b"".to_vec(),
            end: b"\xff".to_vec(),
        },
        Op::Snapshot,
    ];
    if let Err(e) = run_case(&ops) {
        panic!("smoke sequence diverged: {e:?}");
    }
}

// ---------- proptest -------------------------------------------------

fn case_count() -> u32 {
    if std::env::var("MANGO_BTREEMAP_THOROUGH").is_ok() {
        10_000
    } else {
        256
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: case_count(),
        // Persistence-on-disk is unnecessary: this harness is in-process,
        // there is no subprocess state to preserve, and proptest's
        // built-in shrinking output (printed to stdout on failure) is the
        // sole reproduction artifact. The bbolt harness disables
        // persistence for a different reason (failure-artifact dir is the
        // persistence); here it's because the shrinker output suffices.
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// 256 cases by default (CI); 10 000 when
    /// `MANGO_BTREEMAP_THOROUGH=1`.
    #[test]
    fn proptest_btreemap_oracle(ops in ops_strat()) {
        run_case(&ops)?;
    }
}
