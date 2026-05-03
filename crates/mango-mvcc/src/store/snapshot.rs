//! Immutable read-side view of [`crate::MvccStore`] (L846).
//!
//! [`Snapshot`] is the type published via [`arc_swap::ArcSwap`] on
//! every successful writer commit. Readers acquire the latest
//! snapshot with one `load_full()` and observe a coherent
//! `(rev, compacted)` pair — replacing the prior pair of
//! independent atomics on `MvccStore` (`current_main: AtomicI64`
//! and `compacted: AtomicI64`).
//!
//! # What the snapshot contains
//!
//! - [`Snapshot::rev`] — highest fully-published revision.
//! - [`Snapshot::compacted`] — compaction floor. Reads at
//!   `revision < compacted` return [`crate::MvccError::Compacted`].
//!
//! # What the snapshot does NOT contain
//!
//! The key index ([`crate::ShardedKeyIndex`]) and `keys_in_order`
//! `BTreeMap` are **not** in this snapshot. They are read through
//! their own concurrency primitives (per-shard `RwLock` and a
//! single `RwLock<BTreeMap<...>>` respectively).
//!
//! This is sound because:
//!
//! - Puts only **add** revisions to a key's history; an older
//!   `snap.rev` cannot see a put it wasn't published with — the
//!   per-key version walk in
//!   [`crate::ShardedKeyIndex::get`]`(k, snap.rev)` filters by
//!   `version_main <= snap.rev`.
//! - Compacts only **drop** revisions `<= floor`; observing the
//!   older `compacted` floor is conservative — the reader sees
//!   slightly more history than strictly necessary, which is safe.
//! - The pair `(rev, compacted)` is published atomically via
//!   `ArcSwap`, so the [`crate::MvccStore::range`] entry-point
//!   sees a consistent floor for its rev.
//!
//! Subsequent ROADMAP items may add fields (lease epoch, watcher
//! cursor, cache generation). [`Snapshot`] carries
//! `#[non_exhaustive]` so future fields don't break downstream
//! crates' struct-expression init.
//!
//! # `load_full()` vs `load()` discipline
//!
//! Per `arc-swap` 1.x:
//!
//! - `load()` returns a `Guard<Arc<Snapshot>>`, fast on the hot
//!   path because no refcount is bumped.
//! - `load_full()` returns a plain `Arc<Snapshot>`, costing one
//!   atomic refcount-inc.
//!
//! Holding a `Guard` does **not** block writers — the `store()`
//! path is wait-free regardless. The real cost of holding a
//! `Guard` across many iterations or `.await` points is
//! per-thread fast-slot pool exhaustion, falling back to a slower
//! path (still correct, but loses the hot-path optimization).
//!
//! Rule of thumb (matches ROADMAP.md:846):
//!
//! - Long scans (>1000 keys) or values held across `.await` →
//!   `load_full()`.
//! - One-shot field reads → `load()` is fine; the `Guard` drops
//!   immediately.

/// Immutable read-side view of [`crate::MvccStore`].
///
/// See module-level docs for the publication protocol and the
/// rationale for what is (and isn't) in the snapshot.
///
/// `#[non_exhaustive]` blocks struct-expression init from outside
/// the crate. To construct one for testing inside the crate, use
/// [`Snapshot::empty`] or build through the writer paths in
/// [`crate::MvccStore`].
///
/// **No `Copy`**: future fields (e.g. `Arc<KeyIndex>`) will be
/// non-`Copy`; deriving `Copy` today would silently break every
/// call-site when those land.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Snapshot {
    /// Highest fully-published revision. `0` on a fresh store.
    pub rev: i64,
    /// Compaction floor. Reads at `revision < compacted` return
    /// [`crate::MvccError::Compacted`]. `0` = no compaction has
    /// happened.
    pub compacted: i64,
}

impl Snapshot {
    /// Construct the initial snapshot for a fresh store: rev = 0,
    /// compacted = 0. `pub(crate)` because user code never builds
    /// snapshots directly — they observe them via
    /// [`crate::MvccStore::current_snapshot`].
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self {
            rev: 0,
            compacted: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn empty_is_zero_zero() {
        let s = Snapshot::empty();
        assert_eq!(s.rev, 0);
        assert_eq!(s.compacted, 0);
    }

    #[test]
    fn clone_preserves_pair() {
        let s = Snapshot {
            rev: 42,
            compacted: 10,
        };
        let c = s.clone();
        assert_eq!(c, s);
    }
}
