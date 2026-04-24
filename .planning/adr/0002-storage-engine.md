# ADR 0002: Storage engine — redb (KV) + tikv/raft-engine (Raft log) behind a Backend trait pair

## Status

**Accepted** — 2026-04-24.

Supersedes: none.
Superseded by: none.

Deciders: Archith (maintainer), `rust-expert` (adversarial review, post-verification 2026-04-24).

**Gate:** every factual claim below either cites `.planning/adr/0002-storage-engine.verification.md` (primary-source verified 2026-04-24) or `ROADMAP.md`. Unverified numbers are called out as TBD and scheduled for Phase 1 measurement.

## Context

### Mango's workload profile

mango is a Rust port of etcd. Its target workload is therefore etcd's workload, verified against etcd's own documentation (verification H1–H4):

- **Dataset size** (verification H1): etcd hardware tiers are Small ≤ 100 MB, Medium ≤ 500 MB, Large ≤ 1 GB. Practical deployments set `--quota-backend-bytes` in the 2–8 GiB range. Target: **GB-scale, not TB-scale.**
- **Write throughput** (verification H2): 583 QPS single-connection, 44,341–50,104 QPS at 100-conn/1000-client heavy load. Writes are serialized through Raft apply; the ceiling is fsync physics, not engine choice.
- **MVCC read path** (verification H3): persistent B+tree (= bbolt) keyed by `(major revision, sub-id, type)`, plus an in-memory B-tree index mapping user keys → revisions. Read path: in-memory key-index lookup → bbolt read for the value.
- **Fsync batching** (verification H4): etcd batches raft proposals into single commits — the storage engine must support group-commit semantics.

One-line summary: **small keys, small values, GB-scale, read-dominated, single-writer serialized through Raft, fsync-batched.** That profile is a **B-tree workload**, not an LSM workload.

### What etcd runs today

etcd's KV backend is bbolt (`go.etcd.io/bbolt`, verification F1). bbolt is a pure-Go fork of Bolt: B+tree, mmap-backed, single-writer MVCC, copy-on-write pages (verification F3). etcd does not publish bbolt-isolated benchmarks (verification F2); the 44–50k QPS numbers include the full Raft + storage + gRPC path.

**We must beat bbolt on at least one of (write throughput, read p99, on-disk size, fsync amplification) without losing on the others** (ROADMAP.md:813, :822). bbolt at a pinned version is the reference oracle in `benches/oracles/bbolt/`.

### Constraints

1. **Team shape.** Solo + AI agents. Cannot afford to maintain a bespoke storage engine. Strong bias: _take the dep, don't hand-roll_.
2. **Workspace policy.** `unsafe_code = "forbid"` in mango crates; transitive-dep unsafe is tracked via `cargo-geiger` baseline (Phase 0.5, task #19). Dependencies are audited via `cargo-vet` (Phase 0, task #21).
3. **Open-source → traction → monetize.** Apache 2.0 positioning (separate LICENSE PR). The commercial story does not require a hand-rolled engine; it requires a correct, fast, operable one.
4. **Crash-only design** (ROADMAP.md Phase 0 crash-only declaration, task #10): no graceful-shutdown path required; every restart is a crash recovery. The engine must be correct under SIGKILL at arbitrary write points.
5. **Multi-raft forward compatibility** (ROADMAP Phase 10+): the storage boundary must support one-backend-per-group OR one-backend-with-namespacing.

### Decision axes (from the 10 north-star axes in ROADMAP.md)

For this ADR, the relevant axes are:

- **Performance** — must beat bbolt on at least one metric at Phase 1 acceptance.
- **Concurrency** — single-writer engines are correct for a Raft-serialized workload.
- **Reliability** — crash recovery must be bounded and testable; no silent data loss.
- **Correctness** — differential-test harness against bbolt must show no non-bbolt-quirk divergence.
- **Safety** — minimize `unsafe` transitive footprint; `cargo-geiger` baseline-pinned.
- **Storage-efficiency** — on-disk size ≤ 0.7× etcd's on the standard workload (ROADMAP.md:821).
- **Operability** — debuggable, portable (no fragile mmap/fork(2) interactions), no C++ blast surface.

## Decision

1. **KV backend:** `redb` 4.1.0+ (crates.io, verification A2). Accessed through the `Backend` trait in `crates/mango-storage`.
2. **Raft log engine:** `tikv/raft-engine`, git-pinned to a specific master SHA via the workspace `Cargo.toml`. Accessed through the `RaftLogStore` trait in `crates/mango-storage`.
3. **Compression:** raft-engine's built-in `lz4-sys` (C FFI) compression is **disabled**. Compression, if any, is done above raft-engine in `mango-raft` using `lz4_flex` (pure Rust). This keeps the inventory row (ROADMAP.md:485) honest.
4. **Trait boundary:** all engine-specific types are hidden behind associated types. No `redb::Database` or `raft_engine::Engine` types leak above `mango-storage`. See §6.

## Considered alternatives

Each alternative is engaged with verified facts. "Why not X" answers are the decision-criteria mapping, not taste.

### Alternative B: `heed` + LMDB (runner-up, kept as escape hatch)

- **Facts** (verification E1–E5): heed 0.22.1 (2026-04-07), maintained by Meilisearch, 3.25M all-time / 845k 90-day downloads. LMDB upstream still maintained (commits 2026-01-13). One `unsafe` at `EnvOpenOptions::open` (documented LMDB semantics). Real production user: Meilisearch itself.
- **Why not now:** adds a C dep (LMDB). mmap + fork(2) interactions are a known operability footgun; snapshots via filesystem copy are tricky. 2 GiB default mapsize on 32-bit (mango is 64-bit-only so this is cosmetic).
- **Why runner-up, not rejected:** heed has a real marquee production user (Meilisearch) — stronger than redb's production evidence (verification A6 — no README-listed users; Cuprate is the best visible consumer). If Phase 1 differential testing shows redb diverging from bbolt in ways we can't root-cause, we swap to heed. The `Backend` trait (§6) makes this mechanical.

### Alternative C: `fjall` (LSM)

- **Facts** (verification D1–D6): fjall 3.1.4 (2026-04-14). Pure Rust, 100% safe-stable Rust claim in README. 823k all-time / 544k 90-day downloads. ~2.3 years old. No marquee production users.
- **Why not:** workload mismatch. fjall is an LSM tree — optimized for write-heavy workloads with large range scans. etcd's workload is read-dominated, GB-scale, small values. Using fjall pays LSM's compaction tax for a workload that doesn't need the write-amplification tradeoff.
- **Future use:** if mango grows a tiered-storage feature post-v1.0 (hot on B-tree, cold on LSM), fjall is the candidate for the cold tier.

### Alternative D: `sled`

- **Facts** (verification C1–C5): maintainer's own README: "sled is beta… if reliability is your primary constraint, use SQLite." Last stable = 0.34.7 (2022). 1.0-alpha.124 (2024-10) pre-release. Rewrite is happening as `komora/marble`.
- **Why not:** hard disqualification from the maintainer.

### Alternative E: `rust-rocksdb`

- **Facts:** RocksDB is the battle-tested LSM. Inventory mentions it (ROADMAP.md:462) as a documented alternative for write-heavy LSM workloads.
- **Why not:** C++ blast surface is large; supply-chain footprint (bzip2, snappy, lz4, zstd, zlib all via C) is large; building on every platform is friction. TiKV accepts this cost because their workload justifies it; mango's workload doesn't.

### Alternative F: Hand-rolled engine (B+tree + WAL, or `BTreeMap` + WAL + snapshot)

- **Why not:** solo + AI team. ROADMAP crate inventory (ROADMAP.md:462) says "Hand-roll requires an ADR." This ADR declines to hand-roll because durable B-tree + MVCC + crash recovery + snapshot isolation + defrag is person-decades of bug-fixing, and we have no engineering capacity to own it indefinitely.

### Alternative G: Candidates considered and not viable

- **SpacetimeDB's storage:** not packaged as a reusable crate.
- **DataFusion's columnar primitives:** wrong shape (OLAP, not OLTP-KV).
- **sled's rewrite (`komora/marble`):** pre-release, hand-roll-equivalent for our purposes.

## Warts and mitigations

Every verified wart is engaged explicitly. "This is fine" is not an answer.

### W1 — redb has no public marquee production user (verification A6)

- **How bad:** moderate. redb is stable (post-1.0 since 2023-06-16, verification A5), actively maintained (master commit 2026-04-23, verification A4), with 1.8M 90-day downloads (verification A10). But "downloads" is not "a load-bearing system's postmortems." heed has Meilisearch (verification E5); redb has Cuprate.
- **MSRV:** redb 4.x declares `edition = "2024"` with `rust-version` above the workspace floor at the time this ADR was written. Resolved in [ADR 0003](0003-msrv-bump.md) by bumping workspace MSRV 1.80 → 1.89.
- **Mitigation:** Phase 1 acceptance requires _three_ additional gates beyond the bbolt bench comparison (ROADMAP.md:822):
  1. **Differential test harness** `tests/differential/backend_vs_bbolt.rs`: proptest-generated 10k-op sequences run against mango's `Backend` and against the `benches/oracles/bbolt/` Go binary via JSON IPC; identical visible state after every commit.
  2. **7-day sustained chaos gate** `tests/chaos/backend_7day.rs`: `benches/workloads/storage.toml` workload under disk-EIO + SIGKILL injection for 7 continuous wall-clock days on the `benches/runner/HARDWARE.md` sig.
  3. **Miri on the wrapper layer:** we can't run Miri on redb's 37-unsafe-token internals cheaply, but we can and must run Miri on `mango-storage`'s redb adapter.
- **Escape hatch:** differential-test divergence that is not a documented bbolt quirk and that we cannot root-cause to a wrapper bug → swap to heed in Phase 1 itself, not Phase 14. Two weeks of work via the `Backend` trait.

### W2 — redb has 37 `unsafe` tokens in v4.1.0 src/ and no `#![forbid(unsafe_code)]` (verification A7)

- **How bad:** acceptable and already budgeted. Workspace `unsafe_code = "forbid"` is for mango crates; transitive-dep unsafe is tracked by the Phase 0.5 `cargo-geiger` baseline (task #19). 37 tokens in a storage engine is low — redb is mmap-free (verification A1) so the unsafe is page-store buffer arithmetic (locally auditable), not mmap projection.
- **Mitigation:**
  1. Pin `cargo-geiger` baseline. A redb release that pushes the count +10 above 4.1.0's 37 trips CI and requires an ADR refresh before Cargo.toml takes the bump.
  2. `mango-storage` itself stays `forbid(unsafe_code)`. All redb interaction is through redb's safe surface.
- **Escape hatch:** if a redb unsafe block is implicated in a reproducible issue and upstream is slow, file a PR upstream (cberner shipped 14 releases in 12 months, verification A3 — responsive) or pin a patched fork. `Backend` trait isolates the damage.

### W3 — redb MVCC is single-writer / multi-reader (verification A9, A1 correction)

- **How bad:** structurally correct, not a wart. mango is single-writer-by-Raft; redb is single-writer-MVCC. bbolt is single-writer-MVCC. The shapes match etcd's workload by construction.
- **Mitigation:** document this explicitly in the rustdoc so no caller expects RocksDB-style concurrent writers. A1's "B+tree" terminology in the original analysis was wrong — redb is a **copy-on-write B-tree** per its own design doc. Use correct terminology in the ADR and docs.
- **Escape hatch:** none needed.

### W4 — raft-engine is 24 months stale on crates.io (verification B2, B3, B8)

- **How bad:** moderate. Current crate is 0.4.2 (2024-04-26). Master is active through 2026-03-10. PingCAP ships fixes into TiKV via master, not crates.io releases. No semver/stability policy documented.
- **MSRV:** raft-engine master declares `rust-version = "1.85"`, above the workspace floor at the time this ADR was written. Resolved in [ADR 0003](0003-msrv-bump.md) by bumping workspace MSRV 1.80 → 1.89.
- **Mitigation:** git-pin to a specific master SHA in the workspace `Cargo.toml`:
  ```toml
  [workspace.dependencies]
  raft-engine = { git = "https://github.com/tikv/raft-engine", rev = "<sha>" }
  ```
  Plus:
  1. **cargo-vet audit** at the pinned SHA (Phase 0 gate, task #21, already shipped).
  2. **Renovate tracking** of the master branch (Phase 0.5 gate, task #28, already shipped). Bumps open a PR a human reviews.
  3. **Upstream engagement:** issue filed at [tikv/raft-engine#396](https://github.com/tikv/raft-engine/issues/396) asking about 1.0 plans, semver commitment, and crates.io publication cadence. Response (when received) is summarized in this §W4.
  4. **Re-evaluation cadence:** re-check raft-engine's crates.io / master situation at every mango minor release.
- **Escape hatch:** if master goes dark for 12 months or a CVE is filed with no response in 30 days, swap the Raft log engine. Two fallbacks: (a) redb-backed log engine built behind `RaftLogStore` (not ideal for write amplification; acceptable as a temporary fallback); (b) vendor the pinned SHA under `crates/vendored-raft-engine/` with a CODEOWNERS stanza. Vendoring is a multi-year burden; taken only if upstream is truly gone.

### W5 — raft-engine pulls `lz4-sys` C FFI by default (verification B6)

- **How bad:** inconsistent with ROADMAP.md:485 which specifies pure-Rust `lz4_flex` as the workspace default.
- **Mitigation:** **disable raft-engine's compression** in its config and do compression above raft-engine in `mango-raft` using `lz4_flex`. Net effects:
  - Zero new C FFI.
  - Compression policy moves to the mango layer (where we want it for tuning).
  - "Pure Rust" commercial positioning preserved.
- **Escape hatch:** if above-the-engine compression loses meaningfully to raft-engine's per-batch internal compression (measure in Phase 5), revisit with benchmarks. ~1-day swap if we accept the C dep.

### W6 — raft-engine has 49 `unsafe` tokens on master (verification B7)

- **How bad:** acceptable. Higher than redb's 37, but raft-engine does mmap + manual buffer arithmetic (log-structured append via `memmap2`). PingCAP runs raft-engine at multi-PB scale in TiKV (verification G1, G3) — those unsafe blocks have survived more production stress than any test we could write.
- **Mitigation:** same as W2. Baseline pinned; `mango-raft` itself stays `forbid(unsafe_code)`; all raft-engine interaction is through safe APIs.
- **Escape hatch:** same as W2.

### W7 — raft-engine is pre-1.0 (verification B8)

- **How bad:** medium. Upstream reserves the right to break API. A git-pin to a SHA defangs this — at a specific commit, the API shape is fixed until we bump.
- **Mitigation:**
  1. Git-pin per W4.
  2. Integration tests re-run on every bump; surface breaks fail CI.
  3. Contribution-ready: if we need a raft-engine API change, upstream via PR before vendoring.
- **Escape hatch:** W4's escape hatch applies.

### W8 — no published recovery-time SLO per GB for raft-engine (verification B5)

- **How bad:** an information gap, not a defect. The ADR cannot cite a recovery-time number.
- **Mitigation:** measure recovery time ourselves in the Phase 1 crash-recovery test (ROADMAP.md:818, :819) and set the bar from observation. Current budget: **recovery time ≤ 30 seconds at 8 GiB of data after unclean shutdown** (~4× etcd's practical recovery-time expectation).
- **Escape hatch:** if recovery exceeds 30s at 8 GiB in Phase 1, swap engines per §5.

## Backend + RaftLogStore trait design

Two traits. `Backend` is the user-data KV. `RaftLogStore` is the Raft log. Separate because they have different hot paths (log = append + truncate; KV = range + point + batch commit) and potentially different engines.

The full trait sketch (associated types, async-vs-sync decisions, justifications per method) is published in this ADR as reference; the committed trait lives in `crates/mango-storage/src/backend.rs` in the next PR. Method-level signatures below are the contract this ADR freezes.

```rust
// crates/mango-storage/src/backend.rs

/// Identifies a namespaced keyspace inside a Backend.
/// Maps to: redb Table, bbolt bucket, heed Database, LMDB named sub-db.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BucketId(pub u16);

/// Snapshot of the backend at a point in time. Must outlive any reader
/// holding it. Cheap to clone.
pub trait ReadSnapshot: Send + Sync {
    fn get(&self, bucket: BucketId, key: &[u8]) -> Result<Option<Bytes>, BackendError>;

    /// Forward range iterator. Half-open [start, end). GAT-lending so the
    /// iterator borrows from the snapshot; no heap allocation per-item.
    fn range<'a>(
        &'a self,
        bucket: BucketId,
        start: &'a [u8],
        end: &'a [u8],
    ) -> Result<Box<dyn RangeIter<'a> + 'a>, BackendError>;
}

pub trait RangeIter<'a>:
    Iterator<Item = Result<(Bytes, Bytes), BackendError>> + Send
{
}

/// Write batch. Builder-style so multiple ops coalesce into one commit group.
/// Not Send — a write batch is thread-local by construction (single writer).
pub trait WriteBatch {
    fn put(&mut self, bucket: BucketId, key: &[u8], value: &[u8]) -> Result<(), BackendError>;
    fn delete(&mut self, bucket: BucketId, key: &[u8]) -> Result<(), BackendError>;
    fn delete_range(&mut self, bucket: BucketId, start: &[u8], end: &[u8])
        -> Result<(), BackendError>;
}

/// The storage backend. Single-writer-multi-reader MVCC. Matches etcd's
/// batch-tx-over-bbolt semantics.
pub trait Backend: Send + Sync + 'static {
    type Snapshot: ReadSnapshot + 'static;
    type Batch: WriteBatch;

    fn register_bucket(&self, name: &str, id: BucketId) -> Result<(), BackendError>;
    fn snapshot(&self) -> Result<Self::Snapshot, BackendError>;
    fn begin_batch(&self) -> Result<Self::Batch, BackendError>;

    async fn commit_batch(
        &self,
        batch: Self::Batch,
        force_fsync: bool,
    ) -> Result<CommitStamp, BackendError>;

    /// Group-commit: atomically commits multiple batches with a single fsync.
    /// Critical for Raft fsync batching (verification H4).
    async fn commit_group(
        &self,
        batches: Vec<Self::Batch>,
    ) -> Result<CommitStamp, BackendError>;

    fn open(config: BackendConfig) -> Result<Self, BackendError> where Self: Sized;
    fn close(self) -> Result<(), BackendError>;
    fn size_on_disk(&self) -> Result<u64, BackendError>;
    async fn defragment(&self) -> Result<(), BackendError>;
}

/// Raft log storage. Separate from Backend because append-only semantics
/// and truncation patterns are engine-specific.
pub trait RaftLogStore: Send + Sync + 'static {
    async fn append(&self, entries: &[RaftEntry]) -> Result<(), BackendError>;
    fn entries(&self, low: u64, high: u64) -> Result<Vec<RaftEntry>, BackendError>;
    fn last_index(&self) -> Result<u64, BackendError>;
    fn first_index(&self) -> Result<u64, BackendError>;
    async fn compact(&self, idx: u64) -> Result<(), BackendError>;
    async fn install_snapshot(&self, snapshot: &RaftSnapshotMetadata) -> Result<(), BackendError>;
    async fn save_hard_state(&self, hs: &HardState) -> Result<(), BackendError>;
    fn hard_state(&self) -> Result<HardState, BackendError>;
}

/// Opaque durable-commit cursor. Impl-defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommitStamp {
    pub seq: u64,
}
```

### Design points

1. **No engine types leak above `mango-storage`.** All `redb::*` / `raft_engine::*` types are behind associated types and trait methods. Engine-swap affects only the adapter crate.
2. **Write path is async; read path is sync.** Reads go through the MVCC snapshot + in-memory key-index — CPU-bound, not I/O-bound, once data is in the OS page cache. Writes batch and fsync — I/O that must not stall the tokio runtime.
3. **GAT-lending range iterator** (`RangeIter<'a>`). Avoids allocating `Vec<(Bytes, Bytes)>` on every range scan. redb and heed both produce borrowing iterators — this shape matches them natively.
4. **`commit_group` is the fsync-batching primitive** — not an optimization, a requirement per verification H4. It commits N batches with a single fsync.
5. **Multi-Raft forward compatibility** (ROADMAP Phase 10+): each group can have its own `RaftLogStore` instance OR one engine with a group-id-prefixed keyspace. The trait shape accommodates both. `Backend` follows the same pattern.

### Rejected trait shape

`async-trait` on all methods. Rejected because `snapshot()`, `entries()`, `last_index()` are sync in practice (both redb and raft-engine answer from in-memory state), and paying `Pin<Box<dyn Future>>` on the read hot path is the kind of accidental allocation the Performance axis forbids.

## Escape-hatch criteria

Concrete triggers for swapping engines. If any of these fire, the decision above is void and a successor ADR must be written.

### Tier 0 — emergency (same-day action)

1. **CVE filed** against redb or raft-engine that affects mango's usage pattern. Swap plan drafted same day; RUSTSEC advisory treated as de-facto CVE.

### Tier 1 — correctness / reliability (blocker; swap in the phase where it fires)

2. **Differential-test divergence** — mango `Backend` diverges from bbolt on proptest-generated sequences in ways that are not documented bbolt quirks and cannot be root-caused to a wrapper bug.
3. **Reliability-test failure** — any test in `tests/reliability/` fails on the engine and passes on the reference engine (bbolt).
4. **Crash-recovery time** > 30 seconds at 8 GiB after unclean shutdown (measured in Phase 1).
5. **Upstream unmaintained** — redb: no commits on master for 6 months (given A4's commits-within-days cadence, 6 months is a strong signal). raft-engine: no commits on master for 12 months (B3's cadence is already slower — bar is more lenient).

### Tier 2 — performance (defer if feasible; swap if structural)

6. **Throughput floor not met** — Phase 1 bench shows mango losing on _any_ of the four Phase 1 acceptance metrics vs. bbolt (ROADMAP.md:822 — "win on at least one, lose on none").
7. **Tail latency** — Phase 2 MVCC bench p99 read latency > 1.2× bbolt under the 10M-key / 80/20 workload (ROADMAP.md:847).

### Advisory (not an immediate swap; ADR refresh required)

8. **`cargo-geiger` unsafe regression** — redb: +10 tokens above 4.1.0's 37. raft-engine: +10 above the current 49. Either requires ADR refresh before the bump merges.

## Testing strategy

Built on existing Phase 0 / Phase 0.5 infrastructure; new items added to Phase 1 per §"ROADMAP edits":

1. **Bench oracle vs bbolt** — existing item (ROADMAP.md:822). Go binary at `benches/oracles/bbolt/`; workload at `benches/workloads/storage.toml`; hardware sig at `benches/runner/HARDWARE.md`.
2. **Differential-test harness vs bbolt** — NEW Phase 1 item. Proptest sequences, JSON IPC, identical-visible-state assertion.
3. **7-day sustained chaos gate** — NEW Phase 1 item. Disk-EIO + SIGKILL injection on the bench workload.
4. **Engine-swap dry run** — NEW Phase 1 item. In-memory reference `Backend` impl; prove swap is mechanical.
5. **Miri on wrapper** — existing Phase 0.5 infra (task #18). Run on `mango-storage`'s redb adapter.
6. **cargo-geiger baseline** — existing Phase 0.5 infra (task #19). Pin redb@4.1.0 = 37 tokens, raft-engine@<sha> = 49 tokens; +10 regression = CI fail + ADR refresh.
7. **cargo-vet audit** — existing Phase 0 infra (task #21). New entries at the pinned raft-engine SHA and redb 4.1.0+.

## Migration path (if escape-hatch fires)

Cost estimate for swapping engines, given the `Backend`/`RaftLogStore` trait discipline:

- **redb → heed** (the runner-up, most likely candidate): ~2 weeks.
  - Implement `heed` adapter against existing trait.
  - Re-run Phase 1 differential + chaos gates.
  - Update ADR 0002 as superseded by ADR 0002.1.
  - No changes to `mango-mvcc`, `mango-raft`, or above.
- **redb → fjall** (unlikely; only if workload shifts toward write-heavy): ~4 weeks.
  - Same trait work, plus LSM-specific compaction/tombstone semantics probably leak into the trait somewhere.
- **raft-engine → hand-rolled on redb**: ~6 weeks.
  - Non-trivial. Only if raft-engine goes dark.

Keeping the trait clean in Phase 1 is load-bearing for this migration budget. If the trait leaks, the swap cost triples.

## Open questions

1. **redb marquee production users.** No README-listed users; Cuprate is the best visible. Open question carried forward; mitigated by the three Phase 1 gates above.
2. **raft-engine 1.0 timing.** New roadmap item: open an issue on `tikv/raft-engine` asking about 1.0 plans. Link the response into this ADR's "risks" section when received.
3. **cargo-geiger formal numbers.** Token-count proxies (37, 49) are sufficient for monitoring regressions but not authoritative. Run `cargo-geiger` against the full dep tree in Phase 1 and commit the formal numbers to `benches/results/phase-1/geiger.md`.
4. **bbolt protocol-level max value size.** README doesn't state one; etcd enforces `--max-request-bytes` above bbolt (default 1.5 MiB). Confirming this is an etcd-layer limit (not bbolt-layer) strengthens the workload-profile reasoning; deferred to Phase 1 research.
5. **etcd fsync-batching thresholds.** Documented conceptually only. Reading `etcd-io/etcd/server/storage/wal/wal.go` would give authoritative batching-window and group-commit logic. Deferred — not ADR-critical; relevant for the Phase 1 bench comparison.

## References

- `.planning/adr/0002-storage-engine.verification.md` — primary-source verification of every factual claim (2026-04-24).
- `ROADMAP.md` — authoritative north-star.
- `https://github.com/cberner/redb` — redb upstream.
- `https://github.com/cberner/redb/blob/master/docs/design.md` — redb design doc (COW B-tree, commit modes).
- `https://github.com/tikv/raft-engine` — raft-engine upstream.
- `https://www.pingcap.com/blog/raft-engine-a-log-structured-embedded-storage-engine-for-multi-raft-logs-in-tikv/` — PingCAP's design writeup.
- `https://github.com/etcd-io/bbolt` — bbolt upstream (reference oracle).
- `https://github.com/etcd-io/etcd/blob/main/server/storage/backend/backend.go` — etcd's bbolt wrapper (behavioral reference).
- `https://etcd.io/docs/v3.5/op-guide/performance/` — etcd's published throughput numbers (verification H2).
- `https://etcd.io/docs/v3.5/op-guide/hardware/` — etcd hardware tiers (verification H1).
- `https://etcd.io/docs/v3.5/learning/data_model/` — etcd MVCC data model (verification H3).
