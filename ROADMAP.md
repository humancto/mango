# mango roadmap

A ground-up Rust port of [etcd](https://github.com/etcd-io/etcd). Mango is
**not** wire-compatible with etcd — we own our `.proto` files and design a
clean Rust-native API. etcd is the reference implementation we study; we are
not bound by its Go-isms.

## North star (non-negotiable)

**Mango is a mature, production-grade distributed KV store that beats etcd
on every axis we care about.** Not a toy port. Not a learning exercise. Not
"good enough." Every plan, every PR, every architectural decision is judged
against the bars below. If a change merely matches etcd, that is a
regression relative to the goal — find the win.

### What Rust gives us that Go etcd cannot

Mango is not "etcd, but rewritten." It is etcd's problem space attacked with
a language whose primitives lift specific etcd footguns out of existence
_at compile time_. These are the structural advantages every plan should
think in terms of:

- **No GC.** Etcd's tail latency is shaped by Go's stop-the-world GC pauses,
  which are small but real and _unbounded_ under memory pressure. Rust has
  deterministic destruction (RAII), so tail latency is bounded by what we
  do in our own code, never by a runtime collector. This is the single
  largest structural lever for the p99 bars below.
- **Memory safety without a runtime.** Use-after-free, double-free, data
  races on shared memory, buffer overflows — _not possible_ in safe Rust.
  CVEs in this class become impossible-by-construction, not
  impossible-after-careful-review.
- **Fearless concurrency via `Send` / `Sync`.** Sharing data across threads
  without proper synchronization is a _compile error_, not a Friday-night
  page. Etcd's Go race-detector catches these dynamically and only when a
  race actually occurs in a test run; Rust catches the _category_ before
  the binary exists.
- **Zero-cost abstractions.** Iterators, futures, generics, traits all
  compile to the same code we'd hand-write. We pay nothing for clean APIs.
- **Explicit failure as a value, not an exception.** `Result<T, E>` makes
  every fallible operation visible at the call site. Etcd has Go's error-
  return convention but no compiler enforcement that errors are handled;
  Rust enforces it.
- **Async without a hidden runtime.** We pick the executor (Tokio); we know
  exactly which thread does what; we can pin Raft tick / apply / serve
  threads explicitly (Phase 14 NUMA item). Etcd is at the mercy of Go's
  goroutine scheduler.
- **`unsafe` is rare, named, and audited.** `unsafe_code = "forbid"` at the
  workspace root means there are _zero_ unsafe blocks unless a module
  explicitly opts in. Every opt-in module is named, has a `# Safety`
  comment block on every `unsafe` block, is Miri-tested. Etcd has neither
  the concept nor the audit trail.
- **Cargo-native supply chain hygiene.** SHA-pinned actions, `cargo-deny`,
  `cargo-audit`, `cargo-vet`, SBOM via `cargo-cyclonedx`. The Go ecosystem
  has analogues; Rust's are more complete and CI-native.

These are the _mechanism_. The bars below are the _measurement_. Every
plan's first paragraph should name which mechanism it leverages and which
bar it moves.

### The bars (each axis has a comparison oracle, a measurable threshold, and a named test that gates merge)

The comparison oracle for every "vs etcd" claim is the **pinned etcd binary**
in `benches/oracles/etcd/` (etcd v3.5.x; exact version pinned per release in
`benches/oracles/etcd/VERSION`), running on the **same hardware class**
described in `benches/runner/HARDWARE.md`, driven by the **bench runner
scripts** under `benches/runner/`. "etcd's published numbers" is never an
acceptable oracle; we run the comparison ourselves.

Every bar lists the **named test** that gates it. If a PR claims to move a
bar, the named test must exist (or be updated by the same PR) and must
pass on the comparison-oracle hardware. PRs that claim a bar without a
named test are auto-`REVISE`.

1. **Performance — Blazing fast.** Rust's no-GC + zero-cost abstractions
   are the structural lever.
   - 1KB Put throughput on a 3-node loopback cluster: **≥ 1.5× etcd's**.
     _Test:_ `benches/runner/raft.sh` (Phase 5).
   - p99 client latency at 50% of mango's saturation: **≤ 0.7× etcd's**
     at the same absolute QPS.
     _Test:_ `benches/runner/grpc.sh` (Phase 6).
   - Resident set size at idle (3-node cluster, empty data dir):
     **≤ 0.7× etcd's**.
     _Test:_ `benches/runner/idle-rss.sh` (Phase 6).
   - Cold start (process exec → first successful Put accepted):
     **≤ 0.7× etcd's**.
     _Test:_ `benches/runner/cold-start.sh` (Phase 6).
   - Leader-failover-to-quorum-write time after `SIGKILL` of the leader:
     **≤ 0.7× etcd's**.
     _Test:_ `benches/runner/failover.sh` (Phase 5).
2. **Concurrency & parallelism.** Rust's `Send` / `Sync` + Tokio's
   structured concurrency mean we get correct parallel scaling without
   the GC scheduler artifacts that bound etcd's per-node throughput.
   Per-core scaling is workload-shaped: Raft serializes the apply path
   on the leader (the consensus protocol, not the runtime, sets the
   ceiling), so write-heavy workloads top out lower than read-heavy.
   We commit to per-workload bars rather than a single number that
   would be either fan-fic on writes or a giveaway on reads.
   - **Read-only workload** (100% Range, MVCC snapshot reads, no Raft
     serialization): throughput at 16 cores **≥ 14× throughput at 1
     core** (linear scaling is achievable on the read path). Etcd
     typically delivers ~10× here.
     _Test:_ `benches/runner/per-core-scaling.sh --workload=read-only`
     (Phase 6).
   - **Mixed workload** (50/50 read/write): throughput at 16 cores
     **≥ 8× throughput at 1 core** (the win over etcd's ~5× comes
     from no-GC tail + parallel reads via MVCC snapshots while writes
     pipeline through Raft).
     _Test:_ `benches/runner/per-core-scaling.sh --workload=mixed`
     (Phase 6).
   - **Write-heavy workload** (90% writes): throughput at 16 cores
     **≥ 4× throughput at 1 core** (apply is fundamentally serial in
     Raft; the win over etcd's ~3× comes from pipelined replication
     and tighter batching, not from getting around the protocol).
     _Test:_ `benches/runner/per-core-scaling.sh --workload=write-heavy`
     (Phase 6).
   - Zero deadlocks under fuzzed concurrent workloads. **Enforcement
     is layered, not single-mechanism**: compile-time,
     `clippy::await_holding_lock` catches the common 'lock held across
     await' pattern; test-time, `loom`-based model-checking tests for
     every shared-state primitive catch lock-cycle and channel-deadlock
     patterns within the modeled state space; runtime, the
     `cargo-nextest` per-test-class timeout (Phase 0 watchdog item)
     catches anything that escapes both. _The compile-time check
     alone does not guarantee deadlock-freedom_; the layered approach
     does, within the modeled state space.
     _Tests:_ per-crate `tests/loom/*.rs`; `.config/nextest.toml`
     timeouts (Phase 0 + Phase 5 onwards).
   - Lock-poisoning is unrepresentable in our code: `parking_lot`
     mutexes (no poisoning) or `tokio::sync` (no poisoning); use of
     `std::sync::Mutex` / `RwLock` in non-test code is rejected by
     `clippy::disallowed_types` (the correct mechanism — `cargo-deny`
     bans crates, not stdlib type uses).
     _Test:_ `clippy.toml` `disallowed-types` config (Phase 0).
3. **Reliability.** Graceful degradation, no thundering herds, no
   cascading failures, bounded recovery time. Rust's typed errors +
   the absence of unchecked exceptions mean every failure mode is
   enumerated.
   - Follower restart against a 10M-revision cluster causes **≤ 1.2×
     the steady-state network ingress on the leader for ≤ 30s** (no
     thundering herd).
     _Test:_ `benches/runner/follower-restart.sh` + assertion in
     `tests/reliability/follower_catchup.rs` (Phase 5).
   - Zero leader changes during a 1-member-add-then-promote cycle on a
     healthy 3-node cluster.
     _Test:_ `tests/reliability/membership_change.rs` (Phase 9).
   - Single-node disk-full scenario: server enters read-only mode,
     raises `NOSPACE`, **never crashes, never corrupts**, recovers
     cleanly when space is freed.
     _Test:_ `tests/reliability/disk_full.rs` + `tests/chaos/disk_eio.rs`
     (Phase 1 + Phase 15).
   - Slow client cannot stall the server: bounded per-stream send
     buffer with documented disconnect policy; zero memory growth
     under a misbehaving-client harness.
     _Test:_ `tests/dos/slow_loris.rs`, `tests/dos/oversized_frames.rs`
     (Phase 6).
   - Recovery from any single-node failure (process kill, kernel
     panic, disk yank) within **≤ 0.7× etcd's recovery time** to the
     same workload. **Levers**: smaller WAL records (no proto-wire
     overhead in WAL — just the state-machine command), faster
     cold-cache reads (better storage engine), no Go-runtime startup
     cost (~50ms saved). If a PR's measured recovery time exceeds
     0.7×, the PR must either find more lever or document why the bar
     should be revised. _Distinct from Performance bar #5_, which is
     specifically leader-`SIGKILL`-to-quorum-write; this Reliability
     bar covers any-node any-failure-mode and is verified by
     `tests/chaos/single_node_failure.rs`, not the failover bench.
     _Test:_ `tests/chaos/single_node_failure.rs` (Phase 15).
4. **Correctness — Distributed-systems grade.** Linearizability is
   the load-bearing claim; we verify it externally.
   - Public Jepsen run published in CI (Phase 13), results uploaded
     to a GitHub Pages site so the claim is externally checkable.
     _Test:_ `tests/jepsen/` (Phase 13).
   - Deterministic simulator (Phase 5 onwards) replays every reported
     bug from a seed; every fix lands with the seed in the regression
     suite.
     _Test:_ `tests/simulator/regression/` (Phase 5).
   - Property tests for every state machine — Raft transitions, MVCC
     visibility, lease state, watcher state — under proptest with
     shrinking; the simulator runs every property test under a panic
     hook that fails on any panic from non-test code.
     _Tests:_ `tests/properties/<state-machine>.rs` per crate.
   - Linearizability checker (Porcupine-style) over every recorded
     simulator history.
     _Test:_ `tests/linearizability/` (Phase 13).
5. **Safety — Memory-safe by construction.** Use-after-free, data
   race, double-free, buffer overflow: not possible in safe Rust. We
   enforce the rest mechanically.
   - `unsafe_code = "forbid"` workspace-wide except in audited, named
     modules with documented invariants and a `# Safety` comment block
     on every `unsafe` block. Per-PR sign-off cites Miri output
     (`MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly miri test
-p <crate>`) or a written justification for why Miri doesn't
     apply (e.g. FFI).
     _Test:_ CI step counts `unsafe` blocks across the workspace and
     fails if it grows without an approving label.
   - **No panics in steady state**. **Mechanically enforced in three
     layers**: per-PR via clippy lints (Phase 0 — denies `unwrap_used`,
     `expect_used`, `panic`, `unimplemented`, `todo`,
     `indexing_slicing`, `arithmetic_side_effects`,
     `cast_possible_truncation`, `cast_sign_loss` in non-test code);
     per-PR via Phase 13 simulator's panic-hook test (catches panics
     under seeded fuzz); per-release via Phase 15's continuous chaos
     run (catches panics under sustained real-cluster load). All three
     layers must be green; any layer failing is auto-`REVISE`. Plus
     `[profile.release] overflow-checks = true` so production builds
     panic on arithmetic overflow rather than silently wrapping.
     **Continuous-chaos policy by release type**: scheduled major /
     minor releases require a **≥ 7-day-clean signal** from the
     continuous chaos run on `main` (the runner runs continuously; the
     release commit must inherit a 7-day-clean window). **Hotfix
     releases** (security or correctness regression) require only the
     **1-hour weekly chaos regression gate** plus the per-PR layers —
     waiting 7 days for a CVE patch is unacceptable. The next
     scheduled release post-hotfix re-establishes the 7-day-clean
     signal before cutting.
     _Tests:_ clippy config (Phase 0); `tests/simulator/panic_hook.rs`
     (Phase 13); `tests/chaos/long_running.rs` (Phase 15) + the
     continuous-chaos-runner workflow.
   - Every public fallible op returns a typed crate-local `Error`
     enum; `Box<dyn Error>` in a public API is auto-`REVISE`.
     _Test:_ `cargo public-api --diff` (Phase 0) plus a CI grep
     against `Box<dyn` in `pub fn` signatures.
   - Miri runs on a curated subset of tests (the ones touching unsafe
     blocks or pointer arithmetic) on every push to `main` and once
     per PR night.
     _Test:_ `.github/workflows/miri.yml` (Phase 15).
6. **Security — Defense-in-depth.** Memory safety is necessary but
   not sufficient. Supply chain, network, disk, and operator are all
   trust boundaries.
   - **Supply-chain**: SHA-pinned actions; `cargo-deny` (license,
     advisory, duplicate-version, ban-git-deps); `cargo-audit`
     (RustSec); `cargo-vet` (transitive-dep audits); SBOM via
     `cargo-cyclonedx` published with every release.
     _Tests:_ `.github/workflows/supply-chain.yml` (Phase 0).
   - **Cryptographic correctness**: TLS via `rustls` (memory-safe TLS,
     formally-verified crypto primitives via `*ring*` / `aws-lc-rs`);
     never `openssl` (CVE history, C codebase). All key material
     handled via `secrecy::Secret<T>` so it's zeroized on drop.
     _Tests:_ `tests/crypto/zeroize.rs`; `cargo-deny` ban list
     prohibits `openssl-sys`.
   - **Disk-corruption detection**: every backend write is checksummed
     (XXH3 or BLAKE3); reads verify; mismatch raises `CORRUPT` and
     refuses to serve stale-checksum pages.
     _Test:_ `tests/security/disk_corruption.rs` (Phase 15).
   - **DoS hardening at the gRPC layer**: max-message-size, max-concurrent-
     streams, http2 keepalive, per-conn rate limit. Defaults documented
     and tested with a misbehaving-client harness.
     _Test:_ `tests/dos/grpc_hostile_client.rs` (Phase 6).
   - **Threat model** in `docs/security/threat-model.md` covering
     client ↔ server, peer ↔ peer, operator ↔ disk; every threat has
     a named mitigation and a test (or a documented "accepted risk"
     justification).
     _Test:_ `docs/security/threat-model.md` is reviewed by
     `sensitive-data-auditor` + `security-reviewer` subagents before
     1.0 (Phase 15).
   - **Side-channel awareness**: constant-time comparison for all
     credential / token / hash-chain checks; `subtle` crate enforced
     via clippy custom-lint; CI test for non-constant-time use of
     `==` on `&[u8]` in security-relevant modules.
     _Test:_ `tests/security/constant_time.rs` (Phase 8 + 15).
7. **Large-scale distributed.** Etcd is famously stretched at large
   cluster size (≥ 7 voters), large dataset (multi-GB), and large
   watcher counts. We target two scale tiers in v1.0: the single-Raft-
   group Tier 1 that everything else in the roadmap delivers, and a
   read-scale-out Tier 2 (Phase 14.5) that uses learner replicas +
   client-side caching to deliver ≥ 1M ops/sec on read-heavy workloads
   without changing the write path. Multi-shard "Tier 3" (the
   FoundationDB / TiKV / Cockroach trick for ≥ 10M ops/sec) is **not
   on the roadmap** — it's a separate, multi-year engineering project
   that we'd only justify by real user demand after v1.0. See the
   README's positioning table for where mango fits relative to etcd,
   FoundationDB, and DynamoDB.

   **Tier 1 — single-Raft-group, write-bounded by quorum** (the etcd
   regime, plus Rust + better data structures):
   - **5-voter and 7-voter** clusters tested in CI with the same
     workload as 3-voter; throughput delta documented and bounded
     (Raft has fundamental quorum-size cost; we minimize it via
     pipelined replication).
     _Test:_ `benches/runner/cluster-size.sh` (Phase 5 + 9).
   - **100k concurrent watchers** on a single server: RSS bounded at
     **≤ 100 KB per watcher** (so ≤ 10 GB total at the 100k case —
     this is the realistic memory footprint of 100k tonic streams +
     bounded per-watcher channel buffers + per-task state, not
     "constant overhead"), CPU **≤ 50% of one core** for the watcher-
     management work (excluding actual event delivery), p99 event
     delivery latency **≤ 100ms** under a 1k-events/sec write
     workload.
     _Test:_ `benches/runner/watcher-scale.sh` (Phase 3 + 14).
   - **8 GB on-disk dataset** with **≥ 100M revisions**: range
     queries, compaction, snapshot, defrag all complete within the
     same per-op latency bounds as a small dataset.
     _Test:_ `benches/runner/large-dataset.sh` (Phase 2 + 14).
   - **Long-running stability**: 7-day continuous run at 5k writes/sec
     ships a stability report (Phase 15).
     _Test:_ `tests/chaos/long_running.rs` (Phase 15).

   **Tier 2 — read-scale-out within a single Raft group** (the lever
   no etcd-shaped system has used; ships in Phase 14.5 pre-1.0). The
   bar is split by read mode because the math is materially different:
   ReadIndex routes through the leader (which serializes), bounded-
   staleness reads stay local on the learner.
   - **Tier 2a (bounded-staleness reads): 5-voter + 5-learner cluster
     delivers ≥ 1M ops/sec** on an 80/20 read/write mix at 1KB values
     on the canonical `benches/runner/HARDWARE.md`. Reads are served
     locally on the learner under a documented `MaxStaleness(d)`
     bound; writes stay quorum-bound at Tier 1 ceilings. Roughly
     **up to ~2× over etcd's serializable-read ceiling** of
     ~500K-1M ops/sec — bounded-staleness is etcd's stronger suit
     (it's already serving reads off followers locally), so the
     multiplier here is naturally smaller than on linearizable.
     _Test:_ `benches/runner/read-scale-out.sh --read-mode=bounded-staleness` (Phase 14.5).
   - **Tier 2a 95/5 mix: ≥ 1.5M ops/sec** on the same topology
     (~1.5-3× over etcd serializable, depending on where in the
     ~500K-1M etcd range the comparison lands on the same hardware).
     This is the workload K8s-class operators actually run; calling
     it out separately so the bar isn't perceived as cherry-picked
     at 80/20.
     _Test:_ `benches/runner/read-scale-out.sh --read-mode=bounded-staleness --mix=95/5` (Phase 14.5).
   - **Tier 2b (linearizable ReadIndex reads): ≥ 600K ops/sec** on the
     same 80/20 mix and topology. The honest ceiling — every read
     pays a leader-confirm round trip, even with ReadIndex batching.
     Roughly **5-10× etcd's published per-cluster ReadIndex ceiling
     of ~50-150K reads/sec**, with strong consistency preserved. The
     "5-10×" range reflects the spread in published etcd numbers
     across hardware generations; mango's bar is fixed at ≥ 600K, the
     comparison ratio is informational.
     _Test:_ `benches/runner/read-scale-out.sh --read-mode=linearizable` (Phase 14.5).
   - **Read-throughput scaling with learner count**: throughput at N
     learners is **≥ 0.7 × N × (single-voter read throughput), up to
     N = 7 learners** (bounded-staleness mode). Past N = 7 the leader
     becomes the membership-and-replication bottleneck even without
     ReadIndex serialization. Going further requires multi-shard,
     which is explicitly post-1.0 and out of scope.
     _Test:_ `benches/runner/learner-scale.sh` (Phase 14.5).
   - **Hot-key client cache hit rate ≥ 90%** on the typical "watch one
     key, read it repeatedly" pattern, with watch-driven invalidation
     correctness verified by **multiple property tests** (server-cache
     equivalence at the cached revision, reconnect-with-compaction,
     event-ordering across reconnect — see Phase 14.5).
     _Test:_ `tests/cache/watch_invalidation.rs` and siblings (Phase 14.5).
   - **Process commitment**: if any of the Tier 2 bars above slips at
     bench time, the bar gets restated **in the same release** as the
     evidence — no quiet downgrade. Phase 12 includes a design task
     for a CI gate that enforces this mechanically; until that task
     ships, the commitment is reviewer-enforced (the rust-expert
     auto-`REVISE`s any release-tag PR whose README + bar #7 +
     `benches/results/phase-14.5/*.md` headline numbers don't
     triple-match). Calling this out honestly: a mechanically
     enforced gate is the goal, but writing "mechanism" without
     having designed the schema, metric-map, tolerance policy, and
     prose-coverage rules was itself an instance of the failure mode
     the commitment is meant to prevent. The design task is what
     turns the commitment into a mechanism; it is not the mechanism.

8. **Operability.** Production-grade defaults; predictable behavior at
   the limits.
   - Every metric documented in `docs/metrics.md` with declared label
     set and cardinality bound; CI test scrapes `/metrics` and asserts
     each family's distinct label-value count stays below its declared
     bound under a 10k-key workload.
     _Test:_ `tests/observability/metric_cardinality.rs` (Phase 11).
   - `mango --check-config <path>` validates the entire config and
     exits non-zero on any conflict; tested against a malformed-
     config corpus.
     _Test:_ `tests/config/check_config.rs` (Phase 6).
   - All structured logs use stable `mango.*` tracing target names;
     `tracing-opentelemetry` bridge wired natively so logs are
     OTel-ingestible out of the box.
     _Test:_ `tests/observability/log_targets.rs` (Phase 11).
9. **Developer ergonomics.** Mango should be pleasant to contribute to
   and pleasant to operate.
   - CI cold ≤ 5 min, warm ≤ 90s (CI-asserted via job duration check
     starting Phase 11).
     _Test:_ `.github/workflows/ci-duration-budget.yml` (Phase 11).
   - `mango cluster up --nodes 3` brings up a working local cluster in
     ≤ 10s and prints connection info.
     _Test:_ `tests/cli/cluster_up.rs` (Phase 12).
   - `cargo doc --open` for `mango-client` shows zero `prost`/`tonic`
     types in the public API surface (CI-checked via a doc-extracted
     allowlist).
     _Test:_ `tests/api/no_proto_leakage.rs` (Phase 7).
   - `cargo public-api --diff` clean against the previous tagged release
     unless the PR is tagged `breaking-change`.
     _Test:_ `.github/workflows/public-api-diff.yml` (Phase 0 warn,
     Phase 6 gate).
10. **Storage efficiency.** Smaller, faster compaction, no read stalls.
    - On-disk size after the same workload: **≤ 0.7× etcd's** (with
      mango's default compression on, etcd's default off — both
      defaults).
      _Test:_ `benches/runner/disk-size.sh` (Phase 1 + 2).
    - Compaction: bounded CPU (≤ 25% of one core during compaction)
      and read p99 during compaction within **1.5× of steady-state
      read p99**.
      _Test:_ `benches/runner/compaction-impact.sh` (Phase 2).

### Cross-cutting principle: **Tested or it doesn't exist.**

Every bar above lists a named test. The test must exist (committed,
runnable locally, runnable in CI) before the corresponding feature is
considered "done." A bar without a named test is _not a bar_; it's
marketing. The Reviewer's contract enforces this — see below.

When two approaches both work, pick the one that wins on at least one axis
without losing on the others. When a winning-on-X approach loses on Y,
document the trade-off explicitly in the plan and get the expert agent to
acknowledge it. **The expert agent treats "this is fine" as failure; the
bar is "this beats etcd."**

## Working rules

- One checked item per PR. Small, atomic, mergeable. No mega-PRs.
- Every plan declares which of the ten north-star axes the item moves on,
  names the specific bar within that axis, names the verifying test, and
  records the measured number. "Doesn't move any axis" is a valid answer
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

## Crate inventory & non-rolled stack

**Default: use the proven crate. Hand-rolling requires an ADR.**

Every load-bearing subsystem in mango has a Rust ecosystem crate that is
already battle-tested at scale by another distributed system. Adopting
those crates removes years of bug-finding from the critical path; the
single largest execution risk for mango is hand-rolling something the
ecosystem has already solved. The table below is the **default stack**.
A PR that proposes hand-rolling, replacing, or adding an alternative to
any row requires an ADR in `.planning/adr/` justifying the deviation
against the same five questions the rust-expert applies (correctness,
maintenance burden, audit surface, performance evidence, supply chain).
The auto-`REVISE` triggers list (above) treats "rolled own X" without
an ADR as a blocker.

The reference systems column is the production user whose existence is
the evidence the crate is good enough. If the reference user
discontinues the crate, the row gets re-evaluated.

| Subsystem                                       | Crate (default)                                                      | Reference system                                                                                                                                                                          | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| ----------------------------------------------- | -------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Async runtime                                   | `tokio`                                                              | ~everyone in async Rust                                                                                                                                                                   | The default; `glommio` / `monoio` only with an ADR justifying thread-per-core.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| gRPC server + client                            | `tonic` + `prost`                                                    | Linkerd2-proxy, Databricks, Cloudflare                                                                                                                                                    | The de-facto stack. No alternative considered.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| HTTP (UI server)                                | `axum` + `tower-http`                                                | Cloudflare, Fly.io                                                                                                                                                                        | Standard tokio-stack HTTP.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| Raft consensus                                  | `tikv/raft-rs`                                                       | TiKV (operates raft-rs at multi-PB scale in PingCAP customer deployments)                                                                                                                 | See Phase 5 ADR — `openraft` is the documented alternative; hand-roll requires an ADR demonstrating both crates fail one of the four Phase 5 criteria.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| Storage engine                                  | `redb`                                                               | iroh, several Rust DBs                                                                                                                                                                    | Pure-Rust COW B-tree, design-inspired by bbolt; ~10kloc auditable surface. **Unsafe stance**: limited, audited `unsafe` concentrated in the mmap layer (the `unsafe` is structural — projecting struct types over a memory-mapped file requires it; bbolt has the same shape). Tested under Miri by upstream. Mango's workspace stays `unsafe_code = "forbid"`; redb's transitive `unsafe` is accepted as the storage row's cost and counted in the Phase 0.5 `cargo-geiger` baseline. Documented alternatives: `rust-rocksdb` for write-heavy LSM workloads where the C++ blast surface is acceptable; **`fjall`** as the pure-Rust LSM alternative (active development, smaller production track record). Hand-roll requires an ADR. See Phase 1 ADR. |
| TLS                                             | `rustls` + `rustls-platform-verifier`                                | Cloudflare, AWS, Linkerd                                                                                                                                                                  | `openssl-sys` is banned via `cargo-deny` (security-axis bar). Pure Rust, formally-verified primitives via `aws-lc-rs` / `*ring*`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| Tracing                                         | `tracing` + `tracing-subscriber` + `tracing-opentelemetry`           | Linkerd, Databricks                                                                                                                                                                       | Standard observability stack.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| Metrics                                         | `metrics` + `metrics-exporter-prometheus`                            | Grafana, others                                                                                                                                                                           | Facade-based so backend swaps are cheap; OTel bridge already in roadmap.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| Concurrency primitives                          | `parking_lot` (sync), `tokio::sync` (async), `arc-swap`, `crossbeam` | Bevy, polars, TiKV                                                                                                                                                                        | `std::sync::Mutex` / `RwLock` banned by `clippy::disallowed_types` (Phase 0).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| Hashing (DoS-resistant)                         | `ahash` (per-process random seed)                                    | Bevy, polars                                                                                                                                                                              | Already specified in Phase 2 sharded `KeyIndex`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| Property testing                                | `proptest`                                                           | Tokio, sled, redb                                                                                                                                                                         | The default; `quickcheck` only for legacy interop.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| Fuzzing                                         | `cargo-fuzz` (libFuzzer) + `bolero`                                  | `cargo-fuzz`: RustSec corpora, BoringSSL fuzz harnesses; `bolero`: AWS `s2n-quic`                                                                                                         | `bolero` for property/fuzz unification on the parser surfaces.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| Concurrency model checking                      | `loom`                                                               | Tokio internals                                                                                                                                                                           | Already in Phase 0.5; per-primitive scope discipline applies.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| **Deterministic simulation**                    | **`madsim`**                                                         | **RisingWave (dozens of distributed bugs publicly attributed to madsim DST in failover, recovery, and DDL surfaces — see RW blog series on madsim and `risingwavelabs/risingwave#4527`)** | **Drop-in `tokio` replacement with deterministic time / network / RNG. Adopting it makes the Phase 5 + Phase 13 deterministic-simulator items "integrate a runtime," not "build a simulator." See Phase 0.5 + Phase 5.**                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| Network fault injection (live)                  | `toxiproxy` (binary)                                                 | Shopify                                                                                                                                                                                   | Live, real-network fault injection (latency, drop, partition, bandwidth cap) for Phase 14.5 chaos tests against real cluster topologies. Operates on real sockets; not deterministic.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| In-process network simulation                   | `turmoil`                                                            | Tokio team                                                                                                                                                                                | In-process deterministic network simulation (drop, partition, reorder); narrower than `madsim` (network-only, no time/disk). Complement to `madsim` for tests where the full runtime swap is overkill but deterministic network behavior is required.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Randomized concurrency search                   | `shuttle`                                                            | AWS                                                                                                                                                                                       | Complement to `loom`; randomized scheduler instead of exhaustive. Optional, weekly nightly.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| UB detection                                    | `Miri`                                                               | Rust project                                                                                                                                                                              | Already in Phase 0.5; tree-borrows + strict-provenance enabled.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| Bounded model checking                          | `kani`                                                               | AWS S2N, Firecracker                                                                                                                                                                      | Optional release-gate verification for safety-critical modules.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| Secret handling                                 | `secrecy` (zeroizing wrappers) + `subtle` (constant-time)            | rustls, ring                                                                                                                                                                              | Already specified in security-axis bars.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| Bytes / serialization                           | `bytes`, `serde`, `prost` (proto), `bincode` (internal)              | Everyone                                                                                                                                                                                  | Standard. `serde` for human-facing config; `prost` on the wire; `bincode` for internal-only on-disk records that benefit from speed (with explicit version byte).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| Configuration                                   | `figment`                                                            | Rocket, others                                                                                                                                                                            | Layered config (file → env → CLI) with strict-schema validation. Backs the Phase 6 `mango --check-config` north-star bar.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| CLI                                             | `clap` (derive)                                                      | Most modern Rust CLIs                                                                                                                                                                     | Standard.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| gRPC service surface                            | `tonic-health`, `tonic-reflection`                                   | Everyone using `tonic`                                                                                                                                                                    | Standard health and reflection services for Phase 6 — required by k8s probes and `grpcurl`-style debugging.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| Rate limiting / load shedding                   | `tower::limit`, `governor`                                           | Linkerd, others                                                                                                                                                                           | Per-connection rate limit (`governor`) and concurrency cap (`tower::limit::ConcurrencyLimit`) for the Phase 6 gRPC DoS-hardening bar.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Backoff / retry                                 | `backon`                                                             | RisingWave, GreptimeDB                                                                                                                                                                    | Typed retry with exponential / jittered backoff for the Phase 7 `mango-client` endpoint failover.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| Internal opaque IDs (request IDs, snapshot IDs) | `uuid` (v4 + v7) and/or `ulid`                                       | Everyone                                                                                                                                                                                  | Phase 6 request-ID propagation, Phase 10 snapshot UUIDs. v7 (time-ordered) preferred for IDs that index into b-tree storage. **Note**: Phase 4 lease IDs are `i64` per the etcd-equivalent API shape and are **not** UUIDs/ULIDs — that is a wire-format decision the inventory does not override.                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| Block compression                               | `lz4_flex` (default), `zstd` (high-ratio)                            | TiKV, polars                                                                                                                                                                              | Phase 1 backend block compression (configurable). `lz4_flex` is the pure-Rust default; `zstd` is C-backed but standard.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| Time / wallclock display                        | `jiff`                                                               | Astral (`uv`, `ruff` adjacent timestamping)                                                                                                                                               | Human-facing timestamps in logs / lease-TTL display per `docs/time.md`. Protocol time stays on `Instant` (monotonic) per Phase 0. `chrono` is the legacy alternative.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Linearizability checker                         | **None production-grade in Rust as of 2026** — see notes             | —                                                                                                                                                                                         | **Honest gap**: no production-grade Rust linearizability checker exists. `porcupine-rs` on crates.io is its author's "learning project." The Phase 13 linearizability-checker item depends on one of three paths, picked via ADR in `.planning/adr/0013-linearizability-checker.md`: **(a)** FFI to `anishathalye/porcupine` (Go) via subprocess + JSON history; **(b)** call Aphyr's Elle (Clojure on JVM) as a side-binary from CI; **(c)** accept this as a hand-rolled component (Phase 13 work-item, ADR exception to the inventory's "no hand-rolling without ADR" rule). The Reviewer's Contract treats this as the inventory's named exception so the auto-`REVISE` trigger does not fire.                                                      |

**Reference systems we will actively mine (not just depend on):**

- **TiKV** — Raft usage patterns, snapshot handling, region split logic, CHANGELOG read end-to-end before each subsystem PR. Pinned as a behavioral oracle for Phase 5 (see the Phase 5 differential test).
- **RisingWave** — `madsim` test patterns; their published "bugs `madsim` caught" writeups feed our test design.
- **Databend** — `openraft` usage as the alternative-implementation control case.
- **etcd itself** — its integration test suite is a free correctness corpus; ported to mango in Phase 13.5.
- **Aphyr's Jepsen etcd test** — runs unmodified against mango binaries from Phase 13.

**The "use, don't write" default in two specific places that matter most:**

1. **Raft is the highest-stakes hand-roll.** TiKV's git log demonstrates a decade of bug-fixing in raft-rs, half of which a fresh implementation would re-discover. The Phase 5 ADR's burden of proof is on hand-rolling, not on adopting raft-rs.
2. **The deterministic simulator is the highest-leverage adoption.** `madsim` is a Cargo dependency, not a multi-month engineering project. Adopting it in Phase 0.5 (rather than building one in Phase 13) collapses the simulator-related items in Phases 5 and 13 from "build the harness" to "write the tests." This is the single largest schedule risk reduction available pre-implementation.

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

A phase is not "done" — and items inside it are not mergeable — unless all
of the following hold for the surface the phase introduces. The expert
agent enforces this list in plan + diff review.

- **Tested.** Every test class below is required where applicable. Missing
  any one is auto-`REVISE`.
  - **Unit tests** for every public function (table-driven where the
    input space allows).
  - **Property tests** (`proptest`, with shrinking) for every data
    structure, codec, or protocol op. Property tests are the _default_;
    unit tests are the exception, used only when the input space is
    genuinely a small enum.
  - **Integration tests** for every cross-crate boundary in
    `<crate>/tests/`.
  - **Crash / recovery tests** for anything that touches disk: kill the
    process at every interesting program point (between WAL append and
    fsync, between fsync and apply, mid-snapshot-write, etc.), reopen,
    assert no torn state and no committed data lost.
  - **Concurrency tests** for anything that touches `async` or threads:
    `loom`-based model-checking tests for every shared-state primitive
    introduced (channels, locks, atomics). The `loom` test exhaustively
    explores interleavings up to a configurable depth.
  - **Fuzz targets** (`cargo fuzz`) for every parser surface (codecs,
    config files, snapshot files, WAL records, gRPC bodies, query
    parsers). Per the Reviewer's contract: parser fuzz lives in the
    phase that introduces the parser. CI plumbing for the nightly fuzz
    run lives in Phase 15.
  - **Miri** runs on every test that touches `unsafe` or pointer
    arithmetic; PRs that add `unsafe` cite Miri output (or a written
    no-Miri justification) in the description.
  - **Test watchdog**: any test exceeding 10× its baseline duration is
    failed by CI as a likely deadlock or livelock. Implemented in
    `tests/watchdog.rs` (Phase 0).
- **Benchmarked.** Criterion bench for every hot path with a baseline
  number recorded in the plan and committed to `benches/results/<phase>/`.
  Where etcd has a comparable bench (we always run it ourselves against
  the pinned `benches/oracles/etcd/` binary on the hardware sig from
  `benches/runner/HARDWARE.md`), mango must beat it per the relevant
  north-star bar; where it does not, mango sets its own baseline. Bench
  numbers are tracked in CI (Phase 11) and regressions of more than 2σ
  fail the next PR's CI.
- **Observable.** Every public op emits a `tracing` span at INFO with
  stable target name (`mango.<crate>`) and stable field names. Every
  error path logs at WARN or ERROR with enough context to debug from
  the log alone. Hot-path metrics added to the Prometheus exposition
  through the `metrics` facade so Phase 11's wiring is plumbing-only.
  Spans propagate through `spawn_blocking` correctly (capture
  `Span::current()` and re-enter inside the closure).
- **Failure-safe.** No `unwrap()` / `expect()` / `panic!()` /
  `unimplemented!()` / `todo!()` / `dbg!()` in non-test code (clippy
  enforces — Phase 0). Every fallible op returns a typed error in a
  crate-local `Error` enum; `Box<dyn Error>` in a public API is
  auto-`REVISE`. `unsafe` is forbidden workspace-wide; per-module
  opt-in requires a `# Safety` comment block on every `unsafe` block
  and a Miri-output sign-off line in the PR description. **No
  `std::sync::Mutex` / `RwLock` in non-test code** (clippy enforces) —
  use `parking_lot` (no poisoning) or `tokio::sync` (no poisoning,
  async-aware). **No lock guard held across `.await`**
  (`clippy::await_holding_lock` enforces).
- **Concurrency-correct.** Every PR that introduces shared mutable
  state declares its synchronization strategy in the description and
  ships a `loom` test for it. PRs that introduce a new `Arc<Mutex<T>>`
  must explain in one line why a redesign (channel, actor, single-
  owner) wasn't possible — auto-`REVISE` otherwise. PRs that introduce
  spawned tasks must store the `JoinHandle` or document the fire-and-
  forget justification.
- **Documented.** Public items have rustdoc with at least one example
  that compiles (doctest); `cargo doc --no-deps -D warnings` is CI-
  gated (Phase 0). User-facing config and CLI flags documented in
  `docs/` (Phase 12 builds the site; earlier phases ship docs as `.md`
  next to the code).
- **Backwards-compatible at the API boundary** once Phase 6 ships gRPC
  publicly: `cargo public-api --diff` clean against the previous tagged
  release; on-disk format versioned via `data-dir/VERSION` with an
  upgrade matrix tested in CI (Phase 12). Until Phase 6, every
  breaking change is fine but must be flagged in the PR description.

## Reviewer's contract (the rust-expert agent)

The expert agent's `APPROVE` is the merge gate. To remove ambiguity, here
is the decision rule the agent applies on every plan + diff review.

### How to use this contract

The contract is a decision tree, not a 12-item checklist. Most PRs trip
exactly one classification; a few trip two. Apply only the gates for the
applicable classifications. **Items #1, #10, #11, #12 always apply.**

**First, classify the PR**:

- **plumbing** — CI, formatting, docs-only, tooling that doesn't move
  any north-star bar. Items #1 (declared as plumbing), #10, #11, #12.
- **perf** — claims a Performance / Storage-efficiency / Concurrency
  bar. Add #2.
- **correctness** — claims a Correctness or Reliability bar via a state-
  machine or protocol property. Add #3.
- **concurrency** — touches shared mutable state, async, or threads.
  Add #4.
- **unsafe** — adds or modifies an `unsafe` block, or touches a module
  that does. Add #5.
- **security** — touches auth, crypto, DoS surface, supply-chain, or
  side-channel-relevant comparisons. Add #6.
- **reliability** — claims a Reliability bar (recovery time, no
  thundering herd, slow-client containment, etc.). Add #7.
- **scale** — claims a Large-scale-distributed bar (cluster size,
  watcher count, dataset size). Add #8.
- **new public API** — adds a `pub` symbol that ships in a public crate
  or extends a gRPC surface. Add #9.

A perf PR runs items #1, #2, #10, #11, #12 (5 items); a concurrency PR
runs #1, #4, #10, #11, #12 (5 items); only an `unsafe`-touching auth-
crypto-API-changing PR runs everything. The decision tree keeps the
checklist tractable.

### `APPROVE` requires all of (the applicable items):

1. The plan or PR description **declares which north-star axis the change
   moves and names the specific bar + named test it verifies** (or
   honestly declares it as plumbing, e.g. CI / formatting). "Moves
   performance" is not enough; "moves Performance bar #2 (p99 at 50%
   saturation, verified by `benches/runner/grpc.sh`)" is.
2. **For perf-claiming PRs:** before/after Criterion numbers from the
   named runner script, with the comparison oracle's etcd version + bench
   command + hardware sig from `benches/runner/HARDWARE.md`, committed
   under `benches/results/<phase>/`. The numbers must clear the bar's
   threshold, not merely improve.
3. **For correctness-claiming PRs:** at least one new property test or
   simulator scenario that would have caught the previous bug or class
   of bug. For Raft / MVCC / Lease / Watch state machines, the test runs
   inside the Phase 5 / Phase 13 deterministic simulator, with the seed
   committed.
4. **For concurrency-claiming PRs (anything touching shared state, async,
   or threads):** at least one new `loom` test exhaustively exploring
   the introduced interleavings. PRs that introduce shared state without
   a `loom` test are auto-`REVISE`.
5. **For unsafe code:** every `unsafe` block has a `// SAFETY:` comment
   naming the invariant; PR description has a sign-off line citing Miri
   output (`MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly miri test
-p <crate>`) or a written justification for why Miri doesn't apply
   (e.g. FFI). The workspace `unsafe` count cannot grow without an
   approving PR label.
6. **For security-claiming PRs:** named test(s) covering the threat
   being mitigated (auth, crypto, DoS, supply-chain, side-channel,
   memory). For new auth / crypto code, a constant-time-comparison
   test where the comparison touches credential or hash material.
7. **For reliability-claiming PRs:** named test in `tests/reliability/`
   or `tests/chaos/` that exercises the failure mode and asserts the
   bound (recovery time, no data loss, bounded resource use).
8. **For scale-claiming PRs:** the relevant `benches/runner/*-scale.sh`
   runs to completion within the bar's threshold on the canonical
   hardware.
9. **For new public API:** at least one doctest, `#[must_use]` where
   applicable, `#[non_exhaustive]` for new enums (per the Phase 0.5
   policy), and `cargo public-api --diff` output in the PR
   _(advisory pre-Phase-6, gating from Phase 6 onwards)_. Plus
   `cargo-semver-checks` clean from Phase 6 onwards.
10. **CI green:** clippy clean (no `#[allow]` without a comment), tests
    green including doctests, fmt clean, deny clean, audit clean,
    `loom` tests passing where applicable, `cargo public-api --diff`
    clean (or `breaking-change` labeled).
11. **No new `TODO` / `FIXME` / `unimplemented!()` / `todo!()`** introduced.
12. The change either moves a north-star axis with measured evidence
    against a named test, or is honestly declared as plumbing (#1).

### Auto-`REVISE` triggers (no thinking required):

- A new metric label that takes a user-controlled value (key, lease ID,
  user ID, session ID, prefix, etc.).
- `.unwrap()` / `.expect()` / `panic!()` / `todo!()` / `unimplemented!()`
  / `dbg!()` outside `#[cfg(test)]` (clippy enforces once Phase 0 lint
  config lands).
- A new `unsafe` block without a `// SAFETY:` comment, or growth of the
  workspace `unsafe` count without an approving PR label.
- A `tokio::sync::Mutex` or `std::sync::Mutex` or `std::sync::RwLock`
  lock guard held across an `.await` (`clippy::await_holding_lock`).
- New use of `std::sync::Mutex` / `std::sync::RwLock` in non-test code
  (use `parking_lot` for sync, `tokio::sync` for async — neither
  poisons, both are faster).
- A new `Box<dyn Error>` in a public API.
- A spawned task without a `JoinHandle` stored or a documented
  fire-and-forget justification.
- A new `Arc<Mutex<T>>` without a one-line note explaining why a
  redesign (channel, actor, single-owner) wasn't possible.
- A new shared-state primitive (channel, lock, atomic) without a `loom`
  test exhaustively exploring its interleavings.
- A bench-claiming PR without numbers, with numbers from an unnamed
  oracle, or with numbers that don't clear the relevant bar's threshold.
- A claim against any north-star bar without naming the test that
  verifies it.
- A new dependency on `openssl-sys` (use `rustls`).
- A hand-rolled implementation of any subsystem listed in the workspace **Crate inventory** (see "Crate inventory & non-rolled stack") without an accompanying ADR in `.planning/adr/` justifying the deviation. Adding an _alternative_ to a row (e.g., a second async runtime, a second TLS stack) is the same trigger. **Enforcement is reviewer-side** — unlike the clippy / cargo-deny / grep-able triggers above, no mechanical CI lint detects "this is a hand-rolled TLS stack." The `linearizability checker` row's ADR (`0013-linearizability-checker.md`) is pre-required by the inventory itself, so the auto-`REVISE` rule operates normally on it (the row spells out the three permitted paths; picking one still requires the ADR).
- A non-constant-time comparison (`==` on `&[u8]`) in code touching
  credentials, tokens, or hash chains (use the `subtle` crate).
- A new `pub` symbol without a doctest.

### `APPROVE_WITH_NITS` is for:

- Style-only nits where the substantive bar is met.
- Bench numbers that meet the gate but want re-run on quieter hardware.
- Documentation polish opportunities.

### What "treats 'this is fine' as failure" means in practice:

If the reviewer's instinct is "this works, ship it" — but the PR did not
move any north-star axis, did not add a property test, did not add a
bench, and did not declare itself as plumbing — the verdict is `REVISE`
with the question: _what does this PR do that beats etcd?_ If the answer
is "nothing, it just keeps parity," then the implementation needs to
find the win or the scope needs to expand.

---

## Phase 0 — Foundation (must-block Phase 1)

Get the workspace into a state where Phase 1 can begin: deterministic builds,
CI on every push, the lints and policies that downstream phases will need
from day one, and the bench oracle harness so every later "beats etcd"
claim has a comparator. **Bounded to ~10 items / ~1 week of focused work
so Phase 1 is not blocked indefinitely.** The deeper supply-chain,
unsafe-tracking, Miri, `loom`, and security-shaped enforcement work lives
in **Phase 0.5** which runs in parallel with Phases 1–5 and must land before
Phase 6 ships gRPC publicly.

- [x] Set up CI (GitHub Actions): `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`, on push and PR
- [x] Add `rustfmt.toml` and `.editorconfig` so formatting is unambiguous
- [x] **Lint hardening**: workspace `Cargo.toml` `[workspace.lints.clippy]` table denies `clippy::unwrap_used`, `clippy::expect_used`, `clippy::panic`, `clippy::unimplemented`, `clippy::todo`, `clippy::indexing_slicing`, `clippy::cast_possible_truncation`, `clippy::cast_sign_loss`, `clippy::dbg_macro`, `clippy::print_stdout`, `clippy::print_stderr`, `clippy::await_holding_lock` in non-test code; `#[cfg_attr(test, allow(...))]` at test-module boundaries. **Note**: `clippy::arithmetic_side_effects` is intentionally NOT turned on in this item — it requires the workspace arithmetic-primitive policy (next item) to land first, otherwise every Raft index increment becomes an `#[allow]` retrofit and the lint defeats its own purpose.
- [x] **Workspace arithmetic-primitive policy** doc at `docs/arithmetic-policy.md`: defaults to `checked_*` for protocol-relevant counters (Raft index, revision, term, lease ID), `saturating_*` for timeouts and backoff timers, `wrapping_*` for hashes and explicit modular arithmetic. Once the policy lands and is reviewed, `clippy::arithmetic_side_effects` is turned on workspace-wide as a follow-up PR (counted as part of this item).
- [x] **Concurrency-primitive ban via `clippy::disallowed_types`**: `clippy.toml` (workspace root) declares `disallowed-types = [{ path = "std::sync::Mutex", reason = "use parking_lot::Mutex (no poisoning, faster) or tokio::sync::Mutex (async-aware); both bypass std's poisoning footgun" }, { path = "std::sync::RwLock", reason = "use parking_lot::RwLock or tokio::sync::RwLock for the same reason" }]`. **Correct mechanism** — `cargo-deny` bans crates, not stdlib type uses; `clippy::disallowed_types` operates at the type level inside any source file.
- [x] **Release-profile overflow checks**: `[profile.release] overflow-checks = true` in workspace `Cargo.toml`. Catches arithmetic panics in production, not just debug. Documented trade-off (~1-3% perf hit) accepted.
- [x] Add `deny.toml` and a `cargo-deny` CI job (license + advisory + duplicate-version checks; ban `git`-deps without explicit allowlist; ban `openssl-sys` per the Security axis — use `rustls`).
- [x] Add `cargo-audit` CI job (RustSec advisories) running on push, PR, and a nightly schedule. **Nightly schedule auto-files a GitHub issue on any new advisory**, even if no PR is open, so we don't sit on advisories between PRs.
- [x] Add a `cargo-msrv` job pinning the minimum supported Rust version (start at 1.80, bump deliberately) so we don't accidentally raise it.
- [x] **Bench oracle harness scaffold**: `benches/oracles/etcd/` checks in a script that downloads etcd v3.5.x at a pinned version + sha256, plus `benches/runner/HARDWARE.md` documenting the canonical hardware spec we run comparisons on, plus `benches/runner/run.sh` that prints a hardware signature alongside every result. Without this, every later "beats etcd by Nx" claim has no oracle. **`HARDWARE.md` MUST declare two hardware tiers**: (a) **Tier 1 / single-node bench rig** — single host, NVMe SSD, ≥ 16 cores, ≥ 64 GB RAM, sufficient for Phases 2-13 single-node and 3-node benches; (b) **Tier 2 / multi-node fleet** — ≥ 10 hosts (5 voters + 5 learners), ≥ 25 GbE intra-cluster bandwidth, root access for `tc` / `iptables` (or a `toxiproxy` install) to drive the Phase 14.5 chaos tests. If the Tier 2 fleet is unavailable, Phase 14.5 acceptance benches cannot ship and the Tier 2 release-gate fails (Phase 12 positioning-claim consistency gate).
- [x] **Monotonic-clock policy**: workspace doc note in `docs/time.md` declaring "all protocol-relevant time math uses `Instant` (monotonic), never `SystemTime`. Wallclock is used only for human-facing logs and lease TTL display, never for protocol decisions. Leap seconds: documented as N/A. NTP step tolerance: tested with ±5s clock jumps in Phase 13."
- [x] **Crash-only design declaration** in `docs/architecture/crash-only.md`: storage and server layers assume the process can be killed at any instant; clean shutdown is an optimization, never a correctness requirement. WAL-then-apply ordering and `data-dir/VERSION` recovery (Phase 12) make process restart equivalent to crash recovery. Every storage / Raft PR must satisfy "this would also be correct if killed at any point."
- [x] Create `crates/mango-proto` skeleton with `tonic-build` and a hello-world `.proto` that compiles
- [x] Add `CONTRIBUTING.md` covering branch naming, commit style, PR template, the test bar, **the north-star bar + reviewer's contract**, and the arithmetic-primitive policy.
- [ ] Add a PR template that forces every PR description to declare which north-star axis the change moves, names the verifying test, and records the measured number (or honestly marks as plumbing).

## Phase 0.5 — Foundation (parallel with Phases 1–5; must land before Phase 6 ships gRPC publicly)

Deeper enforcement work that doesn't block Phase 1 (storage) but must be in
place by the time Phase 6 ships a public API surface. Items here can be
worked in parallel with Phases 1–5 by anyone with cycles. They are not
allowed to slip past Phase 5 — the rust-expert is instructed to refuse any
Phase 6 PR that lands before Phase 0.5 is complete.

- [ ] **`loom` as a workspace dev-dependency**: every crate that introduces a shared-state primitive (channel, lock, atomic) ships at least one `loom`-based model-checking test under `tests/loom/`. CI runs `cargo test --features loom-test --release` (cfg-gated so it only builds under the feature) on push and PR. **Scope discipline**: `loom` tests model _individual primitives_ (a single channel, a single lock, a single atomic), not entire subsystems — exhaustive exploration scales exponentially in primitive count. Subsystem-level interleavings live in the Phase 13 deterministic simulator.
- [ ] **`madsim` as a workspace dev-dependency** (the deterministic-simulator runtime). The integration mechanism is **dependency renaming via Cargo's `package = "..."` field**, not `[patch.crates-io]`, per madsim's documented integration guide. Workspace `Cargo.toml` re-targets the runtime crates by rename. **Versions verified against crates.io as of the ADR date** (current as of late 2025: `madsim-tokio 0.2.x` wraps tokio 1.x; `madsim-tonic 0.6.x` wraps tonic 0.14; the team must re-verify before committing):

  ```toml
  [dependencies]
  tokio = { version = "0.2", package = "madsim-tokio" }   # wraps tokio 1.x
  tonic = { version = "0.6", package = "madsim-tonic" }   # wraps tonic 0.14
  # TLS in sim: there is NO published `madsim-tokio-rustls` shim. Real
  # `tokio-rustls` works under cfg(madsim) but TLS handshake timing is
  # non-deterministic; most sim tests skip TLS or terminate it in front
  # of the simulator (e.g., a real load balancer or a no-TLS test profile).
  # RNG in sim: there is NO published `madsim-rand` shim. Use `madsim::rand`
  # directly from inside `#[cfg(madsim)]` test code.
  ```

  Source code then writes `use tokio::time::sleep;` / `use tonic::transport::Channel;` **unchanged** — the rename does the swap at link time. The simulator is activated by `RUSTFLAGS="--cfg madsim"` (set by the `sim` CI profile), not by a Cargo feature; `#[cfg(madsim)]` gates code paths that exist _only_ in sim (test scaffolding, fault injectors, `madsim::rand` calls). `[patch.crates-io]` is reserved as the escape hatch for transitive dependencies the team cannot rename (a dep that pulls `tokio` directly without a feature gate). `std::time` is intercepted from inside `madsim-tokio`'s runtime, not patched.

  **Why now and not Phase 5 / 13**: every later phase that ships an async primitive needs to be sim-testable, and retrofitting the rename across an established codebase is more expensive than adopting it from line 1. Adopting `madsim` in Phase 0.5 turns the Phase 5 "deterministic simulation testing harness" item from "build it" into "write tests against it," which is the single largest schedule-risk reduction in the roadmap.

  **Scope**: `mango-raft`, `mango-mvcc`, `mango-server`, `mango-client` MUST be `madsim`-compatible by the time their respective phases ship. The CI matrix runs every async test under both the default profile (real `tokio`) and the `sim` profile (`RUSTFLAGS="--cfg madsim"`); regressions in either profile fail the PR. The crate inventory table above enumerates `madsim` as the default deterministic-simulator runtime; alternatives (`turmoil`, `shuttle`) are complementary, narrower-scope tools.

- [ ] **Test watchdog via `cargo-nextest`**: `.config/nextest.toml` declares per-test-class timeouts (`unit = 30s`, `integration = 5min`, `loom = 30min`, `chaos = 24h`). CI runs `cargo nextest run --profile ci`; tests exceeding their class timeout are killed and reported as failed. Per-test 10×-baseline regressions are surfaced via `nextest`'s flake-detector and `tests/watchdog-baselines.json` (updated in the same PR that legitimately makes a test slower). **Why nextest, not a custom harness or proc macro**: a custom harness would lose `cargo test --filter` and IDE integration; a proc macro relies on every test author remembering to use it. `nextest` does the wrapping natively and is the standard tool.
- [ ] **Miri CI workflow** at `.github/workflows/miri.yml`: nightly Miri run across the curated subset of tests touching `unsafe` / pointer arithmetic; fails on any UB. **`MIRIFLAGS` includes both `-Zmiri-strict-provenance` AND `-Zmiri-tree-borrows`** (the latter is the stricter aliasing model becoming default in nightly Miri). Per PR: Miri runs only on changed crates that contain `unsafe` blocks. Per release: Miri runs on the full curated subset.
- [ ] **Workspace `unsafe`-count tracker via `cargo-geiger`**: CI runs `cargo geiger --output-format=Json --workspace --all-targets` and asserts the `unsafe_used.functions.unsafe_` + `unsafe_used.exprs.unsafe_` + `unsafe_used.item_impls.unsafe_` counts (across mango crates) do not grow without an approving `unsafe-growth-approved` PR label. Baseline in `unsafe-baseline.json`, updated in the same PR that legitimately adds an `unsafe` block. Bonus: `cargo-geiger` also flags transitive `unsafe` density per dep, used as a supply-chain signal in the `cargo-vet` review.
- [ ] **Constant-time comparison enforcement via `dylint`**: project-local lint that checks any `==` whose operands resolve to `&[u8]` / `Vec<u8>` / `bytes::Bytes` in modules matching `crates/*/src/**/auth*`, `**/crypto*`, `**/token*`, `**/hash_chain*`; suggests `subtle::ConstantTimeEq::ct_eq`. **Until the dylint lands**, the enforcement is a CI grep + manual review at security-relevant PRs and the corresponding north-star Security side-channel bar is downgraded to "documented + reviewer-enforced." Operationalizes the Security side-channel bar.
- [ ] Add `cargo-vet` (or equivalent supply-chain audit gate) so every transitive dep has an audit entry; missing audits fail CI.
- [ ] Add `cargo-semver-checks` CI job: catches semver violations the API surface alone misses (e.g., adding a required generic parameter). Gates breaking changes from Phase 6 onwards alongside `cargo-public-api`.
- [ ] Add an SBOM build step (`cargo-cyclonedx`) that produces a CycloneDX file per release; attached to GitHub Releases in Phase 12.
- [ ] Add a `cargo doc --no-deps --document-private-items` job with `RUSTDOCFLAGS=-D warnings` so broken doc links fail CI. Plus `#![deny(missing_docs)]` at every `crates/mango-*/src/lib.rs` root for public crates.
- [ ] Add `cargo-public-api` CI check **(advisory pre-Phase-6, gating from Phase 6 onwards)** — alongside `cargo-semver-checks` once Phase 6 ships.
- [ ] Add a Renovate / Dependabot config so action SHAs and crate versions get bumped via PR (preserves the SHA-pin policy without it rotting).
- [ ] **`#[non_exhaustive]` policy on public enums**: documented in `docs/api-stability.md`; every `pub enum` in non-internal crates is `#[non_exhaustive]` unless a documented exception applies. Enforced by code review and (where possible) by a clippy custom lint or `cargo-public-api` check.

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
- [ ] **Disk-full reliability test** `tests/reliability/disk_full.rs`: fill the data dir to 100%, attempt a write; assert the server enters read-only mode, raises `NOSPACE`, never crashes, never corrupts; free space; assert the server recovers cleanly and accepts writes. Operationalizes Reliability bar #3.
- [ ] **Disk-size bench** `benches/runner/disk-size.sh`: load `benches/workloads/storage.toml`'s standard workload into mango (default compression on) and into etcd (default compression off — both defaults). Compare on-disk size. **Mango ≤ 0.7× etcd's**, per the Storage-efficiency bar. Numbers in `benches/results/phase-1/disk-size.md`.
- [ ] Bench harness in `benches/storage/`: write-throughput (1KB values, batched and unbatched), read-latency p50/p95/p99 (hot and cold cache), range-scan-throughput (100 / 10k / 100k keys), on-disk size after the workload in `benches/workloads/storage.toml`. Comparison oracle is the Go binary at `benches/oracles/bbolt/` on the hardware sig in `benches/runner/HARDWARE.md`. **Mango must win on at least one metric, lose on none. Numbers committed to `benches/results/phase-1/`.**
- [ ] Block-level compression (LZ4 or zstd, configurable) — disabled by default for parity bench, enabled for the size-comparison number

## Phase 2 — MVCC layer

etcd's MVCC: every write produces a new revision; keys are addressed by
`(key, revision)`; tombstones; compaction. Built on top of the phase-1
backend.

- [ ] Define `Revision { main: i64, sub: i64 }` and the on-disk key encoding (`key_index` + `key`-bucket layout, mirror etcd's split conceptually)
- [ ] Implement `KeyIndex` (in-memory tree of keys → list of generations of revisions) with put / tombstone / compact / restore-from-disk
- [ ] **Sharded in-memory `KeyIndex`** as `[parking_lot::RwLock<HashMap<Bytes, KeyHistory>>; 64]`, hashed by `ahash::RandomState` seeded once at process start from a CSPRNG and shared across all 64 shards. **Why `RwLock` and not `Mutex`**: the watcher-cache target workload is "watch one key, read it repeatedly" — adversarially hot keys land in one shard, where `Mutex` would serialize all readers. `RwLock` lets concurrent readers on the same shard parallelize at the cost of a small write penalty (~10ns per `write()` under `parking_lot`'s implementation), which is the right trade for a read-heavy KV. **Why `ahash` per-process random seed**: prevents key-collision DoS where an attacker who can predict keys (e.g., paths under `/registry/`, lease IDs through clients) targets one shard and serializes its readers. The single-mutex design also fails Phase 6's per-core scaling bar (`≥ 14× at 16 cores` on the read-only workload — Concurrency axis #2). Hand-rolled (not `dashmap`) because of `dashmap`'s CVE history (RUSTSEC-2022-0002 et al.), `unsafe_code = "forbid"` ethos, easier `loom`-testability, and a tighter `cargo-vet` audit surface.
- [ ] **`loom` test for the sharded key index** in `crates/mango-mvcc/tests/loom/sharded_index.rs`: model concurrent put + range + compact across two shards; assert no torn reads, no missed compactions, no deadlock under arbitrary interleaving.
- [ ] **Hostile-key DoS test** in `crates/mango-mvcc/tests/security/keyindex_dos.rs`: with the `ahash` seed fixed to a known value (test-only API), confirm that an attacker who _knows_ the seed can construct N keys colliding into one shard and bring per-shard read latency to its single-`RwLock` ceiling. Then re-seed with a fresh CSPRNG-derived value and confirm the same key set distributes across shards within statistical bounds (no shard holds > 2× the mean key count). Validates that production seeding defeats the attack.
- [ ] **Note on Phase 2 acceptance vs Phase 6 verification**: Phase 2's acceptance for these structures is the `loom` test (correctness) and the snapshot-reclamation bench (resource bounds). The full per-core scaling verification (`≥ 14× at 16 cores`) cannot run until Phase 6's gRPC stack ships and the integrated bench harness exists. Phase 2 ships green on correctness; Phase 6 ships green on the perf number that depends on these structures.
- [ ] Implement the MVCC `KV` API: `Range`, `Put`, `DeleteRange`, `Txn` (compare + then/else ops), `Compact`
- [ ] Read transactions return a consistent snapshot at a chosen revision
- [ ] **Lock-free snapshot publication via `arc_swap::ArcSwap<Snapshot>`**. Readers acquire the latest snapshot with a single `Acquire` load; the MVCC apply loop swaps in a new `Arc<Snapshot>` after each batch; old `Arc`s drop when the last reader releases. Required for the Concurrency axis #2 read-only per-core bar — a snapshot mutex is the same kind of bottleneck the sharded `KeyIndex` removes, in a different position. **API discipline**: `Range` over more than 1000 keys MUST use `arc_swap::ArcSwap::load_full()` (returning a plain `Arc`), not `load()` (returning a `Guard`), so the slot is not pinned for the duration of the scan. Documented in the `mango-mvcc` rustdoc.
- [ ] **Property test in `crates/mango-mvcc/tests/properties/snapshot_consistency.rs`**: under concurrent reads + writes, every reader sees a snapshot that is either "the snapshot at read-time" or "a snapshot that committed before read-time." No torn snapshots.
- [ ] **`benches/runner/snapshot-reclamation.sh`**: sustained writes at 100K/sec for 60s while 16 reader threads each hold a snapshot for 1ms / 10ms / 100ms (three sweeps). **Assert**: reclamation latency p99 ≤ 1ms after slowest reader drops, and RSS bounded by `snapshot_baseline_bytes + (slowest_reader_hold_seconds × write_rate_per_second × bytes_per_write_delta) × 1.5`, where `bytes_per_write_delta` is the per-write retained delta size (typically ~1KB for the bench's 1KB values + per-revision metadata, _not_ the full snapshot size). At 100K writes/sec × 0.1s × ~1KB × 1.5 ≈ 15 MB delta retained at the 100ms-hold sweep, on top of a typical multi-GB snapshot baseline. Catches the `ArcSwap` Guard-pinning footgun before it surfaces in production. Numbers in `benches/results/phase-2/snapshot-reclamation.md`.
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

- [ ] ADR in `.planning/adr/0005-raft-implementation.md` confirming the Raft implementation. **Default per the workspace crate inventory: `tikv/raft-rs`** (TiKV operates raft-rs at multi-PB scale in production at PingCAP customers; a decade of bug-fixing already paid for). **Burden of proof is on deviation, not adoption.** Adopting `openraft` requires the ADR to document a specific raft-rs failure on at least one criterion below that openraft passes. Hand-rolling requires the ADR to document failures of _both_ off-the-shelf crates against _all four_ criteria, plus a written acknowledgement of the multi-year maintenance commitment. **The ADR is not "one page"** — even when the answer is "raft-rs," it must cover §1 the four decision criteria below, §2 the API-driver discipline mango will enforce (callbacks, snapshot model, `Ready` cadence), §3 the integration with `mango-storage`'s `Backend` trait, §4 how `cfg(madsim)` interacts with raft-rs's internal scheduling (does it use `tokio` directly? if not, does the deterministic clock still apply?). Expect ~5-10 pages. **Decision criteria**: (1) faster leader-failover recovery than etcd, (2) lower steady-state CPU, (3) a clean path to deterministic-simulation testing under `madsim` (Phase 0.5), (4) **evidence of active maintenance** — issue triage cadence, security-advisory channel, response to critical bug reports within a reasonable SLA — measured at ADR time, not by a hard "commits in the last N months" threshold (mature, feature-stable crates can quiesce by design). Re-checked at each major-release ADR refresh.
- [ ] `crates/mango-raft` skeleton with the chosen crate (or hand-rolled module structure)
- [ ] Single-node Raft: proposals get applied to a state-machine trait; the state-machine is wired to the MVCC store
- [ ] WAL: append every entry before applying; replay on startup; **bounded WAL space** with retention by size + time, oldest segment recycled or deleted post-snapshot. Documented behavior when WAL disk fills (refuse new proposals, raise `WAL_FULL` alarm).
- [ ] Snapshot: state-machine snapshot + WAL truncation; reload on startup if WAL gap; **snapshot streaming has a configurable bandwidth limit** so it cannot saturate the network and cause Raft heartbeat timeouts on the leader.
- [ ] 3-node cluster over TCP transport: leader election, log replication, follower catch-up
- [ ] Linearizable reads via ReadIndex (no stale reads from followers without quorum-check)
- [ ] **Pipelined log replication + batch commit** — one of mango's core perf wins over etcd; bench gate vs single-flight replication baseline
- [ ] **Deterministic simulation testing on `madsim` from day one** — `mango-raft` builds and tests under both real `tokio` (CI default) and `madsim` (CI `sim` profile). Every Raft test in this phase has a sibling under `tests/sim/` that runs the same scenario through `madsim`'s deterministic clock + network + RNG. The integration cost is paid once (Phase 0.5's `madsim` workspace adoption did the cfg-flag plumbing) so this item is "write the tests," not "build the harness." Seeds for any failing scenario are checked into `tests/sim/regressions/` so every fix lands with its reproducer. (Phase 13 extends this to the full server; it does not start it.)
- [ ] Network-partition tests in the simulator: 2/1 split, 1/1/1 split, leader isolation, asymmetric partitions, message reordering; assert no split-brain, no lost committed entries
- [ ] Crash-recovery tests in the simulator: kill follower mid-replication, kill leader mid-commit, restart, cluster converges
- [ ] **`cargo fuzz` target for WAL record decode** (per the Reviewer's contract: parser fuzz lives where the parser does). CI plumbing in Phase 15.
- [ ] Bench in `benches/raft/`: 3-node cluster on local loopback, 1KB Put values, runner script `benches/runner/raft.sh` invoking `etcd-benchmark put --conns=100 --clients=1000 --total=100000 --val-size=1024` against the pinned etcd in `benches/oracles/etcd/` and the equivalent against mango on the hardware sig in `benches/runner/HARDWARE.md`. **Mango beats etcd by ≥ 1.5× on throughput and ≤ 0.7× on p99 commit latency.** Numbers committed to `benches/results/phase-5/raft.md`.
- [ ] **Failover bench** `benches/runner/failover.sh`: 3-node cluster under sustained 1KB Put load; `SIGKILL` the leader; measure wall-clock time until the cluster accepts a quorum-write again. **Mango's failover time ≤ 0.7× etcd's** under identical conditions. Numbers committed to `benches/results/phase-5/failover.md`. Operationalizes the Performance "leader-failover-to-quorum-write" bar.
- [ ] **Cluster-size scaling bench** `benches/runner/cluster-size.sh`: same workload as `raft.sh` but across {3, 5, 7} voters; document throughput delta vs cluster size and compare to etcd. Operationalizes the Large-scale-distributed bar #1. Numbers in `benches/results/phase-5/cluster-size.md`.
- [ ] **`raft-rs` driver-discipline differential test** `tests/raft/driver_discipline.rs` (gated behind a `raft-driver-oracle` feature so the comparison driver is not pulled in by default). **Honest framing first**: if mango-raft uses `tikv/raft-rs` (the default per the workspace crate inventory), then both sides of this test share the same `raft-rs` implementation. This test cannot catch _Raft semantic bugs_ in raft-rs itself — by construction, two `RawNode`s built from the same crate at the same revision agree on Raft semantics. What it _can_ catch is **API-driver-discipline bugs**: mis-sequenced `propose` / `step` / `tick` calls, missed `Ready` handling, wrong `advance_apply` cadence, snapshot-application races. The "comparison driver" is a known-good minimal driver maintained alongside this test (initially modeled on the `examples/single_mem_node` and `examples/five_mem_node` examples in the upstream `raft-rs` repo at the pinned version), not a TiKV `tikv-server` binary. Run a workload of operations through both drivers with identical Raft configuration; assert the call sequence and `Ready` handling match. **What this test is NOT**: a substitute for Jepsen, the deterministic simulator, or the conformance suite. It is narrow API-discipline insurance against subtly mis-driving raft-rs, which is the single most common failure mode for new raft-rs adopters per the upstream issue tracker. Documented intentional differences live in `docs/raft-driver-discipline.md`. Runs nightly in CI alongside the simulator, not per-PR (the comparison driver is non-trivial to maintain).
- [ ] **`loom` tests for the Raft shared-state primitives** — split into three narrowly-scoped tests because exhaustive `loom` exploration of the full apply-path triple would explode the state space (and end up `#[ignore]`-d). Each test models a single primitive and asserts a single invariant:
  - `crates/mango-raft/tests/loom/apply_channel.rs` — model the `mpsc::channel` between commit-detector and apply-loop; assert no message lost, no message applied twice.
  - `crates/mango-raft/tests/loom/snapshot_apply.rs` — model the snapshot-read-while-apply-writes ordering; assert snapshot consistency under concurrent apply.
  - `crates/mango-raft/tests/loom/wal_apply_ordering.rs` — model WAL-append-then-apply (release/acquire); assert no apply observes an un-fsynced entry.
  - Together these cover the dangerous shared-state interactions in the Raft apply path. Subsystem-level interleavings (full triple) live in the Phase 13 deterministic simulator, not in `loom`. Operationalizes the Concurrency "zero deadlocks" bar for the highest-stakes shared-state primitives in the codebase.
- [ ] **Reliability test** `tests/reliability/follower_catchup.rs` + supporting `benches/runner/follower-restart.sh`: 10M-revision cluster, kill a follower, restart, measure leader's egress bandwidth and time-to-rejoin-quorum. Asserts: ≤ 1.2× steady-state network ingress on the leader for ≤ 30s. Operationalizes Reliability bar #1.

## Phase 6 — gRPC server: KV + Watch + Lease

Wire phases 2–5 to the network. `mango-server` hosts the gRPC services and
is the binary you actually run.

- [ ] Author `.proto` for KV, Watch, Lease (Rust-native shape; copy etcd's semantics, not its message names)
- [ ] **Proto breaking-change check via `buf breaking`**: CI job runs `buf breaking --against '.git#branch=main'` on every PR touching `crates/mango-proto/proto/`; breaking proto changes require the `breaking-change` PR label and a major-version bump. `cargo public-api` does not see proto changes, so `buf` is the right tool here.
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
- [ ] **Idle RSS bench** `benches/runner/idle-rss.sh`: 3-node cluster, empty data dir, no traffic for 60s; record RSS via `/proc/self/status` and compare to etcd's same setup. **Mango's RSS ≤ 0.7× etcd's**, per the Performance bar. Numbers in `benches/results/phase-6/idle-rss.md`.
- [ ] **Cold-start bench** `benches/runner/cold-start.sh`: process exec → first successful Put accepted by the cluster, measured wall-clock; compared to etcd's same setup. **Mango's cold start ≤ 0.7× etcd's**, per the Performance bar. Numbers in `benches/results/phase-6/cold-start.md`.
- [ ] **Per-core scaling bench** `benches/runner/per-core-scaling.sh --workload={read-only,mixed,write-heavy}`: single-node cluster, sweep `tokio` worker-thread count 1..N where N = host core count, measure throughput at each step under each of the three workloads. **Per the Concurrency-axis per-workload bars**: read-only ≥ 14× throughput at 1 core (linear scaling on the read path), mixed (50/50) ≥ 8×, write-heavy (90% writes) ≥ 4× (apply is fundamentally serial in Raft; the win over etcd's ~3× comes from pipelined replication and tighter batching). Compares to etcd at the same `GOMAXPROCS` settings under each workload; numbers in `benches/results/phase-6/per-core-scaling-{read-only,mixed,write-heavy}.md`. Demonstrates that mango's no-GC + structured concurrency translates into actual hardware utilization on the read path, and that pipelined Raft + tighter batching beats etcd on the write path within Raft's structural ceiling.
- [ ] **Slow-loris and oversized-frame DoS tests** `tests/dos/slow_loris.rs` + `tests/dos/oversized_frames.rs` + `tests/dos/grpc_hostile_client.rs`: misbehaving-client harness that opens many slow connections, sends oversized HTTP/2 frames, opens then abandons streams. Asserts: server RSS bounded, p99 latency for legitimate clients unchanged, all hostile connections rejected with the documented status code. Operationalizes the Reliability "slow client cannot stall the server" bar.
- [ ] **Config-validation test** `tests/config/check_config.rs`: corpus of malformed configs (unknown keys, type mismatches, mutually-exclusive flags); each entry asserts `mango --check-config` exits non-zero with the expected diagnostic. Operationalizes the Operability `--check-config` bar.

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
- [ ] **No-proto-leakage test** `tests/api/no_proto_leakage.rs`: extract `mango-client`'s public API surface via `cargo public-api` or `rustdoc-json` and assert zero `prost::*` / `tonic::*` types appear. Operationalizes the Developer-ergonomics bar.

## Phase 7.5 — Web UI (read-only browse mode)

Ships an early, narrowly-scoped slice of the eventual full operational
console (Phase 16 + 16.5) so users get localhost-browse value as soon as
KV is real. **Read-only only** — no mutations until Phase 16, which lands
after auth (Phase 8) so destructive ops are gated by RBAC.

This phase exists to (a) prove the **out-of-process** UI architecture
(see "Architecture" below — the UI is a _separate binary_, never inside
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
   scheduler time with _itself_, not with Raft.
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

- [ ] **Frontend-stack ADR** in `.planning/adr/` (rust-expert + a frontend-design pass at plan time). Candidates: server-rendered HTML + HTMX, Rust→WASM (Leptos / Dioxus / Yew), React + Vite + TS, SvelteKit. Decision criteria, in order: **(a) fastest dev iteration**, **(b) smallest binary contribution**, **(c) no Node in the user's `cargo install` build path** (Node tooling, if any, runs only at release time; built artifacts are checked in or fetched, never built from source on user installs), **(d) first-meaningful-paint ≤ 1s on a simulated 1 Mbps connection**, **(e) team can carry the stack for ≥ 2 years**, **(f) stack supports a top-level error boundary that catches uncaught errors and reports them server-side without leaking stored values into the report payload**. The rust-expert's prior recommendation for the rest of the team's consideration: **HTMX + askama (or maud) for Phase 7.5** (read-only forms / tables; ~14 KB; zero build step; stays Rust-only); **Leptos with SSR + islands for Phase 16** (client-side state for the txn builder + topology view; ~150-400 KB WASM after `wasm-opt`; SSR keeps first-paint fast); SvelteKit-with-built-artifacts as the fallback if Leptos is vetoed by the frontend reviewer.
- [ ] `crates/mango-ui` skeleton: a **separate binary** (`mango-ui`) built from `crates/mango-ui` that talks to mango via the Phase 6 gRPC client. **Not embedded in the `mango-server` process** — see "Architecture" above for the reasoning. Default port `:2381`, configurable.
- [ ] **Operator opt-in required, even for localhost.** Starting `mango-ui` requires `--listen <addr>` set OR `[ui] listen = "..."` in the config OR `MANGO_UI_LISTEN` env. **Precedence matches the project-wide convention from Phase 6: CLI > env > file > default.** There is no auto-start mode. **The UI is OFF by default in every shipped artifact.** The `dev-ui` cargo feature (non-default) lets mango developers run `cargo run --features dev-ui --bin mango-ui` with a default localhost listener — this is for _mango contributors only_ and never enabled in published binaries. **Enforcement mechanism**: a CI job runs `cargo build --release --bin mango-ui --no-default-features` and asserts via `cargo metadata --format-version 1 | jq '.resolve.nodes[] | select(.id | contains("mango-ui")) | .features'` that `dev-ui` is not in the activated set; release artifacts that fail this check are rejected. Documented bluntly in `docs/ui-deployment.md`: "the previous version of this spec said 'on by default in dev profile' — that is impossible to do honestly because Cargo build profiles are not visible at runtime, so anyone running `cargo install --debug mango-ui` would get the UI on without knowing it. The current rule is: opt-in everywhere, no exceptions."
- [ ] **Startup WARN banner** when the UI is enabled: "Mango UI is enabled on `<addr>`. The UI is read-only but exposes every key/value in the cluster to anyone who can reach this address. Do not store secrets in mango until Phase 8 (auth) is shipped and the cluster is auth-enabled. To disable, remove `--listen` / `[ui] listen`." Banner is printed to stderr and logged at WARN so it's visible in both interactive and structured-log shipping.
- [ ] **Bind discipline**:
  - Default bind is **both `127.0.0.1:2381` AND `[::1]:2381`** as separate listeners (don't use `[::]:2381` — dual-stack behavior is OS-dependent and has surprised people for decades). On a dual-stack host, the operator who tries to reach `[::1]:2381` from their browser succeeds without needing to widen the bind.
  - Either listener can be disabled individually via config.
  - Any non-loopback bind requires both `--ui-allow-non-loopback-bind` (renamed from the previous awkward `--insecure-ui`; the new name describes what the operator is acknowledging) AND TLS configured (`--ui-tls-cert` + `--ui-tls-key`); refuse to start otherwise with an actionable error.
  - **Container / pod warning** in `docs/ui-deployment.md`: in Docker, `127.0.0.1` is the _container's_ loopback; use `-p 127.0.0.1:2381:2381` on the host side or `--network host` with mango-ui's own bind. In Kubernetes, every container in the same pod shares `127.0.0.1`; the UI MUST be disabled in pods that host untrusted sidecars.
  - Tested: a startup-config matrix asserts every (bind, tls, allow-non-loopback) combination either starts cleanly or refuses-and-exits-non-zero with the documented diagnostic.
- [ ] **`docs/ui-readonly-warning.md`**: explicit "do not store secrets in mango until Phase 8 is shipped" — Kubernetes Secrets, Vault backend storage, application credentials, TLS private keys. Linked from the README and from the WARN banner.
- [ ] **Observability — every UI route handler emits a `tracing` INFO span** under target `mango.ui` with stable field names: `route`, `method`, `user_id` (when authed), `status`, `duration_ms`, `audit_event_id` (where relevant). Spans propagate the request ID through to the backend gRPC calls so a single operator action is traceable end-to-end. Inherited by Phases 16 and 16.5. **Forward note for Phase 11**: any UI metric introduced when Phase 11's metrics work covers the UI surface follows the cardinality discipline rule from line 486 — no `session_id`, `user_id`, `key`, `prefix`, or `ui_instance_id` may become a metric label, ever.
- [ ] **URL stability — the UI URL space is a public API from Phase 7.5 onwards.** `docs/web-ui-routes.md` is the checked-in snapshot of every route, its method, its query parameters, and its response shape. CI diffs every PR against the snapshot; any change requires the PR to either (a) update the snapshot in the same commit and explain why the change is non-breaking, or (b) carry the `breaking-change` PR label, ship an HTTP 308 redirect from the old URL alongside the new one for one minor version, and bump the appropriate version. Operators inevitably write bookmarks and external monitoring against `/keys/<key>`, `/cluster/status`, etc.; this gate prevents silent contract breakage from the very first UI PR.
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
- [ ] mTLS for both client-server (`:2379`-equivalent) and peer-to-peer (`:2380`-equivalent) — cert + key + CA flags wired through config. **Implementation**: TLS via `rustls` + `rustls-platform-verifier` so peer cert validation uses the platform native trust store on macOS / Windows / Linux (not just a CA bundle). Banned `openssl-sys` per Phase 0 `cargo-deny` policy.
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
- [ ] **Extend `ConfChange` to encode a `promotable: bool` flag on learner state.** `true` for a normal learner (can later be promoted to voter), `false` for a read-replica (Phase 14.5 deployment mode — never promotable). The promote-learner API rejects on `promotable == false` **at the state-machine level**, not at a server-side flag, so a misconfigured operator cannot accidentally promote a read-replica. Required dependency for Phase 14.5's `mango-server --role=read-replica`.
- [ ] **Voter-floor enforced in the `ConfChange` apply path** (not at the CLI). Any `ConfChange` whose effect would drop the cluster's voter count below `quorum_floor` (default 3, configurable via cluster bootstrap) is rejected at the state machine. This catches voter removals, voter→learner demotions, and voter→read-replica demotions equivalently — and equivalently for _every_ client of the gRPC `Cluster` service (the CLI, the web UI, third-party ops tools, older `mangoctl` versions). Rationale: the same lesson as `promotable: bool` — the state machine is the only thing every client must traverse, so it is the only correct enforcement boundary. The CLI's confirmation prompt remains for UX, but it is no longer the enforcement.
- [ ] Promote-learner-to-voter API with safety check (learner must have caught up to within N entries of leader, _and_ `promotable == true`)
- [ ] `Cluster` gRPC service: member list/add/remove/promote/update
- [ ] `mangoctl member` subcommand group including `member add --learner` and `member promote`
- [ ] Tests: 3-node cluster + add learner, learner catches up, promote, remove old member, no quorum lost
- [ ] **Test that promoting a `promotable: false` learner is rejected at the state-machine level** in `tests/reliability/membership_change.rs::promote_read_replica_rejected`: add a learner with `promotable == false`, attempt promote, assert the `ConfChange` is rejected by the apply path (not just the gRPC layer) and cluster state is unchanged.
- [ ] **Test that voter-floor enforcement is at the state-machine level** in `tests/reliability/membership_change.rs::voter_floor_enforced_at_state_machine`: with a 3-voter cluster (at the floor), submit a `ConfChange` directly via the gRPC `Cluster` service (bypassing the CLI's confirmation prompt) that would demote one voter to a read-replica. Assert the apply path rejects with a typed `VoterFloorViolation` error, cluster state is unchanged, no leader change. Repeat with a direct voter-removal `ConfChange` and assert the same rejection.
- [ ] **No-leader-flap test** `tests/reliability/membership_change.rs::no_leader_flap_under_membership_change`: healthy 3-node cluster, run a 1-member-add-then-promote cycle under sustained Put load; assert zero leader changes during the cycle. Operationalizes Reliability bar #2.

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
- [ ] **`tokio-console` integration** behind a `console-subscriber` cargo feature flag; off in default release builds (the instrumentation has measurable overhead — ~1-3% on hot async paths), on for debugging stuck async tasks, deadlocks, or starvation. Documented in `docs/debugging.md` with the recipe to attach `tokio-console` to a running mango. Critical for a system this concurrent — without it, async deadlocks are nearly impossible to debug from logs alone.
- [ ] Default filter exposes user-relevant events without `RUST_LOG` tuning; `MANGO_LOG` env var with precedence over `RUST_LOG`
- [ ] Prometheus exposition on `/metrics` covering request counts/latencies per RPC, Raft proposals / leader changes / log lag, MVCC db size + revision + compacted-revision, lease counts, watcher counts, backend write-amplification, fsync latency
- [ ] **Cardinality discipline**: every metric's label set is documented; no user-controlled values (key names, lease IDs) ever become labels
- [ ] Per-RPC `#[instrument]` spans with stable field names; spans propagate through `spawn_blocking` correctly (capture `Span::current()` and re-enter inside the closure)
- [ ] **Tracing emits OTel-format spans natively** (`tracing-opentelemetry` bridge wired in `mango.server`'s init). The OTLP exporter is **off by default**; setting `MANGO_OTLP_ENDPOINT` enables it. The win over etcd is _format quality_ (etcd's klog output isn't easily ingestible by OTel pipelines; ours is, out of the box) — not transport-on-by-default, which would mean every install logs "OTLP export failed: connection refused" forever in environments without a collector.
- [ ] Sample Grafana dashboard JSON committed to `dashboards/`, with a "mango vs etcd" comparison panel using the bench harness output
- [ ] **Continuous benchmark CI job**: every merge to `main` runs the Phase 5 / Phase 6 benches and uploads to a tracked baseline; regressions fail the next PR's CI
- [ ] Tests: hit the server, scrape `/metrics`, assert expected metric families exist with expected labels and bounded cardinality
- [ ] **Metric-cardinality test** `tests/observability/metric_cardinality.rs`: drive a 10k-key workload, scrape `/metrics`, assert each family's distinct label-value count stays below the bound declared in `docs/metrics.md`. Operationalizes the Operability cardinality bar.
- [ ] **Log-targets test** `tests/observability/log_targets.rs`: capture tracing output during a representative workload **filtered to spans whose module path starts with `mango_`** (i.e., emitted from mango code, not from dependencies like `tonic`, `tower-http`, `h2` which carry their own targets); assert every such span uses one of the documented `mango.*` target names. A separate static check (clippy custom lint or `rg` against `target =` in `#[instrument]` and span macros across `crates/mango-*/src/`) validates that no mango source file declares a non-`mango.*` target. Operationalizes the Operability stable-target-names bar.
- [ ] **CI duration-budget workflow** `.github/workflows/ci-duration-budget.yml`: collects job durations from the last N main-branch CI runs; fails the next PR if the rolling average cold time exceeds 5 min or warm time exceeds 90s. Operationalizes the Developer-ergonomics CI-time bar.

## Phase 12 — Release engineering

Make mango installable.

- [ ] `cargo install`-able crates + binary publishing to crates.io (workspace publish ordered correctly)
- [ ] GitHub Release workflow: cross-compile `mango` and `mangoctl` for `x86_64-linux-gnu`, `aarch64-linux-gnu`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`; attach tarballs + checksums + signatures + SBOM
- [ ] **SLSA Level 3 provenance attestations** for every release artifact (binary, container, SBOM) via `slsa-github-generator`. For a database that downstream users put critical data into, supply-chain attestation is the difference between "trust us" and "verifiable build." Verified by `slsa-verifier` in the release smoke test before publication.
- [ ] Multi-arch `Dockerfile` and image push to GHCR
- [ ] Versioning: SemVer + `CHANGELOG.md` updated per release
- [ ] **On-disk format versioning**: a `data-dir/VERSION` file declares the on-disk format. Mango refuses to start against a newer-format dir or a too-old-format dir, with an actionable error; `mangoctl migrate <data-dir>` performs forward migrations. CI runs an upgrade matrix (N → N+1) on a populated cluster before every release.
- [ ] **Hot-restart / rolling-upgrade SLA**: a 3-node cluster can be rolling-restarted with no client-visible downtime; tested in CI by a workload runner that asserts zero failed Puts during the upgrade. This makes etcd's "informally works" into a tested guarantee.
- [ ] **Positioning-claim consistency gate — design task**. Goal: a CI workflow that fails on any release-tag PR where README's positioning table, ROADMAP's bar #7, and the latest `benches/results/phase-14.5/*.md` headline numbers do not triple-match. **Design output (lands before implementation, in `.planning/phase-12/positioning-claim-gate.md`)**: (a) machine-readable schema for `benches/results/**/*.md` headline numbers (frontmatter or fenced JSON block); (b) metric-ID convention used in both bench-result files and `<!-- metric-id: ... -->` annotations on README/ROADMAP figures so the gate has a deterministic mapping; (c) tolerance policy (e.g. quoted figure must be ≤ measured figure when prefixed `≥`, within ±5% otherwise); (d) prose coverage rules (which paragraphs of README and ROADMAP are in scope; explicitly include README's "Tier 2 targets" prose section, not just the table); (e) etcd-oracle exemption (etcd numbers come from `benches/oracles/etcd/` runs, not mango benches). Implementation is a follow-up PR. Reason for the design-first split: the original framing of this item shipped as workflow-shaped aspiration without specifying any of the above, and was caught as such on second-pass review. Designing the gate before building it forces the schema decisions out of the workflow author's head and into a reviewable artifact.
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

- [ ] Extend the Phase 5 `madsim`-based simulator to model the full server (KV + Watch + Lease + Raft + storage) under deterministic time / network / RNG, not just Raft alone. `mango-server` and `mango-client` build under `cfg(madsim)` per the Phase 0.5 workspace adoption; the full-server simulator wires them together and exposes a `Cluster::new(seed)` API for property-test authors. Every fault-injector knob below operates against the simulated network / disk / clock, so test runs are bit-for-bit reproducible from a seed.
- [ ] Fault injector: drop / delay / duplicate / reorder messages, kill processes mid-fsync / mid-snapshot-install / mid-leader-elect / mid-compaction, partition the network with one-way / asymmetric / flaky links, corrupt individual disk pages, return `EIO` from any syscall, clock skew between nodes
- [ ] Linearizability checker (Porcupine-style or wrap an existing crate) over recorded histories; runs on every simulator trace
- [ ] Long-running fuzz harness: random workload + random faults; CI nightly job runs it for ≥30 minutes per seed across ≥10 seeds in parallel; failures auto-file a GitHub issue with the seed
- [ ] **Differential fuzzing against etcd**: for the KV / Watch / Lease surface, fuzzer generates an operation sequence, replays it against both mango and a pinned etcd v3.5.x instance from `benches/oracles/etcd/`, and asserts equivalent observable behavior (same keys, same revisions modulo a documented offset, same Watch event ordering, same error categories). Any divergent response — beyond the documented intentional differences in `docs/etcd-divergence.md` — is a bug. Catches semantic drift the conformance suite (Phase 13.5) misses because it tests against a static spec, not against living etcd behavior.
- [ ] **Public Jepsen run published in CI**: real Jepsen test driving real mango binaries; results uploaded as a GitHub Pages site so claims about correctness are externally verifiable
- [ ] Document failure modes found and fixed in `docs/robustness/`

## Phase 13.5 — Conformance suite

Without a conformance suite, the post-1.0 stretch goals (embedded mode,
pluggable consensus) have no guardrail when they land. Pinning the
semantic contract now means every future implementation must pass the
same test gauntlet mango itself does.

- [ ] `crates/mango-conformance` — a standalone crate that runs a defined set of KV / Watch / Lease / Raft semantic assertions against any binary that speaks the mango `.proto`. Reference implementation = mango itself; pluggable consensus and embedded mode (stretch) MUST pass it before claiming compatibility.
- [ ] Test categories: KV linearizability, Watch event ordering and at-least-once delivery, Lease expiry timing within tolerance, Txn compare-and-swap semantics, Range pagination edge cases, error-shape stability.
- [ ] **Ported etcd integration tests** under `crates/mango-conformance/tests/etcd-ported/`. The etcd repo's `tests/integration/` and `tests/e2e/` describe behavior, not implementation, and most cases translate to mango's typed client (the test asserts "Put then Get returns the value"; the language is irrelevant). Initial port targets the `clientv3/` integration tests for KV, Watch, Lease, Auth, and Maintenance. Each ported test carries a `// PORTED FROM: etcd@<sha>:<path>:<test_name>` provenance comment; a CI job (`scripts/check-etcd-port-provenance.sh`) asserts every file under `etcd-ported/` has the provenance line. **Three provenance states**: (a) referenced sha matches `benches/oracles/etcd/VERSION` → green; (b) referenced sha is _older_ than the pinned etcd version → CI emits a `divergence-audit-needed` warning (does not block) requiring a human to confirm the test still tracks current etcd behavior or to re-port from the newer sha; (c) referenced sha is unknown to the pinned etcd repo → CI fails. **Tests that fail because mango is intentionally divergent are explicitly skipped** with the divergence documented in `docs/etcd-divergence.md` (matches the Phase 13 differential-fuzz divergence file). **Honest cost note**: Go-to-Rust test translation is not free — initial port estimated at ~2 engineer-weeks for the `clientv3/` core; expanded surface (auth, maintenance, e2e) is iterative. This is still cheaper than re-discovering the test cases, which is the whole point.
- [ ] **Jepsen tests running against mango**, under `tests/jepsen/`. **Honest framing**: mango is not wire-compatible with etcd, so we cannot run `jepsen.etcd` unmodified — its client speaks etcd's `etcdserverpb`. The port is **"reuse Jepsen's nemesis library, generator combinators, and the Knossos / Elle linearizability checkers; write a fresh Clojure client against mango's `.proto`"**. The fresh-Clojure-client cost is **~1-2 engineer-weeks of Clojure for a Rust team** (gRPC + Clojure is well-trodden via `protojure`, but it is real engineering, not gluework). The alternative is **Jepsen's `local` / SSH mode driving a Rust test binary that speaks mango's gRPC client directly**, which keeps the Rust team in Rust at the cost of losing Clojure's nemesis ergonomics. **The ADR in `.planning/adr/0013-jepsen-integration.md` picks one path and documents the trade.** Either way, runs alongside the Phase 13 mango-native simulator-driven property tests; Jepsen's value is not the language — it is "the same property checker that broke etcd in 2014 cannot break mango in CI." Results published to the same GitHub Pages site as the native simulator results.
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
- [ ] **Bounded-staleness follower reads** — **per-RPC opt-in only, never default**. Client passes `MaxStaleness(d)` on a `Range` request; the follower refuses to serve if its applied-index lag exceeds the bound, and the response carries the actual staleness measured at serve time. Documented as a _weakening of linearizability_ in `docs/consistency.md`; explicit warning that operators must NOT enable it globally for systems (Kubernetes, controllers) that depend on linearizable etcd reads. Etcd has no first-class equivalent; this is a real differentiator if shipped responsibly.
- [ ] **Large-dataset bench** `benches/runner/large-dataset.sh`: load 8 GB / ≥ 100M revisions; measure range-query latency, compaction wall-clock, snapshot wall-clock, defrag wall-clock. Compare to etcd at the same dataset size. Operationalizes Large-scale-distributed bar #3. Numbers in `benches/results/phase-14/large-dataset.md`.
- [ ] **Watcher-scale bench** `benches/runner/watcher-scale.sh`: open 100k concurrent watchers on a single server under a 1k-events/sec write workload. Assert: stable RSS, bounded CPU, p99 event-delivery latency ≤ 100ms. Operationalizes Large-scale-distributed bar #2. Numbers in `benches/results/phase-14/watcher-scale.md`.
- [ ] Final integrated bench, runner script `benches/runner/ycsb.sh`: YCSB-A,B,C,D,E,F on a 3-node cluster against the pinned etcd in `benches/oracles/etcd/` on the hardware sig in `benches/runner/HARDWARE.md`. **Realistic acceptance bar: mango wins on YCSB-A (write-heavy) and YCSB-F (read-modify-write) throughput by ≥ 1.3×; ties or wins on YCSB-B/C/D/E throughput within ±10%; wins on p99 latency on at least 4 of the 6 workloads at 50% saturation.** The two workloads where mango may lose are documented with the structural reason in `benches/results/v0.1.0.md`. ("Wins on every workload" is fan-fic; etcd has been profiled by experts for a decade. We win where we have a structural edge — write-heavy paths via Rust + pipelined Raft + better storage engine — and we're honest about read-only point-lookups at small values, which favor bbolt's mmap'd B+tree.)

## Phase 14.5 — Read-scale-out (Tier 2)

The single biggest scale lever an etcd-shaped system has not used.
Single-Raft-group writes are bounded by quorum (~50-200K writes/sec
ceiling — that's physics, not implementation). But **reads can be
served from any replica that has caught up**, and most KV workloads
are read-heavy (typical 80/20, often 95/5 in K8s-class deployments).
This phase ships the Tier 2 north-star bars: a 5-voter + 5-learner
cluster delivers ≥ 1M ops/sec on an 80/20 mix in **bounded-staleness**
read mode (**up to ~2× over etcd's serializable-read ceiling** of
~500K-1M ops/sec; the win grows on read-heavier mixes — see the 95/5
bullet in bar #7), and ≥ 600K ops/sec on the same mix in
**linearizable ReadIndex** mode (**~5-10× over etcd's ReadIndex
ceiling** of ~50-150K ops/sec, with strong consistency preserved
end-to-end). The two ratios differ because etcd is far closer to the
bandwidth ceiling on serializable reads than on linearizable ones,
so the win is naturally larger in the stronger consistency mode.
Lands pre-1.0; v1.0 is the Tier 2 release.

Sequenced after Phase 14 (perf push) so single-node hot-path
optimizations are in place, and after Phase 9 (membership +
`promotable: bool` ConfChange) so learner promotion / removal /
read-replica-state is real. The single-node read-path enablers
(sharded `KeyIndex`, lock-free snapshot reads) live in Phase 2 —
they are required by Phase 6's per-core scaling bar and so cannot
wait until 14.5.

**Note on terminology**: "_sharded_" in this phase (e.g., the
sharded `KeyIndex` referenced from Phase 2) means **in-process
concurrent-map sharding for parallel reader access** — unrelated to
the multi-shard cluster topology of "Tier 3," which is explicitly
not on the roadmap.

### Read-only learner replicas as a first-class deployment mode

- [ ] **`mango-server --role=read-replica`** joins the cluster as a non-voter that does not count toward quorum. Same Raft log replication as a learner (Phase 9), but registered with `promotable: false` in the `ConfChange` so the cluster state machine itself rejects accidental promotion (not just a server-side flag). Documented as the Tier 2 read-scale-out primitive. Depends on the `promotable: bool` ConfChange item in Phase 9.
- [ ] **Linearizable reads on read-replicas via ReadIndex**: replica accepts `Range` requests, sends a ReadIndex request to the leader, waits for its applied-index ≥ leader's commit-index at the time of the request, then serves locally. Adds one round-trip to read latency vs leader-served reads. Per-RPC the client can request "linearizable" (default) or "bounded-staleness" (Phase 14 follower-reads).
- [ ] **`mangoctl member add --read-replica`** and matching `mangoctl member demote-to-read-replica` / `promote-to-voter`. **Demoting a voter to read-replica reduces standing fault tolerance** (5 voters tolerate 2 failures; 4 voters tolerate 1). The CLI surfaces this in the confirmation prompt (showing before/after voter count, before/after fault tolerance, and the cluster's `quorum_floor`) for UX. **Enforcement of the voter floor lives in the Phase 9 `ConfChange` apply path**, not in the CLI — so a direct gRPC client, a third-party ops tool, the web UI, or an older `mangoctl` cannot bypass it. Promoting back to voter is the same code path as learner promotion (and requires `promotable: true`, which a demoted-voter inherits — read-replicas added via `member add --read-replica` get `promotable: false` and need a separate config-change to make them eligible).

### Recent-revision cache (server-side)

- [ ] **In-memory LRU of the last N revisions** (default N = 10K, configurable) in front of the backend, in the read path of `Range` and `Get`. Cache key is `(key, revision)`; cache value is the materialized response. Hit rate ≥ 80% on the typical "read-after-write" workload (clients reading their own recent writes).
- [ ] **Metric cardinality discipline**: metrics on the recent-revision cache are bounded to `mango_mvcc_recent_revision_cache_{hits,misses,evictions,size}` — no per-key, per-revision, or per-client labels, ever. Verified by the metric-cardinality test in `tests/observability/metric_cardinality.rs` (Phase 11).
- [ ] **Server-side compaction-driven invalidation**: when MVCC compacts past a revision present in the cache, the cache evicts every entry at or below that revision **before** `Compact` returns to the client. Test: `tests/mvcc/recent_revision_cache_invalidation.rs`.
- [ ] Bench: cold-cache vs warm-cache p99 read latency, expected ≥ 5× speedup on the cache-hit path. Numbers in `benches/results/phase-14.5/recent-revision-cache.md`.

### Client-side cache with watch-driven invalidation

- [ ] **Typed `WatchedCache<K, V>` in `mango-client`**: holds a key (or range) locally; opens a Watch against the server; serves `Get` from the local cache at the cache's _effective revision_ `R_c`, which lags server-now by at most N events (where N is the per-watcher channel depth). The cache contract is "consistent at `R_c`, not at server-now" — documented in `mango-client` rustdoc and in `docs/consistency.md` as a relaxation that operators must explicitly opt into per cache instance.
- [ ] **Memory bounds**: `WatchedCache::builder()` requires `max_entries` and `max_bytes` (defaults: 100k entries, 256 MiB). Eviction policy is LRU on entries, hard cap on bytes. On eviction of a key that is currently watched, the cache transitions that key to "miss-through" mode (every `Get` goes to wire) until the next watch event re-populates. Without this, a client watching `--prefix /` over a 100M-revision dataset OOMs — exactly the same class of bug as etcd's "watcher channel grows unbounded," moved to the client.
- [ ] **Property test 1 — `tests/cache/watch_invalidation.rs::cache_server_equivalence_at_rc`**: under random `Put`/`Delete`/`Watch`/`Get` sequences, **for every key K and every cache-served read at the cache's effective revision `R_c`, the cache's response equals the server's response for K at `R_c`. The cache's effective revision lags server-now by at most N events.** This is the corrected property — the previous "no stale-positive" wording contradicted the cache's own contract.
- [ ] **Property test 2 — `tests/cache/watch_invalidation.rs::reconnect_with_compaction_invalidates`**: watcher disconnects; server compacts past `cache.last_revision`; watcher reconnects and receives `ErrCompacted`. The cache MUST invalidate all entries (transition to empty, not stale-pinned) and re-bootstrap from a fresh server snapshot. Without this the cache serves a revision the server can no longer prove correct.
- [ ] **Property test 3 — `tests/cache/watch_invalidation.rs::ordering_preserved_across_reconnect`**: simulate watch disconnect with in-flight `Get`s and a concurrent reconnect-then-event burst. Assert that no `Get` returns a value strictly older than a previously returned value for the same key from the same `WatchedCache` instance (per-key monotonic-read consistency at the client).
- [ ] **Property test 4 — `tests/cache/watch_invalidation.rs::leased_keys_are_cache_bypass`**: a leased key disappears via lease expiry (a separate event class from `DeleteRange` that is not always carried on a Watch stream for the affected key). To avoid stale-positive results after lease expiry, **`WatchedCache` does not cache leased keys at all** — every `Get` for a key currently associated with a lease goes to wire. Implementation: on cache insert, check the response's `lease` field; skip the cache path if non-zero. The test asserts (a) a leased key never appears in the cache's internal map, (b) repeated `Get`s on a leased key always hit the wire, (c) when a lease expires server-side the cache observes the consequent `Get` returning "not found" without any internal invalidation step. (Subscribing to lease events is the alternative implementation; cache-bypass is simpler, has no edge cases around watch-reconnect, and is correct by construction. Picking one path here so the test asserts a single behavior, not a behavioral OR.)
- [ ] **Chaos test — `tests/chaos/watched_cache_partition.rs`**: partition the watcher's connection for 30s under sustained writes, then heal. Assert: cache's view at heal-time is either fully refreshed or in miss-through mode for the affected keys; no key returns a value older than the partition window.
- [ ] **Memory-bound test — `tests/cache/memory_bound.rs`**: fill cache to 2× `max_entries` over a 1-hour synthetic run; assert RSS bounded by `max_bytes × 1.5` (1.5× allowance for allocator slop), zero panics, and eviction hit rate stays within 5% of the LRU theoretical optimum.
- [ ] Bench: 90% read / 10% write workload with `WatchedCache` for the read path; cache hit rate ≥ 90% on the hot key set; effective read throughput at the client is **≥ 10× direct-Range throughput** because most reads never hit the wire.

### Acceptance: the Tier 2 north-star bar

The acceptance benches close with a process commitment: if any bar
slips at bench time, the bar is restated _in the same release_ as the
evidence — README's positioning table and ROADMAP bar #7 update
together. No quiet downgrade.

- [ ] **Hardware prerequisite (mandatory)**: `benches/runner/HARDWARE.md` for the Tier 2 acceptance benches **MUST** provide **≥ 25 GbE** intra-cluster bandwidth. At 200K writes/sec × 1KB × 9 receivers (4 followers + 5 learners) the leader's WAL egress is ~1.8 GB/s, which saturates 10 GbE after framing — Raft log compression and pipelined replication reduce per-byte cost but do not eliminate the bandwidth floor. **10 GbE bench runs are diagnostic only and do not constitute a Tier 2 release-gate measurement.** The release-tag workflow (Phase 12 positioning-claim consistency gate) verifies the bench results' hardware sig against `HARDWARE.md` and fails if a 10 GbE run is referenced as a Tier 2 acceptance number.
- [ ] **`benches/runner/read-scale-out.sh --read-mode=bounded-staleness`**: 5-voter + 5-learner cluster on the canonical hardware, 80/20 read/write workload at 1KB values, bounded-staleness reads on the learner tier; **delivers ≥ 1M ops/sec sustained**, p99 latency ≤ 50 ms. Numbers in `benches/results/phase-14.5/read-scale-out-bounded-staleness.md`. Operationalizes Large-scale-distributed Tier 2a bar #1.
- [ ] **`benches/runner/read-scale-out.sh --read-mode=bounded-staleness --mix=95/5`**: same topology, 95/5 read/write mix (the K8s operator workload); **delivers ≥ 1.5M ops/sec sustained**, p99 latency ≤ 30 ms. Numbers in `benches/results/phase-14.5/read-scale-out-95-5.md`. Operationalizes Tier 2a bar #2.
- [ ] **`benches/runner/read-scale-out.sh --read-mode=linearizable`**: same topology, 80/20 mix, linearizable ReadIndex on the learner tier (every read pays a leader-confirm round-trip); **delivers ≥ 600K ops/sec sustained**, p99 latency ≤ 100 ms. Numbers in `benches/results/phase-14.5/read-scale-out-linearizable.md`. Operationalizes Tier 2b bar.
- [ ] **`benches/runner/learner-scale.sh`**: same hardware, sweep learner count 1..10, bounded-staleness mode, measure read throughput at each step. **Throughput at N learners ≥ 0.7 × N × (single-voter read throughput), up to N = 7 learners.** Past N=7 the leader becomes the membership-and-replication bottleneck; the bench documents the actual flatline point. Numbers in `benches/results/phase-14.5/learner-scale.md`. Operationalizes the linear-scaling Tier 2 bar.
- [ ] **`tests/chaos/leader_kill_during_read_scaleout.rs`**: kill the leader at t=30s during a steady read-scale-out bench; assert read throughput recovers to ≥ 80% of steady-state within 5s, no read returns an error other than `Unavailable` (retryable), no read returns stale data past `MaxStaleness` bound.
- [ ] **`tests/chaos/learner_partition.rs`**: partition one learner at t=30s for a 60s window; assert (i) the partitioned learner refuses linearizable reads with a typed `Unavailable` error rather than serving stale, (ii) on rejoin it catches up without flooding the leader (per the Phase 5 catch-up traffic bound), (iii) **total cluster throughput, measured as the rolling 10s average between t=partition+10s and t=partition+50s (excluding the first 10s of client-rebalance transient and the last 10s before heal), degrades by ≤ 1/N from steady-state**, where N is the active learner count. The 10s exclusion at start is required because in-flight requests to the partitioned learner time out, retries fan out, and p99 spikes — none of which is a steady-state signal.

## Phase 15 — Hardening

Production-grade means assume the worst about the network, the disk, and
the operator. This phase makes mango refuse to lose data even when those
assumptions are violated.

- [ ] **CI plumbing for the per-phase fuzz targets** added in Phases 2 / 5 / 6 / 10 (MVCC key codec, WAL record decoder, `.proto` decoders, config TOML parser, gRPC body decoders, snapshot decoder): nightly job runs each for ≥ 30 minutes per seed across ≥ 10 seeds in parallel, with persistent corpora under `fuzz/corpus/<target>/`. Failures auto-file a GitHub issue with the seed and the crashing input. Optional OSS-Fuzz integration as a follow-up.
- [ ] Audit pass: every state machine (Raft state transitions, lease state, MVCC visibility, watcher state) has property tests; backfill any phase that shipped without them (the Definition of Done says they're required, but this is the explicit verification step).
- [ ] **Disk corruption detection** + named test `tests/security/disk_corruption.rs`: every backend write is checksummed (XXH3 or BLAKE3); reads verify; mismatch raises `CORRUPT` alarm and refuses to serve stale-checksum pages. Test injects bit-flips at various offsets and asserts every flip is detected.
- [ ] **Anti-entropy**: periodic cross-replica HashKV check; mismatch raises `CORRUPT` alarm and pinpoints the diverging key range
- [ ] **Crypto zeroize-on-drop test** `tests/crypto/zeroize.rs`: assert every secret-bearing type uses `secrecy::Secret<T>` (or equivalent zeroizing wrapper); a Miri-backed test verifies the underlying bytes are zeroed when the wrapper is dropped. Operationalizes the Security cryptographic-correctness bar.
- [ ] **Constant-time-comparison test** — split into PR-time source check and release-gate timing measurement, because statistical timing tests are CI-flaky on shared runners (jitter from neighbors, thermal throttling, scheduler effects):
  - **PR-time source check** `tests/security/constant_time.rs`: for every `==` on `&[u8]` in `auth*` / `crypto*` / `token*` / `hash_chain*` modules, assert the underlying call goes through `subtle::ConstantTimeEq` (compile-time / source-grep equivalent of the Phase 0.5 dylint). Fast, deterministic, CI-safe.
  - **Release-gate timing measurement** `tests/security/constant_time_timing.rs` (separate workflow `.github/workflows/timing-bench.yml`, runs on a self-hosted bare-metal runner, gated as part of the 7-day chaos run): the actual statistical timing test with **100k samples per prefix length**; asserts timing-distribution overlap within 2σ. Failures block the release; PR CI does not depend on it. Matches how rustls / ring test constant-time: source verification per PR, occasional bare-metal timing measurement as a release gate.
    Operationalizes the Security side-channel bar with the appropriate split between cheap (per-PR) and expensive (release-gate) enforcement.
- [ ] **Single-node failure chaos test** `tests/chaos/single_node_failure.rs`: under sustained Put load, `SIGKILL` + `kernel_panic` (via `/proc/sysrq-trigger c` in a container) + `disk_yank` (unmount the data-dir filesystem) one node at a time; assert the cluster's recovery time to the same workload is ≤ 0.7× etcd's. Operationalizes Reliability bar #5.
- [ ] **Disk-EIO chaos test** `tests/chaos/disk_eio.rs`: inject `EIO` on every syscall family the backend uses; assert no silent data loss, no panic, every error is reported through the typed error enum.
- [ ] **Memory profiling under load**: Massif / dhat profile, no leaks, RSS bounded under sustained load — see the long-running chaos item below for the required duration.
- [ ] **Continuous chaos runner** `tests/chaos/long_running.rs` + `.github/workflows/chaos-continuous.yml`: real cluster, real network, random faults via toxiproxy or equivalent; **runs continuously on a dedicated self-hosted chaos runner**, not on shared CI. The runner consumes the latest `main` commit on rotation and emits a "clean since `<sha>`" signal as the chaos workflow's status. Fails on any panic from non-test code (mechanically enforces the north-star "no panics in steady state" claim). **Release-type policy**: scheduled major / minor releases require a **≥ 7-day-clean signal** (release commit must be N where the chaos runner has been clean from M ≤ N for ≥ 7 days). Hotfix releases require only the 1-hour weekly chaos regression gate (below) plus the per-PR layers; the next scheduled release post-hotfix re-establishes the 7-day window. Documented in `docs/release-process.md`.
- [ ] **Weekly chaos regression gate** in shared CI: same harness as the continuous runner, runs for ≥ 1 hour every week against `main`. Failures block the next release (any type) until resolved. This is the affordable per-week sanity check; the continuous runner is the high-confidence release gate.
- [ ] **Simulator panic-hook test** `tests/simulator/panic_hook.rs`: install a panic hook that captures the seed and stack on any panic from a non-test crate; the simulator's nightly fuzz job fails if any seed produces a panic. Operationalizes the Safety + Correctness panic-hook claims.
- [ ] **Security review**: third-party (or at minimum, sensitive-data-auditor + security-reviewer subagents) review of the full surface before 1.0
- [ ] **Threat model document** in `docs/security/threat-model.md` covering the trust boundaries (client ↔ server, peer ↔ peer, operator ↔ disk) and mitigations; every threat has a named mitigation and a named test (or an explicit "accepted risk" justification).

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
- [ ] **Server-side session revocation list in the Raft state machine** (so it survives leader change). UI **access tokens are short-lived** (15 min default — long enough that a brief network hiccup doesn't log the operator out mid-edit, short enough that a stolen token has a small window; 8 h refresh token max). Tokens carry a session-id claim. Every backend request consults the revocation set at the auth interceptor — _not_ just at the resource check. **Refresh requests also consult the revocation set; a refresh issued for a revoked session-id returns 401 immediately**, otherwise revoking a session would silently resurrect for another access-token TTL. "Log out everywhere" adds the user's session-ids; admin "revoke user" — which in Phase 16 means the `mangoctl auth revoke <user>` command from Phase 8, becoming the in-UI action when Phase 16.5 ships — adds all of that user's active session-ids atomically with the user-removal. Tested: revoke admin user, assert their in-flight UI session can no longer make any mutating call within one revocation-propagation tick, and assert their next refresh attempt is rejected.
- [ ] **RBAC enforcement in the UI mirrors the backend** — and the UI treats the button-disabled state as **cosmetic and best-effort**; the authoritative check is the backend on every request. Three property tests:
  - **(a)** every UI mutating action that the backend would reject is also disabled in the UI for that user (UI-too-permissive guard);
  - **(b)** every action enabled in the UI for a user is accepted by the backend (UI-too-restrictive guard);
  - **(c) revocation race**: a user whose role is revoked mid-session has every cached-permitted button rejected by the backend within the access-token TTL (≤ 5 min), regardless of UI cache state. This is the test that catches the dangerous case.

### Hardening (the UI is a fresh attack surface)

- [ ] **Same security review** as the gRPC surface in Phase 15: sensitive-data-auditor + security-reviewer subagents pass.
- [ ] **CSRF tokens on every mutating endpoint** using the synchronizer-token-bound-to-session pattern; cookie attributes per the login-flow item above.
- [ ] **Explicit Content-Security-Policy**: `default-src 'self'; script-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'; upgrade-insecure-requests`. The exact header is asserted in an integration test. **No third-party CDNs** — everything served from the `mango-ui` binary. **Implementation note**: `style-src` is not listed and falls through to `default-src 'self'`, which is the strictest behavior; the chosen frontend stack must therefore avoid inline `style="..."` and `onclick="..."` attributes — if the stack requires them, the CSP needs a hash or nonce mechanism added.
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
