# mango

[![ci](https://github.com/humancto/mango/actions/workflows/ci.yml/badge.svg)](https://github.com/humancto/mango/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

> **Status: pre-alpha.** APIs, schemas, and benchmarks are in flight.
> Do not deploy. See [`ROADMAP.md`](./ROADMAP.md) for the build plan.

A distributed, reliable key-value store written in Rust. Mango is a
ground-up port of [etcd](https://github.com/etcd-io/etcd) — same problem
space, same guarantees (linearizable KV over Raft, watch streams, leases,
MVCC), with a clean Rust-native API. **Mango is not wire-compatible with
etcd**; etcd is the reference implementation we study, not a contract we
preserve.

The website is live at <https://humancto.github.io/mango/>.

## Built to beat etcd on ten measurable axes

Mango is not "etcd, rewritten." It is etcd's problem space attacked with
a language whose primitives lift specific etcd footguns out of existence
at compile time. Every PR is judged against the ten north-star bars in
[`ROADMAP.md`](./ROADMAP.md#north-star-non-negotiable). Each bar has a
named test, a comparison oracle (the **pinned etcd v3.5.x binary** at
`benches/oracles/etcd/`), and a hardware signature.

| #   | Axis                          | What "beats etcd" means here                                                                                      |
| --- | ----------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| 1   | **Performance**               | ≥ 1.5× write throughput, ≤ 0.7× p99 latency, ≤ 0.7× idle RSS, ≤ 0.7× cold start, ≤ 0.7× failover time             |
| 2   | **Concurrency & parallelism** | Read-only scaling ≥ 14× at 16 cores; mixed ≥ 8×; write-heavy ≥ 4×; zero deadlocks under fuzzed loads              |
| 3   | **Reliability**               | No thundering-herd on follower restart; bounded recovery time; disk-full → read-only, never crash, never corrupt  |
| 4   | **Correctness**               | Public Jepsen run in CI; deterministic simulator from Phase 5; Porcupine linearizability checker on every history |
| 5   | **Safety**                    | `unsafe_code = "forbid"` workspace-wide; `clippy::unwrap_used`/`panic`/`indexing_slicing` denied in non-test code |
| 6   | **Security**                  | Threat model in Phase 12; supply-chain hardening via cargo-deny / cargo-audit / cargo-vet / SBOM                  |
| 7   | **Large-scale distributed**   | Tier 2 read-scale-out via learner replicas; **up to ~5-10× etcd** on linearizable ReadIndex (target — Phase 14.5) |
| 8   | **Operability**               | Production-grade defaults, predictable behavior at scale, OpenTelemetry-native observability                      |
| 9   | **Developer ergonomics**      | Clean Rust API; fast CI; small contribution surface; expert-gated PR review on every change                       |
| 10  | **Storage efficiency**        | Smaller on-disk, faster compaction, no read stalls during major compaction                                        |

If a change merely matches etcd, that is a regression relative to the
goal. The full bar definitions, with named tests and acceptance
thresholds, live in [`ROADMAP.md`](./ROADMAP.md#the-bars-each-axis-has-a-comparison-oracle-a-measurable-threshold-and-a-named-test-that-gates-merge).

## Is mango the right tool for me?

Distributed KV stores are not interchangeable. Pick the one whose
consistency model and scale ceiling match the problem you have.

|                                         | **Mango**                                                 | **etcd**                                           | **FoundationDB**                                              | **DynamoDB**                                      |
| --------------------------------------- | --------------------------------------------------------- | -------------------------------------------------- | ------------------------------------------------------------- | ------------------------------------------------- |
| Consistency                             | Linearizable                                              | Linearizable                                       | Strict serializable (stronger than linearizable)              | Eventual (default); strong reads opt-in (2× cost) |
| Replication                             | Raft, single cluster                                      | Raft, single cluster                               | Multi-version, multi-shard                                    | Hash-sharded, multi-region async                  |
| Scale ceiling (writes)                  | ~200K writes/sec / cluster (Tier 1)                       | ~50-200K writes/sec / cluster                      | ~10M ops/sec / cluster (mixed)                                | ~10-100M ops/sec / global service (mixed)         |
| Scale ceiling (linearizable reads)      | ~600K reads/sec / cluster _(Tier 2b target — Phase 14.5)_ | ~50-150K reads/sec / cluster (ReadIndex)           | (see above)                                                   | Strong-reads-only mode (2× cost)                  |
| Scale ceiling (bounded-staleness reads) | ~1M reads/sec / cluster _(Tier 2a target — Phase 14.5)_   | ~500K-1M reads/sec / cluster (serializable, stale) | (see above)                                                   | Default mode                                      |
| Deployment                              | Self-host, open source (Apache-2.0)                       | Self-host, open source (Apache-2.0)                | Self-host, open source (Apache-2.0)                           | AWS-only hosted                                   |
| Primary use case                        | Cluster metadata, coordination, config, leader election   | Same as mango                                      | Application data with ACID at scale                           | Application data CRUD at hyperscale               |
| Operational profile                     | Single-binary, deterministic latency (no GC)              | Single-binary, Go GC                               | Multi-process (coordinators, storage, log), more moving parts | Fully managed                                     |

**Mango is etcd-shaped, not DynamoDB-shaped.** It is for workloads where
you need _strong_ consistency on a _self-hosted_ cluster and want
significantly better tail latency, memory footprint, and per-cluster
throughput than etcd ships today. If your bottleneck is "I need 10M+
ops/sec on application data," look at FoundationDB, TiKV, or a hosted
service — that is a different product category and not a mango goal.

See [`ROADMAP.md`](./ROADMAP.md) for the v1.0 contract: **Tier 1**
(single-cluster, etcd-shaped) plus **Tier 2** (read-scale-out via
learner replicas + client caching). Tier 2 targets are **up to ~5-10×
etcd on linearizable ReadIndex reads** and **up to ~2× on
bounded-staleness reads**, both measured on a 5-voter + 5-learner
cluster on the canonical bench hardware. Per-mode bars and the
underlying math are in `ROADMAP.md` Tier 2 bars and Phase 14.5.

## Layout

This is a Cargo workspace. Crates live under `crates/`:

```
crates/
  mango-proto/    # gRPC service definitions and generated code
  mango-storage/  # ordered-key K/V store (redb + raft-engine)
  mango-mvcc/     # MVCC layer: Revision, KeyIndex, snapshots, compaction
  mango-raft/     # Raft consensus (planned)
  mango-server/   # KV / Watch / Lease / Auth gRPC services + node (planned)
  mango-client/   # Rust client library (planned)
  mangoctl/       # CLI client, etcdctl-equivalent (planned)
```

Crates are added phase by phase as the roadmap progresses; not every
crate exists at every commit.

## Build, test, contribute

The full local-CI command sequence lives in [`CONTRIBUTING.md`](./CONTRIBUTING.md#commands-ci-runs).
The short version:

```bash
# Format
cargo fmt --all -- --check

# Lints
cargo clippy --workspace --all-targets --locked -- -D warnings

# Tests (nextest is the default runner; doctests run separately)
cargo nextest run --workspace --all-targets --locked --profile ci
cargo test --doc --workspace --locked

# Docs (warnings are errors)
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# Supply chain
cargo deny check
cargo audit
```

Doctests are not in nextest's scope (`cargo nextest`'s upstream design);
the separate `cargo test --doc` invocation is required. See
[`docs/testing.md`](./docs/testing.md) for the full testing policy.

For the contribution flow (branch naming, commit conventions, expert-
gated PR review), see [`CONTRIBUTING.md`](./CONTRIBUTING.md).

## Documentation

### Policy

Standards every PR is held to.

- [`docs/api-stability.md`](./docs/api-stability.md) — public API surface and stability tiers
- [`docs/arithmetic-policy.md`](./docs/arithmetic-policy.md) — overflow, saturation, conversion rules
- [`docs/ct-comparison-policy.md`](./docs/ct-comparison-policy.md) — constant-time crypto comparisons
- [`docs/dependency-updates.md`](./docs/dependency-updates.md) — Renovate / Dependabot policy
- [`docs/documentation-policy.md`](./docs/documentation-policy.md) — what's documented and where
- [`docs/msrv.md`](./docs/msrv.md) — minimum supported Rust version
- [`docs/public-api-policy.md`](./docs/public-api-policy.md) — `cargo-public-api` enforcement
- [`docs/sbom-policy.md`](./docs/sbom-policy.md) — CycloneDX SBOM generation
- [`docs/semver-policy.md`](./docs/semver-policy.md) — `cargo-semver-checks` gate
- [`docs/supply-chain-policy.md`](./docs/supply-chain-policy.md) — vendoring, vetting, audit
- [`docs/unsafe-policy.md`](./docs/unsafe-policy.md) — when `unsafe` is permitted

### Architecture

- [`docs/architecture/crash-only.md`](./docs/architecture/crash-only.md) — crash-only software design

### Testing

- [`docs/testing.md`](./docs/testing.md) — overall test strategy
- [`docs/loom.md`](./docs/loom.md) — `loom` model checking
- [`docs/madsim.md`](./docs/madsim.md) — deterministic simulation testing
- [`docs/miri.md`](./docs/miri.md) — Miri unsafe-code verification
- [`docs/time.md`](./docs/time.md) — clock and time-handling policy

### Contributor flow

- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — how to contribute
- [`SECURITY.md`](./SECURITY.md) — vulnerability reporting
- [`ROADMAP.md`](./ROADMAP.md) — phase-by-phase build plan

## Security

Found a vulnerability? See [`SECURITY.md`](./SECURITY.md).

## License

Licensed under the Apache License, Version 2.0
([LICENSE](LICENSE) or http://www.apache.org/licenses/LICENSE-2.0).

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in mango by you, as defined in the Apache-2.0
license, shall be licensed as above, without any additional terms or
conditions. See [NOTICE](NOTICE) for attribution.
