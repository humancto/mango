//! mango-storage — the storage backend crate for mango.
//!
//! This crate is currently a placeholder skeleton. The `Backend` and
//! `RaftLogStore` trait definitions land per `ROADMAP.md` (Phase 1);
//! implementations follow in their own PRs.
//!
//! Dependencies declared here — `redb` and a git-pinned fork of
//! `raft-engine` — are wired so subsequent trait and impl PRs can
//! consume them via `.workspace = true`. The fork exists to keep
//! `lz4-sys` (C FFI) out of the default build graph; see
//! `.planning/adr/0002-storage-engine.md` §W5 and
//! `.planning/fork-raft-engine-lz4-verification.md`.

#![deny(missing_docs)]

/// The package version string, captured at build time from
/// `CARGO_PKG_VERSION`. Kept as a crate-level constant so downstream
/// tests can assert on the shipped version without re-reading
/// `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::unnecessary_literal_unwrap,
        clippy::arithmetic_side_effects
    )]

    // Watchdog smoke lives in `crates/mango/src/lib.rs`; the single
    // oracle for `scripts/test-watchdog.sh` is sufficient and not
    // duplicated per crate.

    use super::*;

    #[test]
    fn version_matches_cargo_manifest() {
        assert_eq!(VERSION, "0.1.0");
    }
}
