# mango roadmap

A ground-up Rust port of [etcd](https://github.com/etcd-io/etcd). Mango is
**not** wire-compatible with etcd — we own our `.proto` files and design a
clean Rust-native API. etcd is the reference implementation we study; we are
not bound by its Go-isms.

## Working rules

- One checked item per PR. Small, atomic, mergeable. No mega-PRs.
- Every phase ends with `cargo test --workspace` green and the new behavior
  exercised by tests (unit + integration where appropriate).
- The relevant expert agent (currently `rust-expert`) reviews both the plan
  and the final diff. No merge without `APPROVE`.
- Items inside a phase are roughly ordered by dependency. Phases are strictly
  ordered: don't start phase N+1 until phase N's checked items are done
  unless the items are explicitly independent.

## Out of scope (for now)

- Wire compatibility with real etcd's `etcdserverpb` (clients written for
  etcd will not work against mango).
- gRPC gateway / HTTP+JSON transcoding.
- gRPC proxy.
- v2 store / v2 API (etcd's deprecated legacy API).
- Multi-language client SDKs beyond Rust. (A second-language client is a
  post-1.0 stretch goal.)

If any of these become must-haves later, add them as new phases at the end.

---

## Phase 0 — Foundation

Get the workspace into a state where every subsequent phase can move fast:
deterministic builds, CI on every push, lints, formatting, and a place to
put proto definitions.

- [x] Set up CI (GitHub Actions): `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`, on push and PR
- [ ] Add `rustfmt.toml` and `.editorconfig` so formatting is unambiguous
- [ ] Add `deny.toml` and a `cargo-deny` CI job (license + advisory + duplicate-version checks)
- [ ] Create `crates/mango-proto` skeleton with `tonic-build` and a hello-world `.proto` that compiles
- [ ] Add `CONTRIBUTING.md` covering branch naming, commit style, PR template, and the test bar

## Phase 1 — Storage backend (single-node, no MVCC yet)

A durable, transactional, ordered-key K/V store. This is the equivalent of
etcd's `mvcc/backend` layer that wraps bbolt — we pick the Rust analogue and
abstract it behind a `Backend` trait. No revisions yet; that lives in
phase 2.

- [ ] Choose the storage engine (sled / redb / rocksdb / hand-rolled) — write an ADR in `.planning/adr/` after the rust-expert weighs in
- [ ] `crates/mango-storage` skeleton with the chosen engine as a dependency
- [ ] Define `Backend` trait: `begin_read()`, `begin_write()`, named buckets/trees, `put`, `get`, `delete`, `range`, `commit`, `force_commit`
- [ ] Implement `Backend` against the chosen engine, with on-disk durability and `fsync` semantics matching etcd's batch-tx model (commit on N writes or T millis)
- [ ] Property tests: random put/get/delete/range sequences match an in-memory `BTreeMap` oracle
- [ ] Crash-recovery test: kill mid-write via a panic, reopen, assert no torn state and no committed data lost

## Phase 2 — MVCC layer

etcd's MVCC: every write produces a new revision; keys are addressed by
`(key, revision)`; tombstones; compaction. Built on top of the phase-1
backend.

- [ ] Define `Revision { main: i64, sub: i64 }` and the on-disk key encoding (`key_index` + `key`-bucket layout, mirror etcd's split conceptually)
- [ ] Implement `KeyIndex` (in-memory tree of keys → list of generations of revisions) with put / tombstone / compact / restore-from-disk
- [ ] Implement the MVCC `KV` API: `Range`, `Put`, `DeleteRange`, `Txn` (compare + then/else ops), `Compact`
- [ ] Read transactions return a consistent snapshot at a chosen revision
- [ ] Compaction: physically removes old revisions; `Range` against a compacted revision returns `ErrCompacted`
- [ ] Property tests: random ops + random snapshot reads match a model implementation
- [ ] Restore-from-disk test: persist via backend, drop the in-memory index, reopen, all reads consistent

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

- [ ] ADR in `.planning/adr/` choosing the Raft implementation (rust-expert decides at plan-review time)
- [ ] `crates/mango-raft` skeleton with the chosen crate (or hand-rolled module structure)
- [ ] Single-node Raft: proposals get applied to a state-machine trait; the state-machine is wired to the MVCC store
- [ ] WAL: append every entry before applying; replay on startup
- [ ] Snapshot: state-machine snapshot + WAL truncation; reload on startup if WAL gap
- [ ] 3-node cluster over TCP transport: leader election, log replication, follower catch-up
- [ ] Linearizable reads via ReadIndex (no stale reads from followers without quorum-check)
- [ ] Network-partition tests: 2/1 split, 1/1/1 split, leader isolation; assert no split-brain, no lost committed entries
- [ ] Crash-recovery tests: kill follower mid-replication, kill leader mid-commit, restart, cluster converges

## Phase 6 — gRPC server: KV + Watch + Lease

Wire phases 2–5 to the network. `mango-server` hosts the gRPC services and
is the binary you actually run.

- [ ] Author `.proto` for KV, Watch, Lease (Rust-native shape; copy etcd's semantics, not its message names)
- [ ] `crates/mango-server`: KV service backed by Raft-replicated MVCC
- [ ] Watch service: server-streaming RPC backed by phase-3 `WatchableStore`
- [ ] Lease service: unary + bidi `LeaseKeepAlive` stream backed by phase-4 `Lessor`
- [ ] Health and `Status` endpoints (cluster ID, member ID, leader, raft index, db size)
- [ ] Configuration via TOML file + CLI flags + env (precedence: CLI > env > file > default)
- [ ] Integration tests: spin up a 3-node mango cluster in-process, run KV + Watch + Lease scenarios end-to-end

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
spans on every RPC and Raft action.

- [ ] `tracing` + `tracing-subscriber` wired across every crate (`mango.server`, `mango.raft`, `mango.mvcc`, `mango.lease`, `mango.watch` targets)
- [ ] Prometheus exposition on `/metrics` covering request counts/latencies per RPC, Raft proposals/leader changes, MVCC db size + revision, lease counts, watcher counts
- [ ] Per-RPC `#[instrument]` spans with stable field names; spans propagate through `spawn_blocking` correctly (capture `Span::current()` and re-enter)
- [ ] Optional OpenTelemetry OTLP exporter behind a feature flag
- [ ] Sample Grafana dashboard JSON committed to `dashboards/`
- [ ] Tests: hit the server, scrape `/metrics`, assert expected metric families exist with expected labels

## Phase 12 — Release engineering

Make mango installable.

- [ ] `cargo install`-able crates + binary publishing to crates.io (workspace publish ordered correctly)
- [ ] GitHub Release workflow: cross-compile `mango` and `mangoctl` for `x86_64-linux-gnu`, `aarch64-linux-gnu`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`; attach tarballs + checksums + signatures
- [ ] Multi-arch `Dockerfile` and image push to GHCR
- [ ] Versioning: SemVer + `CHANGELOG.md` updated per release
- [ ] `0.1.0` release tag

## Phase 13 — Robustness testing

etcd has a famous robustness test suite (Jepsen-style: random failures + linearizability checking). We need our own.

- [ ] Deterministic simulator: fake clock + fake network so Raft tests are reproducible from a seed
- [ ] Fault injector: drop / delay / duplicate messages, kill processes, partition the network, corrupt disk pages
- [ ] Linearizability checker (Porcupine-style or wrap an existing crate) over recorded histories
- [ ] Long-running fuzz harness: random workload + random faults; CI nightly job runs it for N minutes per seed
- [ ] Document failure modes found and fixed in `docs/robustness/`

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
