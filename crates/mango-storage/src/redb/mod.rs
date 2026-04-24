//! `redb`-backed [`crate::Backend`] impl (ROADMAP:817).
//!
//! Internal module root. The public surface of the impl lands in
//! subsequent commits; this initial commit carries only the
//! in-memory [`registry::Registry`] for the bucket-name/id mapping.
//!
//! The external `::redb` crate is referenced via absolute path
//! (`::redb::Database`, `::redb::TableDefinition`, etc.) so the
//! internal `redb` module name does not shadow it inside this tree.

// Removed in the next commit, which consumes every item in
// `registry`. Scoped to this module so nothing else in the crate
// is silently allowed to accumulate dead code.
#![allow(dead_code)]

pub(crate) mod registry;
