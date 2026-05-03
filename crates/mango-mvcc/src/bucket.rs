//! Bucket reservations for the MVCC layer.
//!
//! Mango's MVCC store uses two `Backend` buckets, mirroring etcd's
//! split:
//!
//! - [`KEY_BUCKET_ID`] / `"key"` — the on-disk MVCC payload, keyed
//!   by `(revision, kind)` (see `crate::encoding`).
//! - [`KEY_INDEX_BUCKET_ID`] / `"key_index"` — the per-user-key
//!   revision history. Format lands at L839; this module reserves
//!   the ID and name only.
//!
//! Names match etcd's exactly so byte-equality holds at the bucket
//! level too. **Renaming any of these constants is a breaking change
//! to the on-disk format** — any deployed cluster's data files
//! would become unreadable.
//!
//! # Workspace bucket-ID registry
//!
//! No central authority enforces uniqueness; this module is the
//! de-facto registry. Known allocations as of L838:
//!
//! | id        | name                | crate                                 | role          |
//! | --------- | ------------------- | ------------------------------------- | ------------- |
//! | `0x0010`  | `"key"`             | `mango-mvcc` (this crate)             | MVCC payload  |
//! | `0x0011`  | `"key_index"`       | `mango-mvcc` (this crate)             | MVCC index    |
//! | `0xb007`  | (test-only)         | `benches/storage`                     | Bench harness |
//! | `1`/`2`   | (test-only)         | `mango-storage::tests`                | Doctests      |
//! | `99`      | (test-only)         | `mango-storage::tests`                | Doctests      |
//! | `0x1234`/`0x5678` | (test-only) | `mango-storage::tests`                | Doctests      |
//!
//! New allocations: pick the next free `u16` and add a row above.

use mango_storage::{Backend, BackendError, BucketId};

/// Name of the MVCC payload bucket. Etcd-equal.
///
/// Renaming this constant is a breaking change to the on-disk
/// format.
pub const KEY_BUCKET_NAME: &str = "key";

/// Name of the MVCC per-key history bucket. Etcd-equal.
///
/// Renaming this constant is a breaking change to the on-disk
/// format.
pub const KEY_INDEX_BUCKET_NAME: &str = "key_index";

/// `BucketId` for the MVCC payload bucket. See [`KEY_BUCKET_NAME`].
pub const KEY_BUCKET_ID: BucketId = BucketId::new(0x0010);

/// `BucketId` for the MVCC per-key history bucket. See
/// [`KEY_INDEX_BUCKET_NAME`].
pub const KEY_INDEX_BUCKET_ID: BucketId = BucketId::new(0x0011);

/// Register both MVCC buckets on `backend`.
///
/// Idempotent on retry against the same `(name, id)` pair (the
/// `Backend` contract), so repeat calls during start-up retries are
/// safe.
///
/// # Atomicity
///
/// **NOT atomic across the two buckets.** If the second
/// `register_bucket` call fails, the first registration has already
/// persisted. Callers MUST treat partial failures as fatal — drop
/// the backend and surface the error. In production this is called
/// once at startup with the constants above, so the only realistic
/// failure mode is [`BackendError::Closed`], which is process-level
/// already.
///
/// # Errors
///
/// Forwards any [`BackendError`] from [`Backend::register_bucket`].
/// In particular: [`BackendError::BucketConflict`] if a different
/// name was previously registered to one of these IDs (or vice
/// versa via [`BackendError::BucketNameConflict`]).
pub fn register<B: Backend>(backend: &B) -> Result<(), BackendError> {
    backend.register_bucket(KEY_BUCKET_NAME, KEY_BUCKET_ID)?;
    backend.register_bucket(KEY_INDEX_BUCKET_NAME, KEY_INDEX_BUCKET_ID)?;
    Ok(())
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
    use mango_storage::{BackendConfig, RedbBackend};
    use tempfile::tempdir;

    #[test]
    fn ids_are_distinct() {
        assert_ne!(KEY_BUCKET_ID, KEY_INDEX_BUCKET_ID);
    }

    #[test]
    fn id_values_are_pinned() {
        assert_eq!(KEY_BUCKET_ID.raw, 0x0010);
        assert_eq!(KEY_INDEX_BUCKET_ID.raw, 0x0011);
    }

    #[test]
    fn names_are_distinct_and_pinned() {
        assert_eq!(KEY_BUCKET_NAME, "key");
        assert_eq!(KEY_INDEX_BUCKET_NAME, "key_index");
        assert_ne!(KEY_BUCKET_NAME, KEY_INDEX_BUCKET_NAME);
    }

    fn open_backend() -> (RedbBackend, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let cfg = BackendConfig::new(dir.path().to_path_buf(), false);
        let backend = RedbBackend::open(cfg).expect("open redb");
        (backend, dir)
    }

    #[test]
    fn register_against_real_backend() {
        let (backend, _dir) = open_backend();
        register(&backend).expect("first register");
        // Idempotent on the same (name, id) pairs per `Backend` contract.
        register(&backend).expect("second register is idempotent");
    }

    #[test]
    fn register_rejects_id_rebind() {
        let (backend, _dir) = open_backend();
        // Reserve KEY_BUCKET_ID under the canonical name.
        backend
            .register_bucket(KEY_BUCKET_NAME, KEY_BUCKET_ID)
            .expect("first register");
        // Attempt to reuse the same id with a different name —
        // contract requires `BucketConflict`.
        let err = backend
            .register_bucket("rogue_name", KEY_BUCKET_ID)
            .expect_err("must reject id rebind");
        assert!(
            matches!(err, BackendError::BucketConflict { id, .. } if id == KEY_BUCKET_ID),
            "got {err:?}"
        );
    }

    #[test]
    fn register_rejects_name_rebind() {
        let (backend, _dir) = open_backend();
        backend
            .register_bucket(KEY_BUCKET_NAME, KEY_BUCKET_ID)
            .expect("first register");
        let rogue_id = BucketId::new(0xDEAD);
        let err = backend
            .register_bucket(KEY_BUCKET_NAME, rogue_id)
            .expect_err("must reject name rebind");
        assert!(
            matches!(err, BackendError::BucketNameConflict { requested_id, .. } if requested_id == rogue_id),
            "got {err:?}"
        );
    }
}
