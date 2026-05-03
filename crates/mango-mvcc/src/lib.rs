//! Mango MVCC primitives.
//!
//! This crate carries the pure-data foundations of Mango's MVCC
//! store (per ROADMAP.md Phase 2). The on-disk key encoding and
//! bucket reservations land in a follow-up commit; this commit
//! ships the [`Revision`] value type only.
//!
//! What this crate is NOT (each is a separate ROADMAP item):
//!
//! - The in-memory `KeyIndex` (L839)
//! - The `KV` API (L844)
//! - Read transactions / snapshot publication (L845/L846)
//! - Compaction (L849/L850)
//! - Property test against a model (L851)
//! - `cargo fuzz` target (L853)
//!
//! The crate is `unsafe`-free.

pub mod revision;

pub use revision::Revision;
