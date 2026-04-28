//! `BTreeMap` proptest oracle for `RedbBackend` (ROADMAP:824).
//!
//! Complements the bbolt subprocess harness in
//! `differential_vs_bbolt.rs`. The `BTreeMap` oracle is in-process
//! (no subprocess, no JSON IPC), ~200× cheaper per case than the
//! bbolt harness, and catches **wrapper bugs** at high case rates:
//! key-range conversion errors, batch-state machine off-by-ones,
//! snapshot-iterator bugs that observe uncommitted state, and
//! intra-batch op-ordering bugs.
//!
//! It does NOT model engine-level semantics — fsync, copy-on-write,
//! page layout, freelist, on-disk size — those are the bbolt
//! harness's job. The two harnesses are complementary, not
//! redundant: a wrapper bug that survives 256 bbolt cases will be
//! flushed by 10 000 `BTreeMap` cases; an engine quirk the
//! `BTreeMap` doesn't model is invisible to it.
//!
//! ## Default vs thorough run
//!
//! Default (no env var): **256 cases** — runs in ~20 s on debug
//! builds, gates every PR via the `test` job in
//! `.github/workflows/ci.yml`. Each case opens a fresh `RedbBackend`
//! against a `tempfile::TempDir`, which dominates wall-clock; the
//! per-op cost is negligible. Reusing a backend across cases is
//! deliberately **not** done — a divergence in case N must not be
//! able to pollute case N+1 and shrink to a misleading minimum.
//!
//! Thorough (`MANGO_BTREEMAP_THOROUGH=1`): **10 000 cases** — runs
//! in ~12 min on debug, intended for nightly / on-demand. Same
//! shape as the bbolt harness's `MANGO_DIFFERENTIAL_THOROUGH=1` so
//! contributors only have to learn one knob.
//!
//! ## Op surface
//!
//! `Op::Put / Get / Delete / DeleteRange / RangeScan / Snapshot /
//! Commit`. A single `Vec<Staged>` staging buffer mirrors the
//! `WriteBatch`'s sequential semantics in **generation order** —
//! both the batch playback and the oracle update walk the same
//! ordered list. This is load-bearing: the wrapper's `apply_staged`
//! preserves insertion order, so any sequence-affecting wrapper
//! bug (e.g. delete-then-put silently reordered to put-then-delete)
//! will diverge from the oracle and trip the assertion. Empty
//! commits are intentional — they exercise the no-op commit path
//! that `redb_backend.rs::commit_group_empty_still_increments_stamp`
//! gates.
//!
//! ## Out of scope (vs the bbolt harness)
//!
//! - `CommitGroup` — engine-internal Raft fsync batching primitive.
//!   `commit_batch(b, true)` exercises the same `commit_staged`
//!   path as `commit_group(vec![b])`; the multi-batch flatten is
//!   covered by the bbolt harness's `CommitGroup` op.
//! - `Defragment` — engine-specific.
//! - `CloseReopen` — engine-specific durability check.
//! - Concurrency — `BTreeMap` is `!Sync`; concurrency lives in
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
//!
//! ## Wrapper-API asymmetry exercised here
//!
//! `delete_range`'s `end.is_empty()` means "unbounded upper" (per
//! the engine-neutral contract bbolt established and the wrapper
//! adopted in `db5c76d`). `snapshot.range`'s `end.is_empty()` does
//! **not** carry that meaning — `start..b""` is just an empty range
//! when `start == b""` and an inverted error when `start > b""`. So
//! `Op::DeleteRange` may emit `end = b""`, but `Op::RangeScan`
//! never does. This asymmetry is documented because if the wrapper
//! ever harmonizes the two, this harness must follow.

#![cfg(not(madsim))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
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

/// 16-symbol alphabet (matches the bbolt harness for cross-comparable
/// key-shape coverage; seeds are NOT cross-replayable because the
/// `Op` enums and weights differ — see plan §M2).
///
/// **Invariant.** All bytes < `0xff`. The "snapshot everything"
/// upper bound on lines `Op::Snapshot` and the final implicit
/// snapshot is `b"\xff"`, which is sound only because every key
/// the strategy can produce sorts strictly below that. If the
/// alphabet ever widens to include `0xff`, switch the upper bound
/// to `Bound::Unbounded` (which `snapshot.range` does not currently
/// support — see the Wrapper-API asymmetry note in the module
/// doc).
const ALPHABET: &[u8] = b"0123456789abcdef";

const _ALPHABET_MAX_LT_FF: () = {
    let mut i = 0;
    while i < ALPHABET.len() {
        assert!(ALPHABET[i] < 0xff, "ALPHABET invariant: all bytes < 0xff");
        i += 1;
    }
};

/// Process-global, single-threaded tokio runtime reused across
/// proptest cases.
///
/// **Why `current_thread`:** `commit_batch` calls
/// `tokio::task::spawn_blocking` for the redb fsync, which lands on
/// the runtime's blocking-pool thread regardless of flavor. We
/// don't need worker threads; the proptest macro runs cases
/// sequentially on the test thread, so the cheaper flavor wins.
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
/// **MUTATION TEST INVARIANT.** Validated by `DoD M4`: replacing
/// `table.remove(...)` with a no-op in `apply_staged` is caught by
/// the proptest within seconds and shrinks to a 3-op
/// `[Put, Delete, Commit]` minimum. Do not flatten this distribution
/// without re-running the same mutation experiment — flattening
/// silently regresses the harness's bug-finding power even though
/// every case still passes.
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

/// Two non-empty keys, sorted so `start <= end`.
///
/// Used by `Op::RangeScan`. The inverted-range path is covered by
/// `redb_backend.rs::snapshot_range_invalid_range_errors`; we
/// exercise the well-formed range here.
fn range_pair_strat() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    (key_strat(), key_strat()).prop_map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
}

/// Range pair for `Op::DeleteRange`, where `end` is empty 10 % of
/// the time to exercise the wrapper's "empty end means unbounded
/// upper" branch (`apply_staged` at `redb/mod.rs:302-305`,
/// originally fixed in commit `db5c76d`).
///
/// When `end` is empty, no sorting is applied — the wrapper
/// special-cases that branch entirely. When both are non-empty,
/// they are sorted so `start <= end`.
fn delete_range_pair_strat() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    prop_oneof![
        90 => (key_strat(), key_strat())
            .prop_map(|(a, b)| if a <= b { (a, b) } else { (b, a) }),
        10 => key_strat().prop_map(|start| (start, Vec::new())),
    ]
}

/// Strategy weights:
/// - `Put` 35 — dominates writes; matches the wrapper's hot path.
/// - `Get` 15 — point-lookup coverage of `snapshot.get`.
/// - `Delete` 10 — single-key delete.
/// - `DeleteRange` 10 — half-open interval delete, the most
///   wrapper-heavy mutation (where empty-end / empty-start bugs
///   hide — see `db5c76d`).
/// - `RangeScan` 15 — exercises `snapshot.range` iterator.
/// - `Snapshot` 5 — full state diff vs committed oracle (catches
///   "snapshot accidentally observes uncommitted state" bugs).
/// - `Commit` 10 — flushes the staging buffer, then advances the
///   committed oracle. Empty commits intentional (no-op commit
///   path).
///
/// Weights total to 100 by design — keeps the mental arithmetic
/// trivial when tweaking ratios.
fn op_strat() -> impl Strategy<Value = Op> {
    prop_oneof![
        35 => (key_strat(), val_strat()).prop_map(|(k, v)| Op::Put(k, v)),
        15 => key_strat().prop_map(Op::Get),
        10 => key_strat().prop_map(Op::Delete),
        10 => delete_range_pair_strat().prop_map(|(start, end)| Op::DeleteRange { start, end }),
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

/// One staged op, tagged so the commit handler can replay them in
/// **generation order** against both the batch and the oracle.
///
/// Three parallel by-type vecs would be wrong: the wrapper's
/// `apply_staged` walks the batch in insertion order, so a sequence
/// like `Op::Delete(k) ; Op::Put(k, v) ; Op::Commit` must commit as
/// "delete-then-put" (final state: `{k: v}`). Bucketing by type
/// would force "put-then-delete" (final state: `{}`) and would also
/// make the oracle agree with that wrong ordering, hiding the bug.
#[derive(Debug, Clone)]
enum Staged {
    Put(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
    DelRange { start: Vec<u8>, end: Vec<u8> },
}

/// Apply `Vec<Op>` against a fresh `RedbBackend` and a `BTreeMap`
/// oracle in lockstep, asserting equality at every observation
/// point.
///
/// Returns `TestCaseResult` so the proptest body can use
/// `prop_assert_eq!` for shrinker-friendly diagnostics.
///
/// **Oracle semantics.** The `oracle: BTreeMap<Vec<u8>, Vec<u8>>`
/// reflects the **committed** state only. `Op::Get`,
/// `Op::RangeScan`, and `Op::Snapshot` go through
/// `Backend::snapshot()`, which also returns committed state — so
/// the comparison is apples-to-apples. Do NOT model staged writes
/// in a "shadow" oracle: that would diverge from `snapshot()` and
/// is exactly the wrapper bug we want to find. (The bbolt harness
/// has a TLA+-shaped staged-buffer model for the same reason.)
///
/// **Batch lifecycle.** `RedbBatch` is `!Send` (its internal
/// `WriteTransaction` carries a `PhantomData<*const ()>`); the
/// `commit_batch` prologue at `redb/mod.rs:434-440` consumes it via
/// `into_staged()` synchronously, **before** the returned future is
/// awaited, so the `!Send` batch never enters the future's capture
/// set and `block_on` on a `current_thread` runtime is sound.
fn run_case(ops: &[Op]) -> TestCaseResult {
    let tmp = TempDir::new().expect("tempdir");
    let backend = open_backend(&tmp);
    let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    let mut staged: Vec<Staged> = Vec::new();

    for (i, op) in ops.iter().enumerate() {
        match op {
            Op::Put(k, v) => {
                staged.push(Staged::Put(k.clone(), v.clone()));
            }
            Op::Delete(k) => {
                staged.push(Staged::Del(k.clone()));
            }
            Op::DeleteRange { start, end } => {
                staged.push(Staged::DelRange {
                    start: start.clone(),
                    end: end.clone(),
                });
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
                // no-op commit path that
                // redb_backend.rs::commit_group_empty_still_increments_stamp
                // gates. The `force_fsync = true` argument is currently
                // ignored by the wrapper (see the `let _ = force_fsync`
                // at redb/mod.rs:442); when fsync policy is wired
                // through, this oracle won't observe the change — TODO:
                // add an `Op::CommitNoFsync` variant once the wrapper
                // honors the parameter.
                let mut batch = backend.begin_batch().expect("begin_batch");
                for s in &staged {
                    match s {
                        Staged::Put(k, v) => {
                            batch.put(KV, k, v).expect("batch put");
                        }
                        Staged::Del(k) => {
                            batch.delete(KV, k).expect("batch delete");
                        }
                        Staged::DelRange { start, end } => {
                            batch
                                .delete_range(KV, start, end)
                                .expect("batch delete_range");
                        }
                    }
                }
                let _stamp = runtime()
                    .block_on(backend.commit_batch(batch, true))
                    .expect("commit_batch");

                // Replay the same staged ops against the oracle in the
                // same order — must match the wrapper's insertion-order
                // semantics or B1 (intra-batch reordering bugs) goes
                // undetected.
                for s in staged.drain(..) {
                    match s {
                        Staged::Put(k, v) => {
                            oracle.insert(k, v);
                        }
                        Staged::Del(k) => {
                            oracle.remove(&k);
                        }
                        Staged::DelRange { start, end } => {
                            // Empty `end` means "unbounded upper" per
                            // the wrapper contract (db5c76d). Mirror
                            // that here so the oracle agrees with the
                            // wrapper on this branch.
                            let upper = if end.is_empty() {
                                Bound::Unbounded
                            } else {
                                Bound::Excluded(end.as_slice())
                            };
                            let to_remove: Vec<Vec<u8>> = oracle
                                .range::<[u8], _>((Bound::Included(start.as_slice()), upper))
                                .map(|(k, _)| k.clone())
                                .collect();
                            for k in to_remove {
                                oracle.remove(&k);
                            }
                        }
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

/// Reorder smoke — `Delete(k) ; Put(k, v) ; Commit` must end with
/// `{k: v}`, not `{}`. Catches the B1-class bug where an oracle
/// silently agrees with a wrapper that reorders intra-batch ops by
/// type. If this fails, the staging-buffer ordering is broken.
#[test]
fn smoke_intra_batch_delete_then_put() {
    let ops = vec![
        Op::Put(b"a".to_vec(), b"1".to_vec()),
        Op::Commit,
        // Within one batch: delete, then put. End state must be
        // `{a: 2}` — wrapper applies in order, oracle must too.
        Op::Delete(b"a".to_vec()),
        Op::Put(b"a".to_vec(), b"2".to_vec()),
        Op::Commit,
        Op::Get(b"a".to_vec()),
        Op::Snapshot,
    ];
    if let Err(e) = run_case(&ops) {
        panic!("intra-batch reorder smoke diverged: {e:?}");
    }
}

/// Empty-end `DeleteRange` smoke — `[start, "")` must mean "from
/// start onward" per the wrapper contract from `db5c76d`. Pinned
/// here so the harness can never silently lose this coverage.
#[test]
fn smoke_delete_range_empty_end() {
    let ops = vec![
        Op::Put(b"a".to_vec(), b"1".to_vec()),
        Op::Put(b"b".to_vec(), b"2".to_vec()),
        Op::Put(b"c".to_vec(), b"3".to_vec()),
        Op::Commit,
        Op::DeleteRange {
            start: b"b".to_vec(),
            end: Vec::new(),
        },
        Op::Commit,
        Op::Get(b"a".to_vec()),
        Op::Get(b"b".to_vec()),
        Op::Get(b"c".to_vec()),
        Op::Snapshot,
    ];
    if let Err(e) = run_case(&ops) {
        panic!("empty-end DeleteRange smoke diverged: {e:?}");
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
