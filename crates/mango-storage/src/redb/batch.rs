//! [`WriteBatch`] impl for the redb backend.
//!
//! A [`RedbBatch`] is a pure staging buffer: `put`/`delete`/`delete_range`
//! calls append a [`StagedOp`] to an in-memory `Vec` with no redb
//! involvement. The buffer is replayed into a single
//! `redb::WriteTransaction` at commit time by the backend's
//! `apply_staged` / `commit_staged` helpers (see `super`).
//!
//! Why staging instead of a live `WriteTransaction`: a live txn
//! holds redb's write-lock for the batch's entire lifetime, which
//! would serialize *every* concurrent writer behind an uncommitted
//! batch. Staging decouples batch construction from the disk
//! critical section — the only time the redb write-lock is held is
//! inside `commit_batch` / `commit_group`, for the duration of
//! fsync.
//!
//! # Send-ness
//!
//! [`RedbBatch`] is `Send + Sync` — it carries only a
//! `Vec<StagedOp>` (pure data, no thread-local handles). The
//! earlier design used a `PhantomData<*const ()>` `!Send + !Sync`
//! marker to express "one batch per logical writer" at the type
//! system level, but that propagated up through
//! `MvccStore::{put, delete_range, txn, compact}` and made every
//! writer future `!Send`, blocking `tokio::spawn` in L854's gRPC
//! server (rust-expert PR #75 review S2). The "one logical writer"
//! invariant is enforced more cheaply by `&mut self` on
//! `WriteBatch::{put, delete, delete_range}` and by single-
//! ownership move into `commit_batch`; cross-thread move is sound
//! and is now allowed.

use crate::backend::{BackendError, BucketId, WriteBatch};

/// A single staged mutation. Carries owned byte vectors so the
/// batch outlives the caller's buffer references (the trait
/// methods take `&[u8]`; staging copies).
#[derive(Debug, Clone)]
pub(super) enum StagedOp {
    /// Insert-or-overwrite a key in `bucket`.
    Put {
        /// Target bucket id.
        bucket: BucketId,
        /// Owned key bytes.
        key: Vec<u8>,
        /// Owned value bytes.
        value: Vec<u8>,
    },
    /// Remove a single key. No-op at apply time if absent.
    Delete {
        /// Target bucket id.
        bucket: BucketId,
        /// Owned key bytes.
        key: Vec<u8>,
    },
    /// Remove every key in the half-open interval `[start, end)`.
    /// Range validation happens at apply time; staging is
    /// unconditional so `delete_range` stays infallible on the
    /// hot path (symmetric with `put`/`delete`).
    DeleteRange {
        /// Target bucket id.
        bucket: BucketId,
        /// Owned start-bound bytes (inclusive).
        start: Vec<u8>,
        /// Owned end-bound bytes (exclusive).
        end: Vec<u8>,
    },
}

/// Staging-buffer write batch for the redb backend. Produced by
/// [`crate::Backend::begin_batch`]; consumed by
/// [`crate::Backend::commit_batch`] or
/// [`crate::Backend::commit_group`].
///
/// See the module-level docs for the Send-ness rationale.
#[derive(Debug, Default)]
pub struct RedbBatch {
    staged: Vec<StagedOp>,
}

impl RedbBatch {
    /// Construct an empty batch. Called only from
    /// [`crate::Backend::begin_batch`]; user code does not
    /// construct batches directly.
    #[must_use]
    pub(super) fn new() -> Self {
        Self { staged: Vec::new() }
    }

    /// Consume the batch and return its staged ops. Called by the
    /// commit paths in the sync prologue of
    /// `commit_batch` / `commit_group`, *before* any future is
    /// constructed, so the `!Send` marker on [`RedbBatch`] does
    /// not propagate into the `Future + Send` return type.
    pub(super) fn into_staged(self) -> Vec<StagedOp> {
        self.staged
    }

    /// Observability for tests. Counts staged ops without
    /// revealing the op variant.
    #[cfg(test)]
    pub(super) fn staged_len(&self) -> usize {
        self.staged.len()
    }
}

/// Error text returned when a caller attempts to stage an empty
/// key. Matches the wire-level error string emitted by the bbolt
/// oracle so the differential harness sees byte-identical errors
/// from both engines (plan §5 "Hard contracts — Empty keys").
pub(crate) const EMPTY_KEY_ERROR: &str = "empty key";

/// Error text returned for empty values. Same rationale as
/// [`EMPTY_KEY_ERROR`] — see plan §5 "Hard contracts — Empty values".
pub(crate) const EMPTY_VALUE_ERROR: &str = "empty value";

impl WriteBatch for RedbBatch {
    fn put(&mut self, bucket: BucketId, key: &[u8], value: &[u8]) -> Result<(), BackendError> {
        // Reject empty key/value at stage time so we match bbolt's
        // `ErrKeyRequired` / `ErrValueNil`. redb itself tolerates
        // both, which would otherwise cause a differential
        // divergence (plan §5 Hard contract). Checked here — the
        // only entry point into the staging buffer — so every
        // commit path inherits the invariant.
        if key.is_empty() {
            return Err(BackendError::Other(EMPTY_KEY_ERROR.to_owned()));
        }
        if value.is_empty() {
            return Err(BackendError::Other(EMPTY_VALUE_ERROR.to_owned()));
        }
        self.staged.push(StagedOp::Put {
            bucket,
            key: key.to_vec(),
            value: value.to_vec(),
        });
        Ok(())
    }

    fn delete(&mut self, bucket: BucketId, key: &[u8]) -> Result<(), BackendError> {
        if key.is_empty() {
            return Err(BackendError::Other(EMPTY_KEY_ERROR.to_owned()));
        }
        self.staged.push(StagedOp::Delete {
            bucket,
            key: key.to_vec(),
        });
        Ok(())
    }

    fn delete_range(
        &mut self,
        bucket: BucketId,
        start: &[u8],
        end: &[u8],
    ) -> Result<(), BackendError> {
        // `start`/`end` can legally be empty — they denote the min
        // and max of the keyspace respectively (see oracle's
        // cursor-walk in main.go). No lifting here.
        self.staged.push(StagedOp::DeleteRange {
            bucket,
            start: start.to_vec(),
            end: end.to_vec(),
        });
        Ok(())
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

    use super::*;

    #[test]
    fn empty_batch_has_no_staged_ops() {
        let b = RedbBatch::new();
        assert_eq!(b.staged_len(), 0);
    }

    #[test]
    fn put_delete_delete_range_all_stage_without_io() {
        let mut b = RedbBatch::new();
        b.put(BucketId::new(1), b"k", b"v").unwrap();
        b.delete(BucketId::new(1), b"k").unwrap();
        b.delete_range(BucketId::new(1), b"a", b"z").unwrap();
        assert_eq!(b.staged_len(), 3);
    }

    #[test]
    fn put_empty_key_is_rejected() {
        let mut b = RedbBatch::new();
        let err = b.put(BucketId::new(1), b"", b"v").unwrap_err();
        match err {
            BackendError::Other(msg) => assert_eq!(msg, EMPTY_KEY_ERROR),
            other => panic!("expected Other(empty key), got {other:?}"),
        }
        assert_eq!(b.staged_len(), 0, "rejected op must not stage");
    }

    #[test]
    fn put_empty_value_is_rejected() {
        let mut b = RedbBatch::new();
        let err = b.put(BucketId::new(1), b"k", b"").unwrap_err();
        match err {
            BackendError::Other(msg) => assert_eq!(msg, EMPTY_VALUE_ERROR),
            other => panic!("expected Other(empty value), got {other:?}"),
        }
        assert_eq!(b.staged_len(), 0);
    }

    #[test]
    fn delete_empty_key_is_rejected() {
        let mut b = RedbBatch::new();
        let err = b.delete(BucketId::new(1), b"").unwrap_err();
        match err {
            BackendError::Other(msg) => assert_eq!(msg, EMPTY_KEY_ERROR),
            other => panic!("expected Other(empty key), got {other:?}"),
        }
        assert_eq!(b.staged_len(), 0);
    }

    #[test]
    fn delete_range_tolerates_empty_bounds() {
        // `start = []` means "from the min of the keyspace";
        // `end = []` means "to the max". Both are legal and must
        // NOT be lifted into an error — see plan §5 and the bbolt
        // oracle's cursor-walk comments.
        let mut b = RedbBatch::new();
        b.delete_range(BucketId::new(1), b"", b"").unwrap();
        b.delete_range(BucketId::new(1), b"", b"z").unwrap();
        b.delete_range(BucketId::new(1), b"a", b"").unwrap();
        assert_eq!(b.staged_len(), 3);
    }

    #[test]
    fn into_staged_consumes_and_returns() {
        let mut b = RedbBatch::new();
        b.put(BucketId::new(1), b"k", b"v").unwrap();
        let ops = b.into_staged();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            StagedOp::Put { bucket, key, value } => {
                assert_eq!(*bucket, BucketId::new(1));
                assert_eq!(key, b"k");
                assert_eq!(value, b"v");
            }
            other => panic!("expected Put, got {other:?}"),
        }
    }
}
