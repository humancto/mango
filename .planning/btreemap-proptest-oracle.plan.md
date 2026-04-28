# Plan: BTreeMap proptest oracle (ROADMAP:824)

## Context

ROADMAP:824 reads:

> Property tests: random put/get/delete/range sequences match an in-memory `BTreeMap` oracle (proptest, 10k+ cases)

ROADMAP:819 (just merged at `df0491e`) shipped a proptest harness against
a **bbolt subprocess oracle** with JSON IPC. The bbolt path costs ~10 ms
per case (subprocess startup + JSON encode/decode + Go bbolt fsync), so
256 cases run in ~67 s and 10 000 cases run in hours.

ROADMAP:824 is a **separate, in-process, much faster** harness against
a `BTreeMap<Vec<u8>, Vec<u8>>` oracle. The economics are different:
no subprocess, no JSON, no fsync — just pure-Rust comparison. 10 000
cases is the floor, not the stretch goal; we should comfortably hit
50 000+ in a CI minute.

The two harnesses serve different purposes:

| Harness                                                 | Oracle           | Cost / case | Surface coverage                                          |
| ------------------------------------------------------- | ---------------- | ----------- | --------------------------------------------------------- |
| `tests/differential_vs_bbolt.rs` (ROADMAP:819, shipped) | bbolt subprocess | ~10 ms      | engine-vs-engine: every commit boundary, every range scan |
| `tests/btreemap_oracle.rs` (ROADMAP:824, this PR)       | BTreeMap         | ~50 µs      | wrapper logic vs the trivially-correct in-memory model    |

Why both? The bbolt harness catches **engine asymmetries** (B+-tree vs.
copy-on-write page allocator, fsync semantics, freelist quirks). The
BTreeMap harness catches **wrapper bugs** (key-range conversion errors,
batch-state machine off-by-ones, snapshot iterator bugs) at 200× the
case rate. A wrapper bug that survives 256 bbolt cases will be flushed
by 50k BTreeMap cases — and a engine quirk that the BTreeMap doesn't
model is invisible to it (which is why the bbolt run still has to exist).

Existing partial work: `crates/mango-storage/tests/redb_backend.rs:560`
(`btreemap_oracle_matches_backend_for_mixed_workload`) is a deterministic
200-iter pseudo-random sequence. It's a smoke test, not a property test —
no shrinking, no failure persistence, only one case-shape ever runs. It
stays as the smoke; this PR adds the proper proptest version next to it.

## Scope of this PR

Single integration test file: `crates/mango-storage/tests/btreemap_oracle.rs`.

The file owns:

1. A `Op` enum mirroring the surface of `Backend` we test against an
   in-memory model: `Put`, `Get`, `Delete`, `RangeScan`, `Snapshot`,
   `Commit`, `BeginBatch` (implicit between `Put`/`Delete` runs).
2. A `proptest` strategy that emits `Vec<Op>` sequences with biased
   weights (Put-heavy, Delete and RangeScan moderate, Get and Snapshot
   light — same shape as the bbolt harness for consistency).
3. A `run_case(ops: &[Op])` function that:
   - Opens a fresh `RedbBackend` on a `TempDir`.
   - Maintains a `BTreeMap<Vec<u8>, Vec<u8>>` oracle.
   - For each op: applies to both, asserts equality (or both-error).
4. `proptest_btreemap_oracle_10k_cases` — gated default at **256** cases
   (CI), bumped to **10 000** when `MANGO_BTREEMAP_THOROUGH=1`. The
   `MANGO_BTREEMAP_THOROUGH` knob is the BTreeMap analog of the bbolt
   harness's `MANGO_DIFFERENTIAL_THOROUGH` — same shape so contributors
   only have to learn one pattern.
5. `smoke_btreemap_oracle_short_seq` — a hardcoded 20-op sequence that
   exercises Put/Delete/RangeScan in one fixed shape. Smokes the
   harness wiring without proptest seeding luck.

## Surface NOT in scope

- **`CommitGroup`** — engine-internal Raft fsync batching primitive;
  the BTreeMap oracle has no concept of "group". The bbolt harness is
  the authoritative test for `CommitGroup` semantics.
- **`Defragment`** — engine-specific; BTreeMap can't model on-disk
  layout.
- **`CloseReopen`** — engine-specific durability check; BTreeMap has
  no "reopen" semantics. The redb_backend integration test
  (`reopen_persists_data`, line 245 in redb_backend.rs) covers this.
- **Concurrency** — BTreeMap is not Send+Sync; concurrent access is the
  job of `concurrent_committers_get_distinct_stamps` and the multi-task
  tests in redb_backend.rs.
- **Failure-artifact dumps** — proptest's built-in shrinking + console
  output is sufficient at this scale (the bbolt harness needs artifact
  dumps because subprocess state survives the test process; here it
  doesn't).

## File layout

```
crates/mango-storage/tests/btreemap_oracle.rs           (new, ~400 lines)
```

No new dev-dependencies (proptest is already a dev-dep from PR #54;
serde/base64 not needed because nothing leaves the process).

## Implementation sketch

```rust
//! BTreeMap proptest oracle for `RedbBackend` (ROADMAP:824).
//!
//! Complements the bbolt subprocess harness in
//! `differential_vs_bbolt.rs`. The BTreeMap oracle is in-process
//! (no subprocess, no JSON), 200× cheaper per case, and catches
//! wrapper bugs at high case rates. It does NOT model engine-level
//! semantics like fsync, copy-on-write, or page layout — those
//! belong to the bbolt harness.
//!
//! Default: 256 cases (PR runs). Set `MANGO_BTREEMAP_THOROUGH=1`
//! for the 10 000-case sweep (nightly / on-demand).

use std::collections::BTreeMap;

use mango_storage::{Backend, BucketId, BackendError};
use proptest::prelude::*;
use tempfile::TempDir;

#[derive(Debug, Clone)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Get(Vec<u8>),
    Delete(Vec<u8>),
    RangeScan { start: Vec<u8>, end: Vec<u8> },
    Snapshot,
    Commit,
}

fn op_strat() -> impl Strategy<Value = Op> { /* ... */ }

fn run_case(ops: &[Op]) -> Result<(), String> {
    let tmp = TempDir::new().unwrap();
    let backend = open_backend(&tmp);
    let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut staged_puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut staged_dels: Vec<Vec<u8>> = Vec::new();

    for op in ops {
        match op {
            Op::Put(k, v) => {
                staged_puts.push((k.clone(), v.clone()));
            }
            Op::Delete(k) => {
                staged_dels.push(k.clone());
            }
            Op::Get(k) => {
                let snap = backend.snapshot().map_err(|e| format!("snapshot: {e}"))?;
                let got = snap.get(KV, k).map_err(|e| format!("get: {e}"))?;
                let want = oracle.get(k).cloned();
                if got != want {
                    return Err(format!("Get mismatch on {k:?}: got {got:?}, want {want:?}"));
                }
            }
            Op::RangeScan { start, end } => { /* parallel iteration vs BTreeMap range */ }
            Op::Snapshot => { /* full state diff */ }
            Op::Commit => {
                let mut batch = backend.begin_batch().map_err(|e| format!("begin: {e}"))?;
                for (k, v) in &staged_puts { batch.put(KV, k, v).unwrap(); }
                for k in &staged_dels { batch.delete(KV, k).unwrap(); }
                backend.commit_batch(batch, true).await.map_err(|e| format!("commit: {e}"))?;
                for (k, v) in staged_puts.drain(..) { oracle.insert(k, v); }
                for k in staged_dels.drain(..) { oracle.remove(&k); }
            }
        }
    }
    Ok(())
}

#[test]
fn smoke_btreemap_oracle_short_seq() { /* hardcoded 20-op smoke */ }

proptest! {
    #![proptest_config(ProptestConfig {
        cases: btreemap_cases(),
        failure_persistence: None,  // proptest's shrunk seed is the persistence
        ..ProptestConfig::default()
    })]

    #[test]
    fn proptest_btreemap_oracle(ops in proptest::collection::vec(op_strat(), 1..50)) {
        run_case(&ops).map_err(|e| TestCaseError::fail(e))?;
    }
}

fn btreemap_cases() -> u32 {
    if std::env::var("MANGO_BTREEMAP_THOROUGH").is_ok() { 10_000 } else { 256 }
}
```

(The above is a sketch; the real file will follow the `tokio::test` /
async-commit pattern from `redb_backend.rs` and import the same
`KV: BucketId` constant as the existing tests.)

## Key design questions and answers

### Q1: How to handle `Backend::commit_batch` async in proptest?

`commit_batch` is async (returns `Future`). proptest doesn't natively
support async test bodies. Two options:

(a) **Block on a tokio runtime per case.** `tokio::runtime::Runtime::new()`
inside `run_case` and `block_on` the commit. Per-case overhead is
~1 ms (runtime spawn), which is fine at 10k cases.

(b) **`#[tokio::test]` + proptest macro.** Tokio's test macro doesn't
nest with proptest's macro cleanly; the maintained workaround is
`proptest_runtime` or hand-rolling.

**Decision: (a).** Build the runtime once outside the proptest body,
reuse it across cases. This keeps per-case startup at ~50 µs and
avoids the macro-nesting headache. Pattern matches what `redb_backend.rs`
does for its `#[tokio::test]` cases (one runtime per test fn).

### Q2: Strategy weights — same as the bbolt harness, or recalibrated?

The bbolt harness weights are tuned for divergence-finding against an
engine that has fsync semantics. The BTreeMap oracle has none, so:

- **Put: 40%** (down from 44% in bbolt) — leave room for more Range
- **Get: 15%** (up from 10%) — Get coverage is cheap and catches snapshot bugs
- **Delete: 15%** (same)
- **RangeScan: 15%** (up from 10%) — range scans are wrapper-heavy
- **Snapshot: 5%** (down from 10%) — snapshot is a no-op against BTreeMap state
- **Commit: 10%** (same — every ~10 ops, force a flush)

Total: 100%. Rationale lives in inline doc comments.

### Q3: Should we share `Op` with `differential_vs_bbolt::DiffOp`?

No. The bbolt `DiffOp` carries `serde` derives and base64-encoded
fields for JSON IPC; this file is in-process only. Sharing would
either bloat this file with serde-only fields or require a
re-export from `differential_vs_bbolt.rs` (a test file) into another
test file — proptest strategies aren't meaningfully reusable across
harnesses anyway because the weights are different.

### Q4: Range-scan validation — full diff or sampled?

Full diff. `BTreeMap::range(start..end).collect::<Vec<_>>()` and
compare to `Backend::range(KV, start, end)`'s vec. At case-size 50 ops
the range result is ~100 entries max. Diffing 100 entries 10k times
is microseconds.

### Q5: Empty-key / empty-value handling — symmetric with the bbolt harness?

The bbolt harness exercises `PutNilKey` as a separate op variant with
a normalized symmetric-error contract. Here we don't need that — `Put`
just generates 1..=16-byte keys via the strategy. Empty-key handling
is tested in `redb_backend.rs::put_with_empty_key_returns_other` and
in the bbolt harness; this file stays focused on the well-formed path.

The strategy generates keys/values of length 1..=16 over a 16-symbol
alphabet, identical to the bbolt harness for cross-comparable failure
seeds (a seed that fails one harness should be replayable on the other
modulo serde wrapping).

### Q6: Failure-artifact dump — needed?

No. proptest's built-in shrinking outputs the minimal failing case
to stdout. There's no subprocess state to preserve, and the
`TempDir` is dropped at the end of `run_case`. If a divergence is
found in CI, the GitHub Actions log captures the proptest output;
the maintainer can reproduce locally by pasting the seed into a
hardcoded smoke test.

## Test strategy

- **Local — default sweep:** `cargo nextest run -p mango-storage --test btreemap_oracle`
  runs 256 cases + the smoke. Should complete in < 5 s.
- **Local — thorough sweep:** `MANGO_BTREEMAP_THOROUGH=1 cargo nextest run …`
  runs 10 000 cases. Should complete in < 60 s.
- **CI:** Add to the existing `test` job in `ci.yml`. No new job.
  No env var → 256 cases per PR run, fast.
- **Regression:** the existing `btreemap_oracle_matches_backend_for_mixed_workload`
  in `redb_backend.rs` stays as a deterministic smoke; this file
  adds the proptest version. Both run on every PR.

## Definition of done

- [ ] `crates/mango-storage/tests/btreemap_oracle.rs` exists.
- [ ] `cargo nextest run -p mango-storage --test btreemap_oracle` passes
      locally with 256 cases in < 5 s.
- [ ] `MANGO_BTREEMAP_THOROUGH=1 cargo nextest run …` passes with 10 000
      cases in < 60 s.
- [ ] `cargo clippy -p mango-storage --all-targets -- -D warnings` clean.
- [ ] No new dev-dependencies.
- [ ] rust-expert APPROVE on final diff.
- [ ] ROADMAP.md line 824 flipped to `- [x]` on main.

## Risks specific to this PR

| Risk                                                                                             | Mitigation                                                                                                                                                              |
| ------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Per-case `TempDir` + `Backend::open` overhead exceeds 5 ms, blowing the 60 s thorough budget.    | Pre-warm pattern not needed — ~1 ms per `Backend::open` was measured during PR #54. Budget at 10k × 5 ms = 50 s, well inside CI.                                        |
| `tokio::runtime` reuse across cases fails (runtime not Send across proptest case boundary).      | proptest runs cases serially in one thread; one runtime instantiated outside the macro body, captured by reference via a `OnceLock<Runtime>`. Pattern proven elsewhere. |
| Strategy emits sequences that exhaust the redb file size budget on a `TempDir`.                  | Cap value sizes at 16 bytes and op count at 50 per case; total bytes per case ≤ 800 B, well below any limit.                                                            |
| BTreeMap oracle's "Range" diverges from `Backend::range` semantics (`[low, high)` vs inclusive). | Both are exclusive-end. Asserted in `redb_backend.rs::range_is_half_open`; this file uses the same convention.                                                          |

## Out of scope (explicit non-goals)

- 7-day chaos run (ROADMAP:820) — operational, separate.
- Engine-swap dry-run (ROADMAP:821) — separate test, separate PR.
- Crash-recovery tests (ROADMAP:825/826) — needs panic/EIO injection.
- Bench harness (ROADMAP:828/829) — performance, not correctness.

## Plan v2 — revisions from rust-expert review (APPROVE_WITH_REVISIONS)

Adopted in v2:

- **R2** — soften DoD: 10 000-case thorough run budget is **< 120 s** (not 60 s). Per-case overhead is ~6–15 ms on a slow CI runner, not the optimistic 1 ms.
- **R3** — strategy sorts the two range keys so `start <= end` always; inverted-range error path is covered by `redb_backend.rs::snapshot_range_invalid_range_errors`. Document in inline comment.
- **R4** — `Op::Snapshot` does a **full diff against the committed oracle state** (not just an exercise call). This is the only way to catch a wrapper bug where `snapshot()` accidentally observes uncommitted batch state.
- **M1** — empty commit (no staged puts/dels) is **intentional**: exercises the no-op commit path, mirrors `commit_group_empty_still_increments_stamp`. Note in inline comment.
- **M2** — drop the cross-harness seed-replayability claim from Q5. Different `Op` enums, different serialization, different range strategies → seeds are NOT cross-replayable. Two separate seed spaces is fine; document the divergent goals.
- **S1** — match `redb_backend.rs` blanket `#![allow(clippy::unwrap_used, clippy::expect_used)]` at the file top.
- **S2** — `#![cfg(not(madsim))]`. The harness opens a real `RedbBackend` on a real `TempDir`; under madsim that's a category error.
- **S3** — drop the `BackendError` import; errors stringify via `Display`. Add it back only if a future variant assertion needs it.

Plus runtime tactics from R1 / Q1:

- **`OnceLock<Runtime>`** with **`current_thread` flavor** (`Builder::new_current_thread().enable_all().build()`). Multi-thread is unneeded — `commit_batch` calls `spawn_blocking`, which works on either flavor, and current-thread is cheaper. Leak-on-process-exit is intentional; document so a future contributor doesn't "fix" it with `Drop`.

And inside the proptest body (B1):

- Return `TestCaseResult` from `run_case`, propagate via `prop_assert_eq!` where feasible — better shrinker diagnostics. Reserve raw `String` errors for the `Result<_, BackendError>` round-trip path.

Plus deferred-by-design (M3, M4):

- **M4** — manual verification: deliberately break `RedbBackend::commit_batch` (e.g., off-by-one in delete-range) and confirm proptest shrinks to a minimal failing case in < 30 s. **Adding to DoD.**
- **M3** — `#[ignore]`d "harness self-test" with a deliberately-wrong oracle. **Skipped** as overkill at this scale; manual verification (M4) is sufficient.

## Definition of done (v2)

- [ ] `crates/mango-storage/tests/btreemap_oracle.rs` exists.
- [ ] `cargo nextest run -p mango-storage --test btreemap_oracle` passes
      locally with 256 cases in < 10 s.
- [ ] `MANGO_BTREEMAP_THOROUGH=1 cargo nextest run …` passes with 10 000
      cases in < 120 s.
- [ ] `cargo clippy -p mango-storage --all-targets -- -D warnings` clean.
- [ ] No new dev-dependencies.
- [ ] Manual verification: a deliberate bug in `RedbBackend::commit_batch`
      causes proptest to shrink to a minimal failing case in < 30 s
      (verify locally before merge).
- [ ] rust-expert APPROVE on final diff.
- [ ] ROADMAP.md line 824 flipped to `- [x]` on main.

Ready to implement.
