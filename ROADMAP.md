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

The six axes mango must beat etcd on:

1. **Performance** — sustained write throughput meaningfully above etcd's
   ~10k writes/sec single-node bench; lower p99 client latency at p50 load
   and at saturation; smaller resident memory; faster cold start; faster
   leader-failover recovery time.
2. **Correctness** — Jepsen-grade linearizability properties verified by
   our own deterministic-simulation testing harness from Phase 13 *fed by
   property tests written alongside every phase*; explicit safety
   arguments for every concurrency primitive; formal-ish models where the
   protocol demands them (Raft state machine, MVCC visibility rules).
3. **Operability** — better default observability (structured tracing
   spans, OpenTelemetry-native, richer + more cardinality-safe metrics);
   simpler config with stronger validation; faster + safer recovery
   procedures; smaller blast radius on partial failures (no thundering
   herds on follower restart, no leader-flap storms during membership
   change).
4. **Safety** — `unsafe_code = "forbid"` workspace-wide except in
   audited, named modules with documented invariants; supply-chain
   hardening (SHA-pinned deps, `cargo-deny`, SBOM); zero panics in
   steady-state code paths (panic == bug == test); explicit failure
   modes returned as typed errors, never propagated as `Box<dyn Error>`.
5. **Developer ergonomics** — Rust client API that is strictly nicer than
   etcd's gRPC stubs (typed responses, async-first, no leaky proto
   types); CI under 5 min cold and under 90 s warm at every phase
   boundary; one-command local cluster bring-up.
6. **Storage efficiency** — smaller on-disk footprint than equivalent
   bbolt-backed etcd at the same data set (block-level compression, key
   prefix dedup, smarter compaction); faster compaction with no read
   stalls.

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
  etcd will not work against mango).
- gRPC gateway / HTTP+JSON transcoding.
- gRPC proxy.
- v2 store / v2 API (etcd's deprecated legacy API).
- Multi-language client SDKs beyond Rust. (A second-language client is a
  post-1.0 stretch goal.)

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

---

## Phase 0 — Foundation

Get the workspace into a state where every subsequent phase can move fast:
deterministic builds, CI on every push, lints, formatting, supply-chain
hardening, and a place to put proto definitions.

- [x] Set up CI (GitHub Actions): `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`, on push and PR
- [ ] Add `rustfmt.toml` and `.editorconfig` so formatting is unambiguous
- [ ] Add `deny.toml` and a `cargo-deny` CI job (license + advisory + duplicate-version checks; ban `git`-deps without explicit allowlist)
- [ ] Add `cargo-audit` CI job (RustSec advisories) running on push, PR, and a nightly schedule; failures block merge
- [ ] Add `cargo-vet` (or equivalent supply-chain audit gate) so every transitive dep has an audit entry; missing audits fail CI
- [ ] Add an SBOM build step (`cargo-cyclonedx`) that produces a CycloneDX file per release; attached to GitHub Releases in Phase 12
- [ ] Add a `cargo-msrv` job pinning the minimum supported Rust version (start at 1.80, bump deliberately) so we don't accidentally raise it
- [ ] Add a `cargo doc --no-deps --document-private-items` job with `RUSTDOCFLAGS=-D warnings` so broken doc links fail CI
- [ ] Add a Renovate / Dependabot config so action SHAs and crate versions get bumped via PR (preserves the SHA-pin policy without it rotting)
- [ ] Create `crates/mango-proto` skeleton with `tonic-build` and a hello-world `.proto` that compiles
- [ ] Add `CONTRIBUTING.md` covering branch naming, commit style, PR template, the test bar, **and the north-star bar**
- [ ] Add a PR template that forces every PR description to declare which north-star axis the change moves and how it was measured

## Phase 1 — Storage backend (single-node, no MVCC yet)

A durable, transactional, ordered-key K/V store. This is the equivalent of
etcd's `mvcc/backend` layer that wraps bbolt — we pick the Rust analogue and
abstract it behind a `Backend` trait. No revisions yet; that lives in
phase 2.

- [ ] Choose the storage engine (sled / redb / rocksdb / hand-rolled) — write an ADR in `.planning/adr/` after the rust-expert weighs in. **Decision criterion: must beat bbolt's published numbers on at least one of (write throughput, read latency at p99, on-disk size for the same dataset, fsync amplification) without losing on the others.**
- [ ] `crates/mango-storage` skeleton with the chosen engine as a dependency
- [ ] Define `Backend` trait: `begin_read()`, `begin_write()`, named buckets/trees, `put`, `get`, `delete`, `range`, `commit`, `force_commit`
- [ ] Implement `Backend` against the chosen engine, with on-disk durability and `fsync` semantics at least as strong as etcd's batch-tx model (commit on N writes or T millis), with the batching parameters tunable
- [ ] Property tests: random put/get/delete/range sequences match an in-memory `BTreeMap` oracle (proptest, 10k+ cases)
- [ ] Crash-recovery test: kill mid-write via a panic, reopen, assert no torn state and no committed data lost
- [ ] Crash-recovery test under simulated fsync failure (return `EIO`) — backend either commits cleanly or reports failure; no silent data loss
- [ ] Bench harness in `benches/storage/`: write-throughput, read-latency, range-scan-throughput, on-disk size after N inserts. Numbers recorded vs bbolt on the same hardware (use a Go binary with bbolt as the comparison oracle, checked into `benches/oracles/bbolt/`). **Mango must win on at least one metric, lose on none.**
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
- [ ] **Online compaction with no read stalls** — etcd's compaction can pause readers; mango compacts in the background with bounded CPU and zero impact on read p99. Bench gate confirms.
- [ ] Property tests: random ops + random snapshot reads match a model implementation
- [ ] Restore-from-disk test: persist via backend, drop the in-memory index, reopen, all reads consistent
- [ ] Bench in `benches/mvcc/`: 10M-key dataset, 80/20 read/write mix, p50/p95/p99 latency and throughput recorded vs etcd's published MVCC numbers. **Mango wins on p99 read latency and on-disk size, ties or wins on write throughput.**

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
- [ ] WAL: append every entry before applying; replay on startup
- [ ] Snapshot: state-machine snapshot + WAL truncation; reload on startup if WAL gap
- [ ] 3-node cluster over TCP transport: leader election, log replication, follower catch-up
- [ ] Linearizable reads via ReadIndex (no stale reads from followers without quorum-check)
- [ ] **Pipelined log replication + batch commit** — one of mango's core perf wins over etcd; bench gate vs single-flight replication baseline
- [ ] **Deterministic simulation testing harness from day one** — fake clock + fake network + seeded RNG; every Raft test in this phase runs in the simulator, not against real wallclock + real sockets. (The Phase 13 robustness work extends this; it does not start it.)
- [ ] Network-partition tests in the simulator: 2/1 split, 1/1/1 split, leader isolation, asymmetric partitions, message reordering; assert no split-brain, no lost committed entries
- [ ] Crash-recovery tests in the simulator: kill follower mid-replication, kill leader mid-commit, restart, cluster converges
- [ ] Bench in `benches/raft/`: 3-node cluster on local loopback, 1KB writes at saturation; p50/p99 commit latency, write throughput, leader-failover-to-quorum-write time. **Mango beats etcd's published 10k writes/sec by ≥1.5x and recovers from leader loss in ≤1.5x its election timeout.**

## Phase 6 — gRPC server: KV + Watch + Lease

Wire phases 2–5 to the network. `mango-server` hosts the gRPC services and
is the binary you actually run.

- [ ] Author `.proto` for KV, Watch, Lease (Rust-native shape; copy etcd's semantics, not its message names)
- [ ] `crates/mango-server`: KV service backed by Raft-replicated MVCC
- [ ] Watch service: server-streaming RPC backed by phase-3 `WatchableStore`
- [ ] Lease service: unary + bidi `LeaseKeepAlive` stream backed by phase-4 `Lessor`
- [ ] Health and `Status` endpoints (cluster ID, member ID, leader, raft index, db size)
- [ ] Configuration via TOML file + CLI flags + env (precedence: CLI > env > file > default), with strict schema validation at startup (reject unknown keys; refuse to start on conflicts)
- [ ] **Graceful shutdown**: SIGTERM drains in-flight RPCs within configurable budget, then exits cleanly; no half-applied Raft proposals
- [ ] **Backpressure everywhere** — every server-streaming RPC has a bounded send buffer with documented slow-consumer policy; no unbounded memory growth under client misbehavior
- [ ] Integration tests: spin up a 3-node mango cluster in-process, run KV + Watch + Lease scenarios end-to-end
- [ ] End-to-end bench at the gRPC boundary: 3-node cluster, real client, 1KB Put @ saturation. **Beats etcd's same-hardware bench by ≥1.5x on throughput and on p99 latency at 50% of saturation.**

## Phase 7 — `mangoctl` CLI client

User-facing CLI mirroring `etcdctl`'s ergonomics: `put`, `get`, `del`,
`watch`, `lease grant/revoke/keep-alive`, `member list/add/remove`,
`endpoint status/health`, `compaction`, `defrag`, `snapshot save/restore`.

- [ ] `crates/mango-client`: typed Rust client over the phase-6 gRPC services
- [ ] `crates/mangoctl` with `clap`-based subcommands and human + JSON output formats
- [ ] `put`, `get`, `del`, `range` subcommands with txn support
- [ ] `watch` subcommand (streaming output)
- [ ] `lease` subcommand group
- [ ] `endpoint status`, `endpoint health`
- [ ] Integration tests against an in-process cluster: every subcommand exercised

## Phase 8 — Authentication & authorization

etcd's auth model: users, roles, role-based key-range permissions, password
auth, token-based session, optional mTLS.

- [ ] `Authenticator` trait + simple-token and JWT-token implementations
- [ ] Users + roles + role permissions persisted in their own buckets, replicated via Raft
- [ ] `Auth` gRPC service: enable/disable, user add/remove/grant-role, role add/grant-permission
- [ ] Authorization middleware on every KV/Watch/Lease op (RBAC over key ranges)
- [ ] mTLS for both client-server (`:2379`-equivalent) and peer-to-peer (`:2380`-equivalent) — cert + key + CA flags wired through config
- [ ] `mangoctl auth` and `mangoctl user` / `mangoctl role` subcommands
- [ ] Tests: authenticated client can read/write, anonymous client rejected, role permission boundaries enforced, mTLS round-trips

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
- [ ] `defrag` (compact the on-disk backend after MVCC compaction)
- [ ] `Maintenance` gRPC service: `Status`, `Snapshot`, `HashKV`, `Defragment`, `Alarm` (NOSPACE / CORRUPT)
- [ ] Quota: refuse writes when DB size exceeds configured quota; raise NOSPACE alarm
- [ ] `mangoctl snapshot save/restore`, `mangoctl defrag`, `mangoctl alarm list/disarm`
- [ ] Tests: snapshot a populated cluster, restore into a fresh node, data identical; quota tripping behavior

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
- [ ] **OpenTelemetry OTLP exporter on by default** (etcd has it as opt-in); behind a config knob to disable but on out of the box
- [ ] Sample Grafana dashboard JSON committed to `dashboards/`, with a "mango vs etcd" comparison panel using the bench harness output
- [ ] **Continuous benchmark CI job**: every merge to `main` runs the Phase 5 / Phase 6 benches and uploads to a tracked baseline; regressions fail the next PR's CI
- [ ] Tests: hit the server, scrape `/metrics`, assert expected metric families exist with expected labels and bounded cardinality

## Phase 12 — Release engineering

Make mango installable.

- [ ] `cargo install`-able crates + binary publishing to crates.io (workspace publish ordered correctly)
- [ ] GitHub Release workflow: cross-compile `mango` and `mangoctl` for `x86_64-linux-gnu`, `aarch64-linux-gnu`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`; attach tarballs + checksums + signatures
- [ ] Multi-arch `Dockerfile` and image push to GHCR
- [ ] Versioning: SemVer + `CHANGELOG.md` updated per release
- [ ] `0.1.0` release tag

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

## Phase 14 — Performance push

A dedicated phase to chase the quantitative "beat etcd" numbers across
the board. Earlier phases set per-feature bench gates; this phase
optimizes against the integrated workload.

- [ ] Profile the integrated 3-node cluster under the YCSB workloads (A/B/C/D/E/F) and produce flamegraphs for each; commit them to `docs/perf/baselines/`
- [ ] Identify the top three CPU and top three latency hotspots; fix each in its own PR with before/after numbers
- [ ] **Zero-copy on the read path**: range responses serialize directly from the backend's mmap'd pages where the engine allows; no intermediate `Vec<u8>` copy
- [ ] **io_uring backend** on Linux behind a config flag (compare to the default async-io path; ship as default if it wins)
- [ ] **NUMA awareness** for multi-socket boxes (pin Raft tick / apply / serve threads sensibly)
- [ ] **Adaptive batching**: batch sizes auto-tune to maintain target p99 latency under varying load (etcd's batching is static)
- [ ] **Read-only follower reads** with bounded staleness, opt-in per request — etcd has nothing like this; clients that can tolerate ms-of-staleness get cluster-scaled reads for free
- [ ] Final integrated bench: YCSB-A,B,C,D,E,F on a 3-node cluster on identical hardware to a published etcd run. **Mango wins on every workload's throughput AND p99 latency. Committed numbers in `benches/results/v0.1.0.md`.**

## Phase 15 — Hardening

Production-grade means assume the worst about the network, the disk, and
the operator. This phase makes mango refuse to lose data even when those
assumptions are violated.

- [ ] `cargo fuzz` targets for every parser surface: `.proto` decoders, config files, snapshot files, WAL records, gRPC request bodies. CI nightly job runs each target for ≥10 minutes; corpora committed under `fuzz/corpus/`
- [ ] Property tests for every state machine (Raft state transitions, lease state, MVCC visibility, watcher state) — proptest with shrinking
- [ ] **Disk corruption detection**: every backend write is checksummed (XXH3 or BLAKE3); reads verify; mismatch raises CORRUPT alarm and refuses to serve stale-checksum pages
- [ ] **Anti-entropy**: periodic cross-replica HashKV check; mismatch raises CORRUPT alarm and pinpoints the diverging key range
- [ ] **Memory profiling under load**: Massif / dhat profile, no leaks, RSS bounded under sustained load — ship a "running for 7 days at 5k writes/sec" stability report
- [ ] **Chaos test in CI** (weekly): real cluster, real network, random faults via toxiproxy or equivalent; failures block the next release
- [ ] **Security review**: third-party (or at minimum, sensitive-data-auditor + security-reviewer subagents) review of the full surface before 1.0
- [ ] **Threat model document** in `docs/security/threat-model.md` covering the trust boundaries (client ↔ server, peer ↔ peer, operator ↔ disk) and mitigations
- [ ] **Backup verification tooling**: `mangoctl snapshot verify` validates a snapshot file's integrity offline before restore

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
