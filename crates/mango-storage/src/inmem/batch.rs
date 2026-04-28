//! [`WriteBatch`] impl for the in-memory reference backend
//! (ROADMAP:821).
//!
//! Mirrors the staging-buffer shape of [`crate::redb::batch::RedbBatch`]
//! (`crates/mango-storage/src/redb/batch.rs`). Both batch types
//! exist purely to decouple op construction from the apply
//! critical section; the in-mem batch's apply path is just a
//! `BTreeMap` mutation, but it MUST honor the same staging
//! invariants — empty-key/empty-value rejection, `delete_range`
//! tolerance of empty bounds — so the engine-swap dry-run test
//! (`tests/engine_swap_dryrun.rs`) sees identical observable
//! behavior across the two backends.
//!
//! # Send-ness
//!
//! [`InMemBatch`] is deliberately `!Send + !Sync` via a
//! `PhantomData<*const ()>` marker, mirroring `RedbBatch`. The
//! trait contract in `mango_storage::WriteBatch` permits this.
//! The `compile_fail` doctest below pins the invariant.
//!
//! ```compile_fail
//! fn needs_send<T: Send>() {}
//! needs_send::<mango_storage::InMemBatch>();
//! ```

use std::marker::PhantomData;

use crate::backend::{BackendError, BucketId, WriteBatch};
use crate::redb::batch::{EMPTY_KEY_ERROR, EMPTY_VALUE_ERROR};

/// A single staged mutation. Carries owned byte vectors for the
/// same reason as [`crate::redb::batch::StagedOp`]: the batch
/// outlives the caller's buffer references.
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
    /// Remove every key in the half-open interval `[start, end)`,
    /// with `end == []` meaning "unbounded upper" per the
    /// engine-neutral `DeleteRange` contract (matches bbolt's
    /// `len(end) == 0` semantics — see
    /// `crates/mango-storage/src/redb/mod.rs::apply_staged` lines
    /// 296-310 for the canonical definition).
    DeleteRange {
        /// Target bucket id.
        bucket: BucketId,
        /// Owned start-bound bytes (inclusive).
        start: Vec<u8>,
        /// Owned end-bound bytes (exclusive; empty = unbounded).
        end: Vec<u8>,
    },
}

/// Staging-buffer write batch for the in-memory reference
/// backend. Produced by [`crate::Backend::begin_batch`]; consumed
/// by [`crate::Backend::commit_batch`] or
/// [`crate::Backend::commit_group`].
///
/// See the module-level docs for the Send-ness rationale.
#[derive(Debug, Default)]
pub struct InMemBatch {
    pub(super) staged: Vec<StagedOp>,
    /// `PhantomData<*const ()>` is the canonical `!Send + !Sync`
    /// marker. Same shape as `RedbBatch`.
    _not_send_sync: PhantomData<*const ()>,
}

impl InMemBatch {
    /// Construct an empty batch. Called only from
    /// [`crate::Backend::begin_batch`].
    #[must_use]
    pub(super) fn new() -> Self {
        Self {
            staged: Vec::new(),
            _not_send_sync: PhantomData,
        }
    }

    /// Consume the batch and return its staged ops. Called by the
    /// commit paths in the sync prologue, *before* any future is
    /// constructed, so the `!Send` marker on [`InMemBatch`] does
    /// not propagate into the `Future + Send` return type.
    pub(super) fn into_staged(self) -> Vec<StagedOp> {
        self.staged
    }

    /// Observability for tests. Counts staged ops without revealing
    /// the op variant.
    #[cfg(test)]
    pub(super) fn staged_len(&self) -> usize {
        self.staged.len()
    }
}

impl WriteBatch for InMemBatch {
    fn put(&mut self, bucket: BucketId, key: &[u8], value: &[u8]) -> Result<(), BackendError> {
        // Mirror RedbBatch: empty key/value rejected at stage time
        // with byte-identical error messages
        // (EMPTY_KEY_ERROR / EMPTY_VALUE_ERROR re-used). Necessary
        // for trait parity in the engine-swap dry-run.
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
        // `start = []` means "from the min of the keyspace";
        // `end = []` means "unbounded upper". Both legal at stage
        // time; redb mirror in `redb/batch.rs::delete_range`.
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
        let b = InMemBatch::new();
        assert_eq!(b.staged_len(), 0);
    }

    #[test]
    fn put_delete_delete_range_all_stage_without_io() {
        let mut b = InMemBatch::new();
        b.put(BucketId::new(1), b"k", b"v").unwrap();
        b.delete(BucketId::new(1), b"k").unwrap();
        b.delete_range(BucketId::new(1), b"a", b"z").unwrap();
        assert_eq!(b.staged_len(), 3);
    }

    #[test]
    fn put_empty_key_is_rejected_with_redb_parity_message() {
        let mut b = InMemBatch::new();
        let err = b.put(BucketId::new(1), b"", b"v").unwrap_err();
        match err {
            BackendError::Other(msg) => assert_eq!(msg, EMPTY_KEY_ERROR),
            other => panic!("expected Other(empty key), got {other:?}"),
        }
        assert_eq!(b.staged_len(), 0);
    }

    #[test]
    fn put_empty_value_is_rejected_with_redb_parity_message() {
        let mut b = InMemBatch::new();
        let err = b.put(BucketId::new(1), b"k", b"").unwrap_err();
        match err {
            BackendError::Other(msg) => assert_eq!(msg, EMPTY_VALUE_ERROR),
            other => panic!("expected Other(empty value), got {other:?}"),
        }
        assert_eq!(b.staged_len(), 0);
    }

    #[test]
    fn delete_empty_key_is_rejected() {
        let mut b = InMemBatch::new();
        let err = b.delete(BucketId::new(1), b"").unwrap_err();
        match err {
            BackendError::Other(msg) => assert_eq!(msg, EMPTY_KEY_ERROR),
            other => panic!("expected Other(empty key), got {other:?}"),
        }
        assert_eq!(b.staged_len(), 0);
    }

    #[test]
    fn delete_range_tolerates_empty_bounds() {
        let mut b = InMemBatch::new();
        b.delete_range(BucketId::new(1), b"", b"").unwrap();
        b.delete_range(BucketId::new(1), b"", b"z").unwrap();
        b.delete_range(BucketId::new(1), b"a", b"").unwrap();
        assert_eq!(b.staged_len(), 3);
    }

    #[test]
    fn into_staged_consumes_and_returns() {
        let mut b = InMemBatch::new();
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
