//! Phase 1 parity bench harness for the mango storage backend
//! (ROADMAP:829). Drives a fixed workload against `RedbBackend` and
//! the bbolt oracle subprocess, captures latency / throughput /
//! on-disk-size metrics, and emits a JSON result file consumed by
//! the [`gate`] binary.
//!
//! See `.planning/parity-bench-harness.plan.md` and the README at
//! `benches/README.md` for the full design rationale.
//!
//! The harness is deliberately excluded from `default-members` in
//! the workspace `Cargo.toml`; `cargo build` / `cargo test` from
//! the workspace root skips it. Build explicitly with
//! `cargo build -p mango-bench-storage --release`.

#![deny(rust_2018_idioms)]
// `publish = false` bench-only crate — opted out of the workspace
// `clippy::exhaustive_enums = "deny"` policy; the workload schema
// enums (`Distribution`, `ValueFill`, `Generator`) are deliberately
// closed so that toml typos surface as parse errors, not silently
// fall through. Same pattern as `crates/mango-loom-demo`.
#![allow(clippy::exhaustive_enums)]

pub mod bbolt_runner;
pub mod dropcache;
pub mod gate;
pub mod mango_runner;
pub mod measure;
pub mod stats;
pub mod workload;
pub mod zipfian;
