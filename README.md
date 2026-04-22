# mango

[![ci](https://github.com/humancto/mango/actions/workflows/ci.yml/badge.svg)](https://github.com/humancto/mango/actions/workflows/ci.yml)

A distributed, reliable key-value store written in Rust. Mango is a ground-up
port of [etcd](https://github.com/etcd-io/etcd) — same problem space, same
guarantees (linearizable KV over Raft, watch streams, leases, MVCC), but a
clean Rust-native API and implementation. Mango is **not** wire-compatible
with etcd; it is inspired by it.

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
