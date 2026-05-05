//! Error types for the MVCC store.
//!
//! Two enums:
//!
//! - [`OpenError`] — programmer-error paths in `MvccStore::open`
//!   (idiom shared with `BackendError::Closed` vs operation-time
//!   errors). `MvccStore` lands in a later commit.
//! - [`MvccError`] — runtime errors from `Put` / `Range` /
//!   `DeleteRange` / `Txn` / `Compact`.
//!
//! `MvccError::Compacted` uses `<` (not `<=`) against the floor —
//! etcd retains the floor revision so reads at the watermark
//! continue to work (review item B1 of the L844 plan).
//!
//! Writer-invariant violations (e.g. a monotonic-allocator failure
//! inside the writer critical section) surface as
//! [`MvccError::Internal`] rather than `panic!()`. The workspace
//! lints `clippy::panic` / `unwrap_used` / `expect_used` are deny
//! anyway; this enum gives those sites a typed path (review item
//! S2 of the L844 plan).

use mango_storage::BackendError;

use crate::{KeyDecodeError, KeyHistoryError, KeyIndexError};

/// Errors returned by `MvccStore::open` (lands in a later commit).
///
/// `NonEmptyBackend` is the L844 boundary against L852
/// (restart-from-disk recovery). L852 will detect a non-empty
/// backend and rebuild in-mem state instead.
///
/// `found_revs` is best-effort and capped at 1024 — enough to
/// distinguish "empty" from "has data" without scanning a huge
/// accidentally-populated backend (review item N5 of the L844 plan).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The backend already contains key-bucket data. L844 only
    /// supports opening against an empty backend; full recovery
    /// lands in L852.
    #[error(
        "backend is non-empty (found at least {found_revs} revisions); \
         recovery from a non-empty store lands in L852"
    )]
    NonEmptyBackend {
        /// Best-effort match count, capped at 1024.
        found_revs: u64,
    },
    /// Underlying backend error (bucket registration, snapshot
    /// open, etc.).
    #[error(transparent)]
    Backend(#[from] BackendError),
}

/// Errors returned by `MvccStore` runtime ops (lands in a later
/// commit).
///
/// `Compacted` and `FutureRevision` are user-facing: a `Range` at
/// a stale or speculative revision returns one of these. `Internal`
/// is reserved for invariant violations that should not occur in
/// practice — surfaced as a typed error rather than `panic!()` so
/// the workspace's `clippy::panic` deny holds (review item S2).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MvccError {
    /// Read against a revision **strictly below** the compacted
    /// floor. The floor itself is still readable — etcd parity.
    #[error("range against compacted revision {requested} (compacted floor: {floor})")]
    Compacted {
        /// Revision the caller asked for.
        requested: i64,
        /// Current compacted floor.
        floor: i64,
    },
    /// Read or compact against a revision above the current head.
    #[error("future revision {requested} (current: {current})")]
    FutureRevision {
        /// Revision the caller asked for.
        requested: i64,
        /// Highest fully-published revision.
        current: i64,
    },
    /// Range bounds were invalid (e.g. `start > end`).
    #[error("invalid range: start > end")]
    InvalidRange,
    /// A writer-side invariant was violated. Indicates a Mango bug.
    /// Surfaced rather than panicked so callers can log and abort
    /// at a controlled boundary.
    #[error("internal invariant violation: {context}")]
    Internal {
        /// Static description of the violated invariant.
        context: &'static str,
    },
    /// Underlying backend error.
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// Per-key history operation error (e.g. monotonicity violation
    /// — should not occur under the writer-lock invariant; surfaced
    /// for diagnostic clarity).
    #[error(transparent)]
    KeyHistory(#[from] KeyHistoryError),
    /// Sharded key-index operation error.
    #[error(transparent)]
    KeyIndex(#[from] KeyIndexError),
    /// On-disk key decode error (corrupt encoding bytes from the
    /// `key` bucket).
    #[error(transparent)]
    KeyDecode(#[from] KeyDecodeError),
    /// A feature the caller invoked is not yet wired in this
    /// ROADMAP phase. Carries an [`UnsupportedFeature`] tag so
    /// callers can branch on the specific gap rather than parsing
    /// a string. Phase 3 plan §3 (ROADMAP.md:862).
    ///
    /// **Stability.** Adding a variant to [`UnsupportedFeature`]
    /// is non-breaking (`#[non_exhaustive]`); removing one is the
    /// desired compile-time signal that the feature has shipped.
    #[error("unsupported feature: {0:?}")]
    Unsupported(UnsupportedFeature),
}

/// Tag for a feature gated off at the current ROADMAP phase.
/// Pairs with [`MvccError::Unsupported`].
///
/// **Intent.** Adding a variant is non-breaking; removing one is
/// load-bearing — when the gap closes (e.g. ROADMAP.md:863 wires
/// the unsynced/catch-up watcher path), deleting the variant
/// makes every matchsite that handles it fail compilation, which
/// is exactly the signal we want.
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
#[non_exhaustive]
pub enum UnsupportedFeature {
    /// A `watch(start_rev)` with `start_rev <= current_revision()`
    /// requires the unsynced/catch-up dispatch path. Phase 3 ships
    /// only the synced (current-rev forward) path; the unsynced
    /// path lands in ROADMAP.md:863.
    UnsyncedWatcher,
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::{MvccError, OpenError, UnsupportedFeature};

    #[test]
    fn open_error_non_empty_backend_message_includes_count() {
        let e = OpenError::NonEmptyBackend { found_revs: 42 };
        let msg = format!("{e}");
        assert!(msg.contains("42"), "message: {msg}");
        assert!(msg.contains("L852"), "message: {msg}");
    }

    #[test]
    fn mvcc_error_compacted_message_shapes() {
        let e = MvccError::Compacted {
            requested: 5,
            floor: 10,
        };
        let msg = format!("{e}");
        assert!(msg.contains('5'), "message: {msg}");
        assert!(msg.contains("10"), "message: {msg}");
    }

    #[test]
    fn mvcc_error_future_revision_message_shapes() {
        let e = MvccError::FutureRevision {
            requested: 99,
            current: 10,
        };
        let msg = format!("{e}");
        assert!(msg.contains("99"), "message: {msg}");
        assert!(msg.contains("10"), "message: {msg}");
    }

    #[test]
    fn mvcc_error_invalid_range_message() {
        let msg = format!("{}", MvccError::InvalidRange);
        assert!(msg.contains("invalid range"), "message: {msg}");
    }

    #[test]
    fn mvcc_error_internal_message_includes_context() {
        let e = MvccError::Internal {
            context: "next_main overflow",
        };
        let msg = format!("{e}");
        assert!(msg.contains("next_main overflow"), "message: {msg}");
    }

    #[test]
    fn mvcc_error_unsupported_message_includes_variant_name() {
        let e = MvccError::Unsupported(UnsupportedFeature::UnsyncedWatcher);
        let msg = format!("{e}");
        assert!(
            msg.contains("UnsyncedWatcher"),
            "Display passes through the inner Debug-formatted variant: {msg}"
        );
        assert!(
            msg.contains("unsupported feature"),
            "Display includes the umbrella label: {msg}"
        );
    }

    #[test]
    fn unsupported_feature_is_copy_eq_debug() {
        let a = UnsupportedFeature::UnsyncedWatcher;
        let b = a;
        assert_eq!(a, b);
        let dbg = format!("{a:?}");
        assert!(dbg.contains("UnsyncedWatcher"), "Debug: {dbg}");
    }
}
