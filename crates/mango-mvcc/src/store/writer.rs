//! Writer-serialized state behind the [`super::MvccStore`] writer
//! lock.
//!
//! Holds the monotonic `next_main` allocator and the per-write
//! `emit_buf` watch-event scratch (Phase 3 plan §4.2,
//! ROADMAP.md:862). A single instance lives behind a
//! `tokio::sync::Mutex` on [`super::MvccStore`]; only the lock
//! holder may read/write the fields, so no further
//! synchronization is required here.
//!
//! Sub allocation does not live on `WriterState` because subs
//! reset to zero per top-level op (etcd v3.5.16
//! `mvcc/kvstore_txn.go::storeTxnWrite` parity); each writer
//! method allocates a local `sub: i64 = 0` after acquiring the
//! lock and post-increments per **physical** write (review item
//! S3 of the L844 plan).

/// Allocator for `Revision::main` values. Lives behind a
/// `tokio::sync::Mutex` on [`super::MvccStore`].
///
/// `next_main` starts at `1` (Mango's first user-visible revision —
/// `(0, 0)` is the "before-any-revision" sentinel from
/// `crate::Revision`'s rustdoc).
///
/// Overflow is checked at the call site via `checked_add` per the
/// workspace `clippy::arithmetic_side_effects` deny — at 1M
/// revs/sec, `i64::MAX` requires ~292,000 years, but the lint
/// demands the check (review item M7 of the L844 plan).
#[derive(Debug)]
pub(crate) struct WriterState {
    /// Next `main` revision to allocate. Monotone; never resets.
    pub(crate) next_main: i64,
    /// Per-write watch-event scratch buffer. Phase 3 plan §4.2
    /// (ROADMAP.md:862). Each writer (`put` / `delete_range` /
    /// `txn`) populates this during the op, then dispatches to the
    /// observer (if attached) **after** `snapshot.store()` and
    /// before releasing the writer lock. The buffer is cleared at
    /// the end of every dispatch so allocations carry across
    /// writes — the no-observer hot path costs one branch on
    /// `is_empty()` and one `Vec::clear` on a small Vec.
    pub(crate) emit_buf: Vec<crate::watchable_store::WatchEvent>,
}

impl WriterState {
    /// Construct the allocator for a fresh store. `next_main = 1`.
    pub(crate) fn new() -> Self {
        Self {
            next_main: 1,
            emit_buf: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::WriterState;

    #[test]
    fn new_starts_at_one() {
        let s = WriterState::new();
        assert_eq!(s.next_main, 1);
        assert!(s.emit_buf.is_empty(), "fresh emit_buf is empty");
    }
}
