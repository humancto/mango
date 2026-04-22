# mango

[![ci](https://github.com/humancto/mango/actions/workflows/ci.yml/badge.svg)](https://github.com/humancto/mango/actions/workflows/ci.yml)

A distributed, reliable key-value store written in Rust. Mango is a ground-up
port of [etcd](https://github.com/etcd-io/etcd) — same problem space, same
guarantees (linearizable KV over Raft, watch streams, leases, MVCC), but a
clean Rust-native API and implementation. Mango is **not** wire-compatible
with etcd; it is inspired by it.

## Is mango the right tool for me?

Distributed KV stores are not interchangeable. Pick the one whose
consistency model and scale ceiling match the problem you have.

| | **Mango** | **etcd** | **FoundationDB** | **DynamoDB** |
|---|---|---|---|---|
| Consistency | Linearizable | Linearizable | Strict serializable (stronger than linearizable) | Eventual (default); strong reads opt-in (2× cost) |
| Replication | Raft, single cluster | Raft, single cluster | Multi-version, multi-shard | Hash-sharded, multi-region async |
| Scale ceiling (writes) | ~200K writes/sec / cluster (Tier 1) | ~50-200K writes/sec / cluster | ~10M ops/sec / cluster (mixed) | ~10-100M ops/sec / global service (mixed) |
| Scale ceiling (linearizable reads) | ~600K reads/sec / cluster *(Tier 2b target — Phase 14.5)* | ~50-150K reads/sec / cluster (ReadIndex) | (see above) | Strong-reads-only mode (2× cost) |
| Scale ceiling (bounded-staleness reads) | ~1M reads/sec / cluster *(Tier 2a target — Phase 14.5)* | ~500K-1M reads/sec / cluster (serializable, stale) | (see above) | Default mode |
| Deployment | Self-host, open source (Apache-2.0) | Self-host, open source (Apache-2.0) | Self-host, open source (Apache-2.0) | AWS-only hosted |
| Primary use case | Cluster metadata, coordination, config, leader election | Same as mango | Application data with ACID at scale | Application data CRUD at hyperscale |
| Operational profile | Single-binary, deterministic latency (no GC) | Single-binary, Go GC | Multi-process (coordinators, storage, log), more moving parts | Fully managed |

**Mango is etcd-shaped, not DynamoDB-shaped.** It's for the workloads
where you need *strong* consistency on a *self-hosted* cluster and want
significantly better tail latency, memory footprint, and per-cluster
throughput than etcd ships today. If your bottleneck is "I need 10M+
ops/sec on application data," look at FoundationDB, TiKV, or a hosted
service — that's a different product category and not a mango goal.

See [`ROADMAP.md`](./ROADMAP.md) for the v1.0 contract: Tier 1
(single-cluster, etcd-shaped) plus Tier 2 (read-scale-out via
learner replicas + client caching). Tier 2 targets are **up to
~5-10× etcd on linearizable ReadIndex reads** and **up to ~2× on
bounded-staleness reads**, both measured on a 5-voter + 5-learner
cluster on the canonical bench hardware. Per-mode bars and the
underlying math are in `ROADMAP.md` north-star bar #7 and
Phase 14.5.

## Status

Pre-alpha. See [`ROADMAP.md`](./ROADMAP.md) for the build plan.

## Layout

This is a Cargo workspace. Crates live under `crates/`:

```
crates/
  mango-proto/    # gRPC service definitions and generated code
  mango-storage/  # MVCC + backend (B-tree-on-disk) layer
  mango-raft/     # Raft consensus
  mango-server/   # KV / Watch / Lease / Auth gRPC services and the node
  mango-client/   # Rust client library
  mangoctl/       # CLI client (etcdctl-equivalent)
```

(Crates are added phase by phase as the roadmap progresses; not every crate
exists at every commit.)

## Build

```bash
cargo build --workspace
cargo test  --workspace
```

## License

Apache-2.0.
