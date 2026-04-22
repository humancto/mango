# mango roadmap

A ground-up Rust port of [etcd](https://github.com/etcd-io/etcd). Mango is
**not** wire-compatible with etcd — we own our `.proto` files and design a
clean Rust-native API. etcd is the reference implementation we study; we are
not bound by its Go-isms.

## North star (non-negotiable)

**Mango is a mature, production-grade distributed KV store that beats etcd
on every axis we care about.** Not a toy port. Not a learning exercise. Not
"good enough." Every plan, every PR, every architectural decision is judged
against the bar below. If a change merely matches etcd, that is a regression
relative to the goal — find the win.

The six axes mango must beat etcd on. Every claim has a comparison oracle
and a measurable threshold — the comparison oracle is the **pinned etcd
binary** in `benches/oracles/etcd/` (etcd v3.5.x; exact version pinned per
release in `benches/oracles/etcd/VERSION`), running on the **same hardware
class** described in `benches/runner/HARDWARE.md`, driven by the **bench
runner scripts** under `benches/runner/`. "etcd's published numbers" is
never an acceptable oracle; we run the comparison ourselves.

1. **Performance** — vs etcd v3.5.x on the same hardware, driven by the
   committed runner scripts:
   - 1KB Put throughput on a 3-node loopback cluster: **≥ 1.5× etcd's**.
   - p99 client latency at 50% of mango's saturation: **≤ 0.7× etcd's**
     at the same absolute QPS.
   - Resident set size at idle (3-node cluster, empty data dir): **≤ 0.7×
     etcd's**.
   - Cold start (process exec → first successful Put accepted): **≤ 0.7×
     etcd's**.
   - Leader-failover-to-quorum-write time after `SIGKILL` of the leader:
     **≤ 0.7× etcd's**.
2. **Correctness** — verifiable by external parties:
   - Public Jepsen run published in CI (Phase 13), with results uploaded
     to a GitHub Pages site so the claim is externally checkable.
   - Deterministic simulator (Phase 5 onwards) replays every reported
     bug from a seed.
   - Property tests for every state machine; the simulator runs every
     property test under a panic hook that fails on any panic from
     non-test code.
3. **Operability** — measured by:
   - Every metric documented in `docs/metrics.md` with declared label set
     and cardinality bound; CI test scrapes `/metrics` and asserts each
     family's distinct label-value count stays below its declared bound
     under a 10k-key workload.
   - `mango --check-config <path>` validates the entire config and exits
     non-zero on any conflict; tested against a malformed-config corpus.
   - Follower restart against a 10M-revision cluster causes ≤ 1.2× the
     steady-state network ingress on the leader for ≤ 30s (no
     thundering herd).
   - No leader-flap storms during membership change: zero leader changes
     during a 1-member-add-then-promote cycle on a healthy 3-node
     cluster, asserted in CI.
4. **Safety** — mechanically enforceable:
   - `unsafe_code = "forbid"` workspace-wide except in audited, named
     modules with documented invariants and a `# Safety` comment block
     on every `unsafe` block.
   - Supply-chain hardening: SHA-pinned actions, `cargo-deny`,
     `cargo-audit`, `cargo-vet`, SBOM via `cargo-cyclonedx`.
   - **No panics in steady state**, operationalized as: `[profile.release]
     overflow-checks = true`; clippy denies `unwrap_used`, `expect_used`,
     `panic`, `unimplemented`, `todo`, `indexing_slicing`,
     `arithmetic_side_effects`, `cast_possible_truncation`,
     `cast_sign_loss` in non-test code; Phase 13 simulator runs with a
     panic hook that fails the test on any panic from non-test code;
     Phase 15 chaos test runs ≥1 hour and fails on any panic.
   - Every public fallible op returns a typed crate-local `Error` enum;
     `Box<dyn Error>` in a public API is an auto-`REVISE`.
5. **Developer ergonomics** — measured by:
   - CI cold ≤ 5 min, warm ≤ 90s (CI-asserted via job duration check
     starting Phase 11).
   - `mango cluster up --nodes 3` brings up a working local cluster in
     ≤ 10s and prints connection info.
   - `cargo doc --open` for `mango-client` shows zero `prost`/`tonic`
     types in the public API surface (CI-checked via a doc-extracted
     allowlist).
   - `cargo public-api --diff` clean against the previous tagged release
     unless the PR is tagged `breaking-change`.
6. **Storage efficiency** — vs the same-data-load etcd cluster:
   - On-disk size after the same workload: **≤ 0.7× etcd's** (with
     mango's default compression on, etcd's default off — both
     defaults).
   - Compaction: bounded CPU (≤ 25% of one core during compaction) and
     read p99 during compaction within **1.5× of steady-state read
     p99**.

When two approaches both work, pick the one that wins on at least one axis
without losing on the others. When a winning-on-X approach loses on Y,
document the trade-off explicitly in the plan and get the expert agent to
acknowledge it. **The expert agent treats "this is fine" as failure; the
bar is "this beats etcd."**

## Working rules

- One checked item per PR. Small, atomic, mergeable. No mega-PRs.
- Every plan declares which of the six north-star axes the item moves on,
  and how it will be measured. "Doesn't move any axis" is a valid answer
  for plumbing PRs (CI, formatting, etc.) — but the next item with real
  user-visible behavior must.
- Every phase ends with `cargo test --workspace` green and the new
  behavior exercised by tests (unit + property + integration where
  appropriate). Property tests are the default for any data-structure or
  protocol code, not unit tests.
- Every phase that touches a hot path includes a Criterion benchmark
  checked into `benches/`, with a baseline number recorded in the plan
  and a regression gate enforced in CI by Phase 11.
- The relevant expert agent (currently `rust-expert`) reviews both the
  plan and the final diff. No merge without `APPROVE`. The expert is
  instructed to apply the north-star bar, not the "does it compile and
  pass tests" bar.
- Items inside a phase are roughly ordered by dependency. Phases are
  strictly ordered: don't start phase N+1 until phase N's checked items
  are done unless the items are explicitly independent.
- No `TODO` / `FIXME` / `unimplemented!()` / `todo!()` shipped to `main`.
  If a follow-up is needed, it goes on the roadmap as a new item, not as
  a comment in the code.

## Out of scope (for now)

- Wire compatibility with real etcd's `etcdserverpb` (clients written for
  etcd will not work against mango). The Phase 12.5 etcd-import tooling
  handles data migration; client rewriting is the user's responsibility.
- gRPC gateway / HTTP+JSON transcoding.
- gRPC proxy.
- v2 store / v2 API (etcd's deprecated legacy API).
- Multi-language client SDKs beyond Rust. (A second-language client is a
  post-1.0 stretch goal.)
- **Multi-tenancy as a first-class concept.** Mango's auth model (Phase 8)
  has users + roles + per-user rate-limit + per-user keyspace quota,
  which is enough to operate a shared cluster carefully. True multi-
  tenancy (namespace isolation, per-tenant quotas at the data-dir level,
  per-tenant audit logs, billing) is explicitly out of scope. Etcd does
  not have it either; if it becomes a must-have, file as a new phase.
- KMS integration for backup-at-rest encryption. Phase 10 ships
  operator-supplied keys; KMS adapters (AWS KMS, GCP KMS, HashiCorp
  Vault) are stretch.

If any of these become must-haves later, add them as new phases at the end.

## Definition of Done (every phase)

A phase is not "done" — and items inside it are not mergeable — unless all of
the following hold for the surface the phase introduces. The expert agent
enforces this list in plan + diff review.

- **Tested.** Property tests for every data structure or protocol op (not
  unit tests). Integration tests for every cross-crate boundary. Crash /
  recovery tests for anything that touches disk. Concurrency tests for
  anything that touches `async` or threads.
- **Benchmarked.** Criterion bench for every hot path with a baseline
  number recorded in the plan. Where etcd has a public benchmark for the
  equivalent feature, mango must beat it on the same hardware (document
  the comparison in `benches/README.md`); where it does not, mango sets
  the baseline.
- **Observable.** Every public op emits a `tracing` span at INFO with
  stable target name and stable field names. Every error path logs at
  WARN or ERROR with enough context to debug from the log alone.
  Hot-path metrics added to the Prometheus exposition (Phase 11 wires
  the endpoint; phases before that emit through the `metrics` facade so
  the wiring is plumbing-only).
- **Failure-safe.** No `unwrap()` / `expect()` / `panic!()` /
  `unimplemented!()` / `todo!()` in non-test code. Every fallible op
  returns a typed error in a crate-local `Error` enum. `unsafe` is
  forbidden workspace-wide; per-module opt-in requires a `# Safety`
  comment block on every `unsafe` block and a sign-off line in the PR
  description.
- **Documented.** Public items have rustdoc with at least one example
  that compiles (doctest). User-facing config and CLI flags documented
  in the `docs/` site (Phase 12 builds the site; phases before that
  ship docs as `.md` next to the code).
- **Backwards-compatible at the API boundary** once Phase 6 ships gRPC
  publicly. Until then, every breaking change is fine but must be
  flagged in the PR description.

## Reviewer's contract (the rust-expert agent)

The expert agent's `APPROVE` is the merge gate. To remove ambiguity, here
is the decision rule the agent applies on every plan + diff review.

### `APPROVE` requires all of:

1. The plan or PR description **declares which north-star axis the change
   moves** (or honestly declares it as plumbing, e.g. CI / formatting).
2. **For perf-claiming PRs:** before/after Criterion numbers, with the
   comparison oracle named (etcd version, bench command, hardware sig
   from `benches/runner/HARDWARE.md`), committed under
   `benches/results/<phase>/`.
3. **For correctness-claiming PRs:** at least one new property test or
   simulator scenario that would have caught the previous bug or class
   of bug.
4. **For unsafe code:** every `unsafe` block has a `// SAFETY:` comment
   naming the invariant; PR description has a sign-off line citing Miri
   output (`MIRIFLAGS=... cargo +nightly miri test -p <crate>`) or a
   written justification for why Miri doesn't apply (e.g. FFI).
5. **For new public API:** at least one doctest, `#[must_use]` where
   applicable, considered `#[non_exhaustive]` for new enums, and
   `cargo public-api --diff` output in the PR.
6. **CI green:** clippy clean (no `#[allow]` without a comment), tests
   green including doctests, fmt clean, deny clean, audit clean.
7. **No new `TODO` / `FIXME` / `unimplemented!()` / `todo!()`** introduced.
8. The change either moves a north-star axis with measured evidence, or
   is honestly declared as plumbing (#1).

### Auto-`REVISE` triggers (no thinking required):

- A new metric label that takes a user-controlled value (key, lease ID,
  user ID, etc.).
- `.unwrap()` / `.expect()` / `panic!()` / `todo!()` / `unimplemented!()`
  outside `#[cfg(test)]` (clippy enforces once Phase 0 lint config lands).
- A new `unsafe` block without a `// SAFETY:` comment.
- A `tokio::sync::Mutex` or `std::sync::Mutex` lock guard held across an
  `.await`.
- A new `Box<dyn Error>` in a public API.
- A spawned task without a `JoinHandle` stored or a documented
  fire-and-forget justification.
- A new `Arc<Mutex<T>>` without a one-line note explaining why a
  redesign wasn't possible.
- A bench-claiming PR without numbers, or with numbers from an unnamed
  oracle.

### `APPROVE_WITH_NITS` is for:

- Style-only nits where the substantive bar is met.
- Bench numbers that meet the gate but want re-run on quieter hardware.
- Documentation polish opportunities.

### What "treats 'this is fine' as failure" means in practice:

If the reviewer's instinct is "this works, ship it" — but the PR did not
move any north-star axis, did not add a property test, did not add a
bench, and did not declare itself as plumbing — the verdict is `REVISE`
with the question: *what does this PR do that beats etcd?* If the answer
is "nothing, it just keeps parity," then the implementation needs to
find the win or the scope needs to expand.

---

## Phase 0 — Foundation

Get the workspace into a state where every subsequent phase can move fast:
deterministic builds, CI on every push, lints, formatting, supply-chain
hardening, the bench oracle harness, and a place to put proto definitions.

- [x] Set up CI (GitHub Actions): `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`, on push and PR
- [ ] Add `rustfmt.toml` and `.editorconfig` so formatting is unambiguous
- [ ] **Lint hardening**: workspace `Cargo.toml` denies `clippy::unwrap_used`, `clippy::expect_used`, `clippy::panic`, `clippy::unimplemented`, `clippy::todo`, `clippy::indexing_slicing`, `clippy::arithmetic_side_effects`, `clippy::cast_possible_truncation`, `clippy::cast_sign_loss`, `clippy::dbg_macro`, `clippy::print_stdout`, `clippy::print_stderr`, `clippy::await_holding_lock` in non-test code (`#[cfg(not(test))]`); allow them in tests. This is what operationalizes the north-star "no panics in steady state" bar.
- [ ] **Release-profile overflow checks**: `[profile.release] overflow-checks = true` in workspace `Cargo.toml`. Catches arithmetic panics in production, not just debug. Documented trade-off (~1-3% perf hit) accepted.
- [ ] Add `deny.toml` and a `cargo-deny` CI job (license + advisory + duplicate-version checks; ban `git`-deps without explicit allowlist)
- [ ] Add `cargo-audit` CI job (RustSec advisories) running on push, PR, and a nightly schedule; failures block merge
- [ ] Add `cargo-vet` (or equivalent supply-chain audit gate) so every transitive dep has an audit entry; missing audits fail CI
- [ ] Add an SBOM build step (`cargo-cyclonedx`) that produces a CycloneDX file per release; attached to GitHub Releases in Phase 12
- [ ] Add a `cargo-msrv` job pinning the minimum supported Rust version (start at 1.80, bump deliberately) so we don't accidentally raise it
- [ ] Add a `cargo doc --no-deps --document-private-items` job with `RUSTDOCFLAGS=-D warnings` so broken doc links fail CI
- [ ] Add `cargo-public-api` CI check (warn-only until Phase 6 ships gRPC; gates breaking changes from Phase 6 onwards)
- [ ] Add a Renovate / Dependabot config so action SHAs and crate versions get bumped via PR (preserves the SHA-pin policy without it rotting)
- [ ] **Bench oracle harness scaffold**: `benches/oracles/etcd/` checks in a script that downloads etcd v3.5.x at a pinned version + sha256, plus `benches/runner/HARDWARE.md` documenting the canonical hardware spec we run comparisons on, plus `benches/runner/run.sh` that prints a hardware signature alongside every result. Without this, every later "beats etcd by Nx" claim has no oracle.
- [ ] **Monotonic-clock policy**: workspace doc note in `docs/time.md` declaring "all protocol-relevant time math uses `Instant` (monotonic), never `SystemTime`. Wallclock is used only for human-facing logs and lease TTL display, never for protocol decisions. Leap seconds: documented as N/A. NTP step tolerance: tested with ±5s clock jumps in Phase 13."
- [ ] Create `crates/mango-proto` skeleton with `tonic-build` and a hello-world `.proto` that compiles
- [ ] Add `CONTRIBUTING.md` covering branch naming, commit style, PR template, the test bar, **and the north-star bar + reviewer's contract**
- [ ] Add a PR template that forces every PR description to declare which north-star axis the change moves and how it was measured (or honestly mark as plumbing)

## Phase 1 — Storage backend (single-node, no MVCC yet)

A durable, transactional, ordered-key K/V store. This is the equivalent of
etcd's `mvcc/backend` layer that wraps bbolt — we pick the Rust analogue and
abstract it behind a `Backend` trait. No revisions yet; that lives in
phase 2.

- [ ] Choose the storage engine (sled / redb / rocksdb / hand-rolled) — write an ADR in `.planning/adr/` after the rust-expert weighs in. **Decision criterion: must beat the bbolt comparison oracle in `benches/oracles/bbolt/` (a checked-in Go binary running the workload defined in `benches/workloads/storage.toml` against bbolt at a pinned version) on at least one of (write throughput, read latency at p99, on-disk size for the same dataset, fsync amplification) without losing on the others.**
- [ ] `crates/mango-storage` skeleton with the chosen engine as a dependency
- [ ] Define `Backend` trait: `begin_read()`, `begin_write()`, named buckets/trees, `put`, `get`, `delete`, `range`, `commit`, `force_commit`
- [ ] Implement `Backend` against the chosen engine, with on-disk durability and `fsync` semantics at least as strong as etcd's batch-tx model (commit on N writes or T millis), with the batching parameters tunable
- [ ] Property tests: random put/get/delete/range sequences match an in-memory `BTreeMap` oracle (proptest, 10k+ cases)
- [ ] Crash-recovery test: kill mid-write via a panic, reopen, assert no torn state and no committed data lost
- [ ] Crash-recovery test under simulated fsync failure (return `EIO`) — backend either commits cleanly or reports failure; no silent data loss
- [ ] Bench harness in `benches/storage/`: write-throughput (1KB values, batched and unbatched), read-latency p50/p95/p99 (hot and cold cache), range-scan-throughput (100 / 10k / 100k keys), on-disk size after the workload in `benches/workloads/storage.toml`. Comparison oracle is the Go binary at `benches/oracles/bbolt/` on the hardware sig in `benches/runner/HARDWARE.md`. **Mango must win on at least one metric, lose on none. Numbers committed to `benches/results/phase-1/`.**
- [ ] Block-level compression (LZ4 or zstd, configurable) — disabled by default for parity bench, enabled for the size-comparison number

## Phase 2 — MVCC layer

etcd's MVCC: every write produces a new revision; keys are addressed by
`(key, revision)`; tombstones; compaction. Built on top of the phase-1
backend.

- [ ] Define `Revision { main: i64, sub: i64 }` and the on-disk key encoding (`key_index` + `key`-bucket layout, mirror etcd's split conceptually)
- [ ] Implement `KeyIndex` (in-memory tree of keys → list of generations of revisions) with put / tombstone / compact / restore-from-disk
- [ ] Implement the MVCC `KV` API: `Range`, `Put`, `DeleteRange`, `Txn` (compare + then/else ops), `Compact`
- [ ] Read transactions return a consistent snapshot at a chosen revision
- [ ] Compaction: physically removes old revisions; `Range` against a compacted revision returns `ErrCompacted`
- [ ] **Online compaction with bounded impact** — etcd's compaction can pause readers; mango compacts in the background with bounded CPU (≤ 25% of one core) and read p99 during compaction within **1.5× of steady-state read p99**. Bench gate in `benches/mvcc/` confirms; numbers committed to `benches/results/phase-2/compaction.md`. (Stronger claims like "zero impact" are aspirational and engine-dependent — this is the honest target that still beats etcd, whose major compactions cause much larger spikes.)
- [ ] Property tests: random ops + random snapshot reads match a model implementation (proptest, 10k+ cases, shrinking on)
- [ ] Restore-from-disk test: persist via backend, drop the in-memory index, reopen, all reads consistent
- [ ] **`cargo fuzz` target for the on-disk key encoding** (`encode_key`/`decode_key` round-trip, plus malformed-input → no panic). Per the Reviewer's contract: parser fuzz lives in the phase that introduces the parser, not deferred to Phase 15. CI plumbing for the fuzz job lives in Phase 15.
- [ ] Bench in `benches/mvcc/`: 10M-key dataset, 80/20 read/write mix, 1KB values; p50/p95/p99 latency and throughput. Comparison oracle is the same workload run through etcd's `tools/benchmark` (pinned etcd v3.5.x in `benches/oracles/etcd/`) on the hardware sig in `benches/runner/HARDWARE.md`. **Mango wins on p99 read latency and on-disk size, ties or wins on write throughput.** Numbers committed to `benches/results/phase-2/mvcc.md`.

## Phase 3 — Watch

Streaming change notifications. Watchers subscribe to a key range from a
revision and receive every event at or after that revision. Includes
`watchable_store`, fragmenting, progress notifications.

- [ ] `WatchableStore` wrapping the MVCC store: `watch(range, start_rev) -> stream of Events`
- [ ] Synced vs unsynced watcher groups (catch-up path for watchers behind current revision)
- [ ] Event coalescing per-revision per-watcher
- [ ] Progress-notify ticker (periodic `WatchResponse` with current revision so idle watchers know they're current)
- [ ] Cancel + clean shutdown of a watcher; bounded per-watcher channel with backpressure (slow consumer disconnect policy documented)
- [ ] Tests: 1000 keys, 100 concurrent watchers, no missed and no duplicate events; restart mid-stream

## Phase 4 — Lease

Time-bounded keys. Clients grant a lease, attach keys to it, and either
keep-alive it or let it expire (which deletes all attached keys).

- [ ] `Lessor` data structures: lease ID → expiry, lease ID → set of keys, key → lease ID
- [ ] `Grant`, `Revoke`, `Attach`, `Detach`, `KeepAlive`, `TimeToLive`, `Leases` operations
- [ ] Expiry loop: revoke leases past TTL; revocation deletes attached keys via the MVCC layer (single revision)
- [ ] Persist lease state to the backend so it survives restart; rebuild attachments on startup
- [ ] Tests: granted lease expires on time, keepalive resets TTL, revoke deletes attached keys atomically, restart preserves leases
- [ ] Bench: 100k active leases, expiry processed within one tick

## Phase 5 — Raft consensus (single-node + 3-node cluster)

The hardest phase. Decide between `tikv/raft-rs`, `openraft`, or hand-roll;
the `rust-expert` decides at plan time. Wrap whatever we pick behind a
`Consensus` trait so the upper layers don't care.

- [ ] ADR in `.planning/adr/` choosing the Raft implementation (rust-expert decides at plan-review time). **Decision criterion: faster leader-failover recovery than etcd, lower steady-state CPU, and a clean path to deterministic-simulation testing. If no off-the-shelf crate hits all three, we hand-roll.**
- [ ] `crates/mango-raft` skeleton with the chosen crate (or hand-rolled module structure)
- [ ] Single-node Raft: proposals get applied to a state-machine trait; the state-machine is wired to the MVCC store
- [ ] WAL: append every entry before applying; replay on startup; **bounded WAL space** with retention by size + time, oldest segment recycled or deleted post-snapshot. Documented behavior when WAL disk fills (refuse new proposals, raise `WAL_FULL` alarm).
- [ ] Snapshot: state-machine snapshot + WAL truncation; reload on startup if WAL gap; **snapshot streaming has a configurable bandwidth limit** so it cannot saturate the network and cause Raft heartbeat timeouts on the leader.
- [ ] 3-node cluster over TCP transport: leader election, log replication, follower catch-up
- [ ] Linearizable reads via ReadIndex (no stale reads from followers without quorum-check)
- [ ] **Pipelined log replication + batch commit** — one of mango's core perf wins over etcd; bench gate vs single-flight replication baseline
- [ ] **Deterministic simulation testing harness from day one** — fake clock + fake network + seeded RNG; every Raft test in this phase runs in the simulator, not against real wallclock + real sockets. (The Phase 13 robustness work extends this; it does not start it.)
- [ ] Network-partition tests in the simulator: 2/1 split, 1/1/1 split, leader isolation, asymmetric partitions, message reordering; assert no split-brain, no lost committed entries
- [ ] Crash-recovery tests in the simulator: kill follower mid-replication, kill leader mid-commit, restart, cluster converges
- [ ] **`cargo fuzz` target for WAL record decode** (per the Reviewer's contract: parser fuzz lives where the parser does). CI plumbing in Phase 15.
- [ ] Bench in `benches/raft/`: 3-node cluster on local loopback, 1KB Put values, runner script `benches/runner/raft.sh` invoking `etcd-benchmark put --conns=100 --clients=1000 --total=100000 --val-size=1024` against the pinned etcd in `benches/oracles/etcd/` and the equivalent against mango on the hardware sig in `benches/runner/HARDWARE.md`. **Mango beats etcd by ≥ 1.5× on throughput and ≤ 0.7× on p99 commit latency, and recovers from a `SIGKILL`'d leader in ≤ 0.7× of etcd's recovery time.** Numbers committed to `benches/results/phase-5/raft.md`.

## Phase 6 — gRPC server: KV + Watch + Lease

Wire phases 2–5 to the network. `mango-server` hosts the gRPC services and
is the binary you actually run.

- [ ] Author `.proto` for KV, Watch, Lease (Rust-native shape; copy etcd's semantics, not its message names)
- [ ] `crates/mango-server`: KV service backed by Raft-replicated MVCC
- [ ] Watch service: server-streaming RPC backed by phase-3 `WatchableStore`
- [ ] Lease service: unary + bidi `LeaseKeepAlive` stream backed by phase-4 `Lessor`
- [ ] Health and `Status` endpoints (cluster ID, member ID, leader, raft index, db size)
- [ ] Configuration via TOML file + CLI flags + env (precedence: CLI > env > file > default), with strict schema validation at startup (reject unknown keys; refuse to start on conflicts). `mango --check-config <path>` exits non-zero on any error and prints actionable diagnostics. Tested against a malformed-config corpus.
- [ ] **Graceful shutdown**: SIGTERM drains in-flight RPCs within configurable budget, then exits cleanly; no half-applied Raft proposals
- [ ] **Backpressure everywhere** — every server-streaming RPC has a bounded send buffer with documented slow-consumer policy; no unbounded memory growth under client misbehavior
- [ ] **gRPC DoS hardening**: server enforces `max_decoding_message_size` (default 4 MiB), `max_concurrent_streams` (default 1000 per connection), `http2_keepalive_timeout`, `http2_keepalive_interval`, per-connection request rate limit. Defaults documented in `docs/server-config.md` and tested with a misbehaving-client harness (oversized frames, slow-loris, stream-flood).
- [ ] **`cargo fuzz` targets for**: every `.proto` decode path, the config TOML parser, and gRPC request-body decoders (per Reviewer's contract — fuzz lives where the parser does; CI plumbing in Phase 15).
- [ ] Integration tests: spin up a 3-node mango cluster in-process, run KV + Watch + Lease scenarios end-to-end
- [ ] End-to-end bench at the gRPC boundary, runner script `benches/runner/grpc.sh`: 3-node cluster on hardware sig in `benches/runner/HARDWARE.md`, real client (mango-client and `etcd-benchmark put` against etcd v3.5.x in `benches/oracles/etcd/`), 1KB Put at saturation. **Beats etcd by ≥ 1.5× on throughput and ≤ 0.7× on p99 latency at 50% of saturation.** Numbers committed to `benches/results/phase-6/grpc.md`.

## Phase 7 — `mangoctl` CLI client

User-facing CLI mirroring `etcdctl`'s ergonomics: `put`, `get`, `del`,
`watch`, `lease grant/revoke/keep-alive`, `member list/add/remove`,
`endpoint status/health`, `compaction`, `defrag`, `snapshot save/restore`.

- [ ] `crates/mango-client`: typed Rust client over the phase-6 gRPC services. **No `prost`/`tonic` types in the public API surface** (verified by a doc-extracted allowlist in CI per the dev-ergonomics axis).
- [ ] **Client endpoint failover** with explicit, documented semantics: round-robin or pinned per `Endpoint`, automatic failover on connection loss, health-check policy, retry policy with exponential backoff and bounded retries. Tested against a fault harness that drops endpoints mid-call. (Avoids etcd's well-known client-balancer footguns.)
- [ ] `crates/mangoctl` with `clap`-based subcommands and human + JSON output formats
- [ ] `put`, `get`, `del`, `range` subcommands with txn support
- [ ] `watch` subcommand (streaming output)
- [ ] `lease` subcommand group
- [ ] `endpoint status`, `endpoint health`
- [ ] Integration tests against an in-process cluster: every subcommand exercised

## Phase 7.5 — Web UI (read-only browse mode)

Ships an early, narrowly-scoped slice of the eventual full operational
console (Phase 16 + 16.5) so users get localhost-browse value as soon as
KV is real. **Read-only only** — no mutations until Phase 16, which lands
after auth (Phase 8) so destructive ops are gated by RBAC.

This phase exists to (a) prove the **out-of-process** UI architecture
(see "Architecture" below — the UI is a *separate binary*, never inside
`mango-server`), (b) pick the frontend stack via ADR before Phase 16
commits to it, (c) give the project a visible artifact that demos well
long before the full console.

### Architecture (load-bearing)

The UI is a **separate binary** (`mango-ui`) built from `crates/mango-ui`
that talks to a mango cluster as a regular gRPC client. It is **not**
hosted inside the `mango-server` process. Rationale:

1. **Blast-radius isolation.** A panic, OOM, or stack overflow in a UI
   handler does not crash the storage server, does not affect Raft
   membership, does not stall heartbeats. A misbehaving UI request
   (e.g. export-as-JSON of a 10M-key range) competes for memory and
   scheduler time with *itself*, not with Raft.
2. **Dependency-tree isolation.** Adding `axum` + `tower-http` + the
   chosen frontend stack to `mango-server` would inflate its compile
   time, `cargo-deny` surface, and `cargo-audit` surface. A CVE in any
   UI dep would be a CVE in the storage server. Out-of-process means
   `mango-server` carries none of this.
3. **Deploy-time isolation.** Production deployments that don't want
   any UI just don't install the `mango-ui` binary. That is a stronger
   guarantee than "we forgot to pass `--ui-listen`."

`mango cluster up` (Phase 12) starts both binaries side-by-side for
local dev; production users install only what they need.

### Items

- [ ] **Frontend-stack ADR** in `.planning/adr/` (rust-expert + a frontend-design pass at plan time). Candidates: server-rendered HTML + HTMX, Rust→WASM (Leptos / Dioxus / Yew), React + Vite + TS, SvelteKit. Decision criteria, in order: **(a) fastest dev iteration**, **(b) smallest binary contribution**, **(c) no Node in the user's `cargo install` build path** (Node tooling, if any, runs only at release time; built artifacts are checked in or fetched, never built from source on user installs), **(d) first-meaningful-paint ≤ 1s on a simulated 1 Mbps connection**, **(e) team can carry the stack for ≥ 2 years**. The rust-expert's prior recommendation for the rest of the team's consideration: **HTMX + askama (or maud) for Phase 7.5** (read-only forms / tables; ~14 KB; zero build step; stays Rust-only); **Leptos with SSR + islands for Phase 16** (client-side state for the txn builder + topology view; ~150-400 KB WASM after `wasm-opt`; SSR keeps first-paint fast); SvelteKit-with-built-artifacts as the fallback if Leptos is vetoed by the frontend reviewer.
- [ ] `crates/mango-ui` skeleton: a **separate binary** (`mango-ui`) built from `crates/mango-ui` that talks to mango via the Phase 6 gRPC client. **Not embedded in the `mango-server` process** — see "Architecture" above for the reasoning. Default port `:2381`, configurable.
- [ ] **Operator opt-in required, even for localhost.** Starting `mango-ui` requires `--listen <addr>` set OR `[ui] listen = "..."` in the config OR `MANGO_UI_LISTEN` env; there is no auto-start mode. **The UI is OFF by default in every shipped artifact.** The `dev-ui` cargo feature (non-default, forbidden in release-CI builds via a `cargo metadata` check) lets mango developers run `cargo run --features dev-ui --bin mango-ui` with a default localhost listener — this is for *mango contributors only* and never enabled in published binaries. Documented bluntly in `docs/ui-deployment.md`: "the previous version of this spec said 'on by default in dev profile' — that is impossible to do honestly because Cargo build profiles are not visible at runtime, so anyone running `cargo install --debug mango-ui` would get the UI on without knowing it. The current rule is: opt-in everywhere, no exceptions."
- [ ] **Startup WARN banner** when the UI is enabled: "Mango UI is enabled on `<addr>`. The UI is read-only but exposes every key/value in the cluster to anyone who can reach this address. Do not store secrets in mango until Phase 8 (auth) is shipped and the cluster is auth-enabled. To disable, remove `--listen` / `[ui] listen`." Banner is printed to stderr and logged at WARN so it's visible in both interactive and structured-log shipping.
- [ ] **Bind discipline**:
  - Default bind is `127.0.0.1:2381` (IPv4 loopback only).
  - `[ui] listen = "[::1]:2381"` is also accepted as "localhost-only."
  - Any non-loopback bind requires both `--insecure-ui` AND TLS configured (`--ui-tls-cert` + `--ui-tls-key`); refuse to start otherwise with an actionable error.
  - **Container / pod warning** in `docs/ui-deployment.md`: in Docker, `127.0.0.1` is the *container's* loopback; use `-p 127.0.0.1:2381:2381` on the host side or `--network host` with mango-ui's own bind. In Kubernetes, every container in the same pod shares `127.0.0.1`; the UI MUST be disabled in pods that host untrusted sidecars.
  - Tested: a startup-config matrix asserts every (bind, tls, insecure-ui) combination either starts cleanly or refuses-and-exits-non-zero with the documented diagnostic.
- [ ] **`docs/ui-readonly-warning.md`**: explicit "do not store secrets in mango until Phase 8 is shipped" — Kubernetes Secrets, Vault backend storage, application credentials, TLS private keys. Linked from the README and from the WARN banner.
- [ ] Browse view: prefix-search keys, paginated list, view value + revision + lease + version. Renders binary values as hex with a UTF-8 toggle.
- [ ] Range query view: from / to keys, limit, sort order; results paginated.
- [ ] Cluster status view (read-only): cluster ID, member list, leader, raft index, db size — driven by the Phase 6 `Status` endpoint.
- [ ] `mangoctl ui` subcommand: spawns `mango-ui` against a configured external cluster (sugar — equivalent to `mango-ui --listen ... --endpoints ...`). **Each `mangoctl ui` instance generates a unique `ui_instance_id` (UUID v7) at startup** and includes it in every backend gRPC call's metadata; the audit log (Phase 8) records it alongside the user identity, so an incident response can distinguish "admin from laptop A" from "admin from laptop B."
- [ ] Integration tests with `axum-test` (or equivalent): every UI route returns the expected shape; no XSS in value rendering (stored-XSS test with a malicious value); no panics on malformed input (fuzz target on the search query parser, per Reviewer's contract).
- [ ] Bench: UI page loads ≤ 100ms p99 against a 1M-key cluster (the browse list is paginated; this measures pagination + render). No external comparison oracle exists (etcd ships no UI; etcdkeeper is unmaintained); this sets mango's own baseline for Phase 16. Numbers committed to `benches/results/phase-7.5/ui-readonly.md`.

## Phase 8 — Authentication & authorization

etcd's auth model: users, roles, role-based key-range permissions, password
auth, token-based session, optional mTLS.

- [ ] `Authenticator` trait + simple-token and JWT-token implementations
- [ ] Users + roles + role permissions persisted in their own buckets, replicated via Raft
- [ ] `Auth` gRPC service: enable/disable, user add/remove/grant-role, role add/grant-permission
- [ ] Authorization middleware on every KV/Watch/Lease op (RBAC over key ranges)
- [ ] mTLS for both client-server (`:2379`-equivalent) and peer-to-peer (`:2380`-equivalent) — cert + key + CA flags wired through config
- [ ] **Peer authorization**: a member-allowlist (by cert SPKI fingerprint or by issued cluster token) — cert-presentation alone is insufficient; new peers must be explicitly allowlisted by an existing voting member's `member add` call. Rejects rogue peers even with valid CA-signed certs.
- [ ] **Per-client rate limiting and per-user keyspace quotas** enforced at the gRPC interceptor layer. Limits configurable per-user. Rejects with a typed `ResourceExhausted` error and emits a metric. Tested with a hostile-client harness.
- [ ] **Audit logging** — separate sink from `tracing`, append-only, tamper-evident (hash-chain over consecutive records). Every authn/authz decision and every mutating op records: timestamp (Instant + wallclock for human reading), user, action, key range, success/failure, request ID. Default sink: `data-dir/audit.log`; OTLP and stderr sinks configurable. Verified by a tamper-detection test.
- [ ] `mangoctl auth`, `mangoctl user` / `mangoctl role`, `mangoctl audit verify <log>` subcommands
- [ ] Tests: authenticated client can read/write, anonymous client rejected, role permission boundaries enforced, mTLS round-trips, peer-allowlist rejection, rate-limit and quota enforcement, audit log tamper detection

## Phase 9 — Cluster membership & learner nodes

Reconfiguring a running cluster: add/remove a member, learner promotion,
member metadata.

- [ ] Membership change as a Raft `ConfChange` (single-server change at a time, joint-consensus optional/later)
- [ ] Learner node state: replicates the log but does not vote and does not count toward quorum
- [ ] Promote-learner-to-voter API with safety check (learner must have caught up to within N entries of leader)
- [ ] `Cluster` gRPC service: member list/add/remove/promote/update
- [ ] `mangoctl member` subcommand group including `member add --learner` and `member promote`
- [ ] Tests: 3-node cluster + add learner, learner catches up, promote, remove old member, no quorum lost

## Phase 10 — Snapshot, backup, defrag, maintenance

Operational features needed to run mango in production.

- [ ] `snapshot save` (streamed snapshot from a member)
- [ ] `snapshot restore` (rebuild a single-node cluster from a snapshot file)
- [ ] **Backup encryption at rest**: snapshot files support AES-256-GCM encryption with operator-supplied key (KMS integration deferred to stretch); key rotation tested; snapshots without an encryption key continue to work for backwards compat. `mangoctl snapshot save --encrypt --key-file=...`.
- [ ] **`mangoctl snapshot verify <file>`**: validates a snapshot file's integrity offline before restore (checksum + structure walk + key-range sanity); exits non-zero on any issue. `cargo fuzz` target on the snapshot decoder lives here too.
- [ ] **Online defrag** — defrag does not take the node out of read-rotation; reads continue to be served from the live backend while the new compacted backend is built side-by-side; final swap is brief (≤ 100ms). Etcd's defrag takes the node down for the duration; this is a real differentiator.
- [ ] `defrag` (compact the on-disk backend after MVCC compaction)
- [ ] `Maintenance` gRPC service: `Status`, `Snapshot`, `HashKV`, `Defragment`, `Alarm` (`NOSPACE` / `CORRUPT` / `WAL_FULL`)
- [ ] Quota: refuse writes when DB size exceeds configured quota; raise NOSPACE alarm
- [ ] `mangoctl snapshot save/restore/verify`, `mangoctl defrag`, `mangoctl alarm list/disarm`
- [ ] Tests: snapshot a populated cluster, restore into a fresh node, data identical; quota tripping behavior; encrypted-snapshot round-trip; online defrag with concurrent reads (no read errors, p99 within 1.5× steady-state during the swap)

## Phase 11 — Observability

Production readability: structured logs, Prometheus metrics, tracing
spans on every RPC and Raft action. The bar is **strictly better than
etcd's defaults** — etcd's logs and metrics are functional but
inconsistent (mixed klog / zap, label cardinality blowups). Mango ships
correct from day one.

- [ ] `tracing` + `tracing-subscriber` wired across every crate with stable target names (`mango.server`, `mango.raft`, `mango.mvcc`, `mango.lease`, `mango.watch`, `mango.client`); never the default Rust module path
- [ ] Default filter exposes user-relevant events without `RUST_LOG` tuning; `MANGO_LOG` env var with precedence over `RUST_LOG`
- [ ] Prometheus exposition on `/metrics` covering request counts/latencies per RPC, Raft proposals / leader changes / log lag, MVCC db size + revision + compacted-revision, lease counts, watcher counts, backend write-amplification, fsync latency
- [ ] **Cardinality discipline**: every metric's label set is documented; no user-controlled values (key names, lease IDs) ever become labels
- [ ] Per-RPC `#[instrument]` spans with stable field names; spans propagate through `spawn_blocking` correctly (capture `Span::current()` and re-enter inside the closure)
- [ ] **Tracing emits OTel-format spans natively** (`tracing-opentelemetry` bridge wired in `mango.server`'s init). The OTLP exporter is **off by default**; setting `MANGO_OTLP_ENDPOINT` enables it. The win over etcd is *format quality* (etcd's klog output isn't easily ingestible by OTel pipelines; ours is, out of the box) — not transport-on-by-default, which would mean every install logs "OTLP export failed: connection refused" forever in environments without a collector.
- [ ] Sample Grafana dashboard JSON committed to `dashboards/`, with a "mango vs etcd" comparison panel using the bench harness output
- [ ] **Continuous benchmark CI job**: every merge to `main` runs the Phase 5 / Phase 6 benches and uploads to a tracked baseline; regressions fail the next PR's CI
- [ ] Tests: hit the server, scrape `/metrics`, assert expected metric families exist with expected labels and bounded cardinality

## Phase 12 — Release engineering

Make mango installable.

- [ ] `cargo install`-able crates + binary publishing to crates.io (workspace publish ordered correctly)
- [ ] GitHub Release workflow: cross-compile `mango` and `mangoctl` for `x86_64-linux-gnu`, `aarch64-linux-gnu`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`; attach tarballs + checksums + signatures + SBOM
- [ ] Multi-arch `Dockerfile` and image push to GHCR
- [ ] Versioning: SemVer + `CHANGELOG.md` updated per release
- [ ] **On-disk format versioning**: a `data-dir/VERSION` file declares the on-disk format. Mango refuses to start against a newer-format dir or a too-old-format dir, with an actionable error; `mangoctl migrate <data-dir>` performs forward migrations. CI runs an upgrade matrix (N → N+1) on a populated cluster before every release.
- [ ] **Hot-restart / rolling-upgrade SLA**: a 3-node cluster can be rolling-restarted with no client-visible downtime; tested in CI by a workload runner that asserts zero failed Puts during the upgrade. This makes etcd's "informally works" into a tested guarantee.
- [ ] `mango cluster up --nodes 3` one-command local cluster bring-up in ≤ 10s (per dev-ergonomics axis)
- [ ] `0.1.0` release tag

## Phase 12.5 — Migration from etcd

Without a migration path, no etcd user can adopt mango. This phase makes
adoption real.

- [ ] `mangoctl import etcd-snapshot --file <etcd-snapshot.db>` reads an existing etcd v3 snapshot file and produces a mango snapshot. Tested against snapshots from etcd v3.4, v3.5, and v3.6.
- [ ] `mangoctl import etcd-live --endpoints ...` performs a one-shot copy from a live etcd cluster (range-scan + write into a fresh mango cluster). Documented as a planned-cutover tool, not a zero-downtime migration.
- [ ] Documentation in `docs/migrating-from-etcd.md` covering: data import, semantic differences (where mango is intentionally not wire-compatible), client rewrite guidance, rollback strategy.
- [ ] **Optional dual-write proxy** (stretch within this phase): a small binary that accepts etcd's gRPC, writes to both etcd and mango, returns etcd's response — for users who want to validate mango against production traffic before cutting over. Marked stretch because it requires implementing enough of etcd's `etcdserverpb` to be useful, which is real work.

## Phase 13 — Robustness testing

etcd has a famous robustness test suite (Jepsen-style: random failures +
linearizability checking). Mango must match it and exceed it. The Phase 5
DST harness already exists; this phase scales it up.

- [ ] Extend the Phase 5 deterministic simulator to model the full server (KV + Watch + Lease + Raft + storage), not just Raft alone
- [ ] Fault injector: drop / delay / duplicate / reorder messages, kill processes mid-fsync, partition the network with one-way / asymmetric / flaky links, corrupt individual disk pages, return `EIO` from any syscall, clock skew between nodes
- [ ] Linearizability checker (Porcupine-style or wrap an existing crate) over recorded histories; runs on every simulator trace
- [ ] Long-running fuzz harness: random workload + random faults; CI nightly job runs it for ≥30 minutes per seed across ≥10 seeds in parallel; failures auto-file a GitHub issue with the seed
- [ ] **Public Jepsen run published in CI**: real Jepsen test driving real mango binaries; results uploaded as a GitHub Pages site so claims about correctness are externally verifiable
- [ ] Document failure modes found and fixed in `docs/robustness/`

## Phase 13.5 — Conformance suite

Without a conformance suite, the post-1.0 stretch goals (embedded mode,
pluggable consensus) have no guardrail when they land. Pinning the
semantic contract now means every future implementation must pass the
same test gauntlet mango itself does.

- [ ] `crates/mango-conformance` — a standalone crate that runs a defined set of KV / Watch / Lease / Raft semantic assertions against any binary that speaks the mango `.proto`. Reference implementation = mango itself; pluggable consensus and embedded mode (stretch) MUST pass it before claiming compatibility.
- [ ] Test categories: KV linearizability, Watch event ordering and at-least-once delivery, Lease expiry timing within tolerance, Txn compare-and-swap semantics, Range pagination edge cases, error-shape stability.
- [ ] Conformance suite runs in CI against mango itself on every PR; passes are the merge gate for any future implementation claiming "mango-conformant."
- [ ] Public conformance report published alongside Jepsen results in Phase 13.

## Phase 14 — Performance push

A dedicated phase to chase the quantitative "beat etcd" numbers across
the board. Earlier phases set per-feature bench gates; this phase
optimizes against the integrated workload.

- [ ] Profile the integrated 3-node cluster under the YCSB workloads (A/B/C/D/E/F) and produce flamegraphs for each; commit them to `docs/perf/baselines/`
- [ ] Identify the top three CPU and top three latency hotspots; fix each in its own PR with before/after numbers
- [ ] **Zero-copy on the read path**: range responses serialize directly from the backend's mmap'd pages where the engine allows; no intermediate `Vec<u8>` copy. Expected to win largest on large-value workloads; near-noise on small values — both documented in the bench results.
- [ ] **io_uring backend on Linux, opt-in only, never default**. Required to demonstrate ≥ 2× improvement on the WAL-append microbench to ship at all. Documented kernel-version requirement (≥ 5.15 for the syscalls we use). Falls back to the async-io path with a startup warning if the kernel doesn't support it. **Active CVE policy**: `cargo-audit` config tracks io_uring-related kernel CVEs; we publish a security advisory and bump the documented minimum kernel version on any in-range CVE. Not enabled by default because (a) several cloud providers and security-conscious orgs disable io_uring at the kernel level, (b) it has an active CVE history, (c) the win is workload-shaped (large for small-IO, small for our batched-fsync WAL).
- [ ] **NUMA awareness** for multi-socket boxes (pin Raft tick / apply / serve threads sensibly)
- [ ] **Adaptive batching**: batch sizes auto-tune to maintain target p99 latency under varying load (etcd's batching is static)
- [ ] **Bounded-staleness follower reads** — **per-RPC opt-in only, never default**. Client passes `MaxStaleness(d)` on a `Range` request; the follower refuses to serve if its applied-index lag exceeds the bound, and the response carries the actual staleness measured at serve time. Documented as a *weakening of linearizability* in `docs/consistency.md`; explicit warning that operators must NOT enable it globally for systems (Kubernetes, controllers) that depend on linearizable etcd reads. Etcd has no first-class equivalent; this is a real differentiator if shipped responsibly.
- [ ] Final integrated bench, runner script `benches/runner/ycsb.sh`: YCSB-A,B,C,D,E,F on a 3-node cluster against the pinned etcd in `benches/oracles/etcd/` on the hardware sig in `benches/runner/HARDWARE.md`. **Realistic acceptance bar: mango wins on YCSB-A (write-heavy) and YCSB-F (read-modify-write) throughput by ≥ 1.3×; ties or wins on YCSB-B/C/D/E throughput within ±10%; wins on p99 latency on at least 4 of the 6 workloads at 50% saturation.** The two workloads where mango may lose are documented with the structural reason in `benches/results/v0.1.0.md`. ("Wins on every workload" is fan-fic; etcd has been profiled by experts for a decade. We win where we have a structural edge — write-heavy paths via Rust + pipelined Raft + better storage engine — and we're honest about read-only point-lookups at small values, which favor bbolt's mmap'd B+tree.)

## Phase 15 — Hardening

Production-grade means assume the worst about the network, the disk, and
the operator. This phase makes mango refuse to lose data even when those
assumptions are violated.

- [ ] **CI plumbing for the per-phase fuzz targets** added in Phases 2 / 5 / 6 / 10 (MVCC key codec, WAL record decoder, `.proto` decoders, config TOML parser, gRPC body decoders, snapshot decoder): nightly job runs each for ≥ 30 minutes per seed across ≥ 10 seeds in parallel, with persistent corpora under `fuzz/corpus/<target>/`. Failures auto-file a GitHub issue with the seed and the crashing input. Optional OSS-Fuzz integration as a follow-up.
- [ ] Audit pass: every state machine (Raft state transitions, lease state, MVCC visibility, watcher state) has property tests; backfill any phase that shipped without them (the Definition of Done says they're required, but this is the explicit verification step).
- [ ] **Disk corruption detection**: every backend write is checksummed (XXH3 or BLAKE3); reads verify; mismatch raises `CORRUPT` alarm and refuses to serve stale-checksum pages
- [ ] **Anti-entropy**: periodic cross-replica HashKV check; mismatch raises `CORRUPT` alarm and pinpoints the diverging key range
- [ ] **Memory profiling under load**: Massif / dhat profile, no leaks, RSS bounded under sustained load — ship a "running for 7 days at 5k writes/sec" stability report
- [ ] **Chaos test in CI** (weekly): real cluster, real network, random faults via toxiproxy or equivalent; failures block the next release. Runs for ≥ 1 hour and fails on any panic from non-test code (this is what mechanically enforces the north-star "no panics in steady state" claim).
- [ ] **Security review**: third-party (or at minimum, sensitive-data-auditor + security-reviewer subagents) review of the full surface before 1.0
- [ ] **Threat model document** in `docs/security/threat-model.md` covering the trust boundaries (client ↔ server, peer ↔ peer, operator ↔ disk) and mitigations

## Phase 16 — Web UI (editing core)

The localhost web UI is a real ergonomics win over etcd, which has only
`etcdctl`. Third-party tools like etcdkeeper exist but are barely
maintained and not first-party trustable. Mango ships the UI as a
first-class, supported, security-reviewed surface.

Builds on Phase 7.5's read-only browse mode and the same out-of-process
`mango-ui` binary architecture. Sequenced after Phase 8 (auth) so
destructive ops are gated by RBAC and after Phase 11 (observability) so
inline metric sparklines have data. Operations-side features (cluster
topology, member ops, alarms, maintenance, audit-log viewer, user/role
admin) live in **Phase 16.5**, which depends on Phases 9 and 10.

### Editing

- [ ] **Put / Edit value**: in-line value editor with revision check ("the value changed under you" detection via `If(mod_revision == X)` Txn). Confirmation required for overwrites of leased keys. **Audit-log entry on every Put attempt — successful, rejected by RBAC, rejected by `mod_revision` mismatch, or rejected by quota — per the Phase 8 audit-log contract** (failure cases matter for forensics; an attacker probing for permissions must leave a trace).
- [ ] **Delete key / range**: confirmation modal with the exact set of keys that will be deleted (range expanded to a preview list, capped at N for huge ranges). Two-click for single key. **For range delete, the modal displays the exact prefix string and the operator must type it character-for-character** (including any trailing `/`, no whitespace stripping); submit is disabled until the typed string equals the displayed string with `==` (not `eq_ignore_ascii_case`, not `trim`). Same audit-log-on-attempt rule as Put.
- [ ] **Txn builder**: visual builder for `Compare → Then/Else`; renders the equivalent `mangoctl txn` command alongside so users learn the CLI by clicking. Dry-run mode shows what would happen without committing. Audit-logged on attempt.
- [ ] **Bulk import / export**: upload a JSON or CSV of key/value pairs, preview, commit as a single Txn (subject to mango's max-txn-ops). Export the current range view as JSON or CSV. **Bounded data transfer**: any export endpoint enforces a server-side size cap (default 100 MB, configurable); above the cap the UI offers the equivalent `mangoctl` command instead. Streaming responses use HTTP chunked transfer-encoding; the UI frontend writes them to disk via the File System Access API (or falls back to `<a download>`) rather than buffering in memory.

### Live data

- [ ] **Watch streams over Server-Sent Events (SSE)**: subscribe to a key range, see events arrive in real time, pause / resume / clear, filter by event type. Transport: `text/event-stream`. The UI server holds one server-streaming Watch RPC per active UI watch and forwards events to the browser's `EventSource`. SSE chosen over WebSockets (Watch is one-way; WS adds proxy complexity) and over gRPC-Web (heavier client, harder ops story). **Reconnection uses SSE's `Last-Event-ID` header carrying the last-seen mango revision; the new Watch starts at `revision + 1` to give exactly the at-least-once contract Phase 3 promised.** SSE transport sends `:keepalive` comments every 15s (configurable) — distinct from the Phase 3 progress-notify ticker (revision pointer at the Watch protocol level). Backpressure: when the SSE writer's buffer fills, the underlying Watch RPC is canceled and a typed `slow_consumer_disconnect` event is sent before the connection closes. **Default 8 concurrent watches per UI session; server-wide cap of `max_concurrent_ui_sessions × 8` enforced as a single counter** to prevent the per-session limit from compounding into a global one. Session attempting to open watch #9 gets a 429 with `Retry-After`.
- [ ] **Live metrics inline**: every detail view (key, lease) shows a small sparkline of the relevant Prometheus metric inline. Driven by the same `/metrics` endpoint scraped over short polling, NOT a separate firehose stream.

### Auth + RBAC

- [ ] **Login flow** when cluster auth is enabled: same users / passwords / tokens as the cluster (Phase 8). The UI server enforces:
  - **TLS required for any non-loopback bind** that handles passwords; localhost bind allows cleartext but with a startup-banner warning matching Phase 7.5.
  - **Rate limit**: ≤ 5 login attempts per minute per IP and per username; exponential backoff after 3 failures; user account locked after 10 consecutive failures (admin-resettable via `mangoctl user unlock`).
  - **Password handling**: `autocomplete="current-password"`; never logged; never returned in any error message; constant-time server-side comparison.
  - **Login-form CSRF**: token issued on the GET, validated on the POST (the synchronizer-token-bound-to-session pattern can't apply to the login itself because there's no session yet).
  - **Session cookies**: `HttpOnly; Secure; SameSite=Strict; Path=/` (the `Secure` attribute is gated on TLS being configured — on cleartext localhost it is omitted with a startup-banner warning).
- [ ] **Server-side session revocation list in the Raft state machine** (so it survives leader change). UI **access tokens are short-lived** (≤ 5 min default; 1 h refresh token max) and carry a session-id claim. Every backend request consults the revocation set at the auth interceptor — *not* just at the resource check. "Log out everywhere" adds the user's session-ids; admin "revoke user" adds all of that user's active session-ids atomically with the user-removal. Tested: revoke admin user, assert their in-flight UI session can no longer make any mutating call within one revocation-propagation tick.
- [ ] **RBAC enforcement in the UI mirrors the backend** — and the UI treats the button-disabled state as **cosmetic and best-effort**; the authoritative check is the backend on every request. Three property tests:
  - **(a)** every UI mutating action that the backend would reject is also disabled in the UI for that user (UI-too-permissive guard);
  - **(b)** every action enabled in the UI for a user is accepted by the backend (UI-too-restrictive guard);
  - **(c) revocation race**: a user whose role is revoked mid-session has every cached-permitted button rejected by the backend within the access-token TTL (≤ 5 min), regardless of UI cache state. This is the test that catches the dangerous case.

### Hardening (the UI is a fresh attack surface)

- [ ] **Same security review** as the gRPC surface in Phase 15: sensitive-data-auditor + security-reviewer subagents pass.
- [ ] **CSRF tokens on every mutating endpoint** using the synchronizer-token-bound-to-session pattern; cookie attributes per the login-flow item above.
- [ ] **Explicit Content-Security-Policy**: `default-src 'self'; script-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'; upgrade-insecure-requests`. The exact header is asserted in an integration test. **No third-party CDNs** — everything served from the `mango-ui` binary.
- [ ] **Stored-XSS test**: write a key whose value is `<script>alert(1)</script>`, render it in every view that displays values, assert no script execution (the script tag must be escaped as `&lt;script&gt;...`).
- [ ] **`cargo fuzz` target** on the UI's HTTP request parsers (per Reviewer's contract: parser fuzz lives where the parser does; CI plumbing in Phase 15).
- [ ] **DoS knobs**: max request body size, max concurrent UI sessions per IP, rate-limit per session, max response body size matching the bounded-data-transfer cap above. Defaults documented in `docs/ui-deployment.md`; tested with a misbehaving-client harness.
- [ ] **Threat model addendum** in `docs/security/threat-model.md` covering the UI specifically (browser ↔ UI server, UI server ↔ mango server, operator ↔ stored values rendered in browser).

### Bench + acceptance

- [ ] Page load ≤ 200ms p99 for every editing view against a 10M-key cluster. Watch event delivery to the UI within 100ms p99 of arrival at the server. Numbers committed to `benches/results/phase-16/ui-editing.md`.
- [ ] **Usability bar against the `etcdctl` baseline workflow** (no external UI oracle exists — etcdkeeper is unmaintained, etcd ships no UI; the comparison is "UI vs SSH-and-etcdctl"):
  - "Find a key by prefix and view its value": **≤ 3 clicks, ≤ 2s first-paint** after typing the prefix, vs `etcdctl get --prefix <prefix>` baseline of ~5s including SSH.
  - "Edit a key's value with optimistic concurrency": **≤ 4 clicks, ≤ 1s commit feedback**, vs the multi-step `etcdctl get + edit + put --prev-kv` flow.
  - All measured in `docs/comparisons/web-ui.md` against a recorded screencast for reproducibility.
- [ ] Full Playwright (or `fantoccini` if we go pure Rust) end-to-end suite for the editing surface: login, browse, edit with revision check, delete with prefix-confirm, watch a key, txn builder dry-run + commit. Runs against an in-process 3-node cluster in CI.

## Phase 16.5 — Web UI (operational console)

The operations-side surface of the UI: cluster topology, member ops,
alarms, maintenance, user / role admin, audit-log viewer. Sequenced
after Phase 16 (editing core) and depends on Phase 9 (membership) for
the topology view, Phase 10 (maintenance) for alarms and snapshot ops,
and the Phase 16 auth + revocation infrastructure for admin gating.

Split out from Phase 16 because the editing core and the operations
console are two bounded surfaces with separate dependency trees and
~7-9 items each — keeping them in one phase would have been six
sub-phases stapled together and would have blocked Stretch goals on
the largest single phase in the roadmap.

### Cluster topology and member operations

- [ ] **Cluster topology view**: live diagram of members, leader / follower / learner roles, raft index per member, replication lag, link health. Refreshes on a 2s timer (configurable; documented as eventually consistent — never claim leader from this view).
- [ ] **Member operations** (admin-only, audit-logged on attempt): add member, add learner, promote learner, remove member. Confirmation flows match `mangoctl member` semantics. Refuses operations that would lose quorum with a clear error.

### Alarms and maintenance

- [ ] **Alarms panel**: live list of `NOSPACE` / `CORRUPT` / `WAL_FULL` alarms; one-click `disarm` (admin-only, audit-logged on attempt).
- [ ] **Maintenance panel**: trigger snapshot save (downloads the snapshot file to the operator's browser, subject to the same bounded-data-transfer cap from Phase 16; falls back to `mangoctl snapshot save` for huge clusters), trigger compaction at a chosen revision, trigger online defrag. Each operation shows progress and audit-logs the trigger. **Snapshot download streams via the File System Access API** so multi-GB snapshots don't OOM the browser.

### Admin

- [ ] **User / role management UI** (admin-only): list users, create / delete user, grant / revoke role, list roles, create role with key-range permissions, unlock locked-out users. Mirrors the Phase 8 `Auth` gRPC service exactly. Admin "revoke user" atomically adds all that user's active session-ids to the Phase 16 revocation set (per S4 / Phase 16 session-revocation item).
- [ ] **Audit-log viewer** (admin-only): paginated, searchable view of the Phase 8 hash-chained audit log. **Tamper detection runs server-side and async**: the UI server maintains a "verified up to entry N" pointer advanced by a background task; the viewer shows the current verified-up-to position and offers an explicit "Verify newer entries now" action that runs incrementally without blocking the page. Read-only — the audit log is append-only by design. Tested with a 10M-entry audit log on the bench rig (verifies in ≤ 30s, page renders in ≤ 200ms regardless of chain length).

### Bench + acceptance

- [ ] Page load ≤ 200ms p99 for every operations view against a 10M-key cluster + 10M-entry audit log. Numbers committed to `benches/results/phase-16.5/ui-ops.md`.
- [ ] **Usability bar against the `etcdctl` baseline workflow** for ops tasks:
  - "Add a member to the cluster": **≤ 5 clicks** with a confirmation modal, vs the equivalent `mangoctl member add` invocation.
  - "Read the audit log for a specific user": **≤ 3 clicks + filter**, vs grep over `data-dir/audit.log` baseline.
  - "Disarm a NOSPACE alarm": **≤ 2 clicks**, vs `mangoctl alarm list` + `mangoctl alarm disarm`.
  - All measured in `docs/comparisons/web-ui.md` against a recorded screencast for reproducibility.
- [ ] Full Playwright / `fantoccini` end-to-end suite for the operations surface: topology view, member add + promote, alarm disarm, snapshot save (small-data path), audit-log search + verify-newer. Runs against an in-process 3-node cluster in CI.

---

## Stretch (post-1.0)

These live below the line until we cut a 1.0. Promote them above the line by editing this file when a phase becomes the next priority.

- [ ] Watch progress + fragment options matching etcd's behavior under huge updates
- [ ] gRPC gateway for HTTP+JSON access
- [ ] gRPC proxy (read coalescing, watch fan-out, lease keepalive proxying)
- [ ] Embedded mode (`mango` as a Rust library, not a separate process — like etcd's `embed` package)
- [ ] Second-language client (Go or Python) generated from our `.proto`
- [ ] Hardware-accelerated TLS (rustls on aws-lc-rs)
- [ ] Disaster-recovery drill docs
- [ ] Cross-region async replication (etcd has no first-class story here)
- [ ] Tiered storage (hot revisions in memory / NVMe, cold revisions on object storage)
- [ ] Pluggable consensus (Paxos / EPaxos behind the same `Consensus` trait, for benchmarking)
