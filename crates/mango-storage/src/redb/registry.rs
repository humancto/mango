//! Bucket-name ⇄ [`BucketId`] registry for the redb backend.
//!
//! The registry is the single source of truth for bucket naming at
//! runtime. Persistence to disk is handled by the outer backend
//! (`super::backend` lands in a later commit in this PR); this
//! module owns only the in-memory structure and the conflict-
//! resolution logic.
//!
//! Physical table names inside redb are derived from [`BucketId`]
//! via [`physical_table_name`] — user-supplied bucket names never
//! flow into redb's namespace, so (a) user-facing names can use
//! any byte sequence we later choose to allow without coupling to
//! redb's identifier rules, and (b) the registry's own metadata
//! table cannot collide with a user-named bucket.

use std::collections::HashMap;

use crate::backend::{BackendError, BucketId};

/// The name of the redb table the registry is persisted to. The
/// `__mango_` prefix is reserved for internal mango tables;
/// [`physical_table_name`] never emits a name with that prefix, so
/// no collision is possible.
pub(crate) const REGISTRY_TABLE_NAME: &str = "__mango_bucket_registry";

/// Compute the redb table name that a [`BucketId`] maps to. Hex-
/// encoded so the table listing is sorted by id and the encoding
/// is unambiguous (decimal `10` would collide visually with the
/// 2-digit range `1..=10`; hex does not).
#[must_use]
pub(crate) fn physical_table_name(id: BucketId) -> String {
    format!("bucket_{:04x}", id.raw)
}

/// Outcome of [`Registry::check_and_insert`]. Distinguishes
/// idempotent re-registration (no disk write required) from a
/// genuine insert (caller must persist).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RegisterOutcome {
    /// The `(name, id)` pair was already present. Caller skips
    /// the registry-table write.
    AlreadyRegistered,
    /// The pair is newly inserted into the in-memory view. Caller
    /// MUST persist it to the registry table before returning
    /// success to the user.
    Inserted,
}

/// In-memory registry. Construct empty with [`Registry::default`];
/// populate on backend open by reading the registry table.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    by_name: HashMap<String, BucketId>,
    by_id: HashMap<u16, String>,
}

impl Registry {
    /// Try to register `(name, id)`. On conflict, returns the
    /// appropriate [`BackendError`] variant:
    ///
    /// - id already bound to a different name → [`BackendError::BucketConflict`]
    /// - name already bound to a different id → [`BackendError::BucketNameConflict`]
    ///
    /// The id case is checked first because it is the more
    /// common operator mistake (wiring a duplicate id from a
    /// const table). Ordering is documented rather than load-
    /// bearing; a caller that exercises both simultaneously gets
    /// one of the two variants deterministically.
    pub(crate) fn check_and_insert(
        &mut self,
        name: &str,
        id: BucketId,
    ) -> Result<RegisterOutcome, BackendError> {
        if let Some(existing_name) = self.by_id.get(&id.raw) {
            if existing_name == name {
                return Ok(RegisterOutcome::AlreadyRegistered);
            }
            return Err(BackendError::BucketConflict {
                id,
                existing: existing_name.clone(),
                requested: name.to_owned(),
            });
        }
        if let Some(existing_id) = self.by_name.get(name) {
            return Err(BackendError::BucketNameConflict {
                name: name.to_owned(),
                existing_id: *existing_id,
                requested_id: id,
            });
        }
        self.by_name.insert(name.to_owned(), id);
        self.by_id.insert(id.raw, name.to_owned());
        Ok(RegisterOutcome::Inserted)
    }

    /// Insert a `(name, id)` pair unconditionally. Used by the
    /// open-time hydration path where the on-disk registry table
    /// is the authoritative source — conflicts there imply
    /// corruption, not user error.
    ///
    /// Returns the previous id for this name if any (used by the
    /// hydration caller to flag unexpected duplicates as
    /// corruption).
    pub(crate) fn force_insert(&mut self, name: String, id: BucketId) -> Option<BucketId> {
        let prior = self.by_name.insert(name.clone(), id);
        self.by_id.insert(id.raw, name);
        prior
    }

    /// Whether `id` is currently registered. Used by write paths
    /// to reject puts/deletes against unregistered buckets before
    /// opening a redb transaction.
    #[must_use]
    pub(crate) fn contains_id(&self, id: BucketId) -> bool {
        self.by_id.contains_key(&id.raw)
    }

    /// Number of registered buckets. Test-only observability.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_id.len()
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
    fn physical_table_name_is_hex_four_wide() {
        assert_eq!(physical_table_name(BucketId::new(0)), "bucket_0000");
        assert_eq!(physical_table_name(BucketId::new(1)), "bucket_0001");
        assert_eq!(physical_table_name(BucketId::new(0x10)), "bucket_0010");
        assert_eq!(physical_table_name(BucketId::new(0xFFFF)), "bucket_ffff");
    }

    #[test]
    fn physical_table_name_cannot_collide_with_registry_table() {
        // The registry table uses a reserved prefix that
        // `physical_table_name` never emits. This is load-bearing:
        // if the encoding ever changes to include `__mango_`, the
        // registry would self-collide.
        for raw in [0u16, 1, 0x1000, 0xFFFF] {
            let n = physical_table_name(BucketId::new(raw));
            assert!(
                !n.starts_with("__mango_"),
                "physical name {n:?} leaks into reserved prefix"
            );
        }
        assert!(REGISTRY_TABLE_NAME.starts_with("__mango_"));
    }

    #[test]
    fn empty_registry_contains_nothing() {
        let r = Registry::default();
        assert_eq!(r.len(), 0);
        assert!(!r.contains_id(BucketId::new(0)));
        assert!(!r.contains_id(BucketId::new(42)));
    }

    #[test]
    fn insert_is_inserted_then_already_registered() {
        let mut r = Registry::default();
        assert_eq!(
            r.check_and_insert("kv", BucketId::new(1)).unwrap(),
            RegisterOutcome::Inserted
        );
        assert_eq!(r.len(), 1);
        assert!(r.contains_id(BucketId::new(1)));
        assert_eq!(
            r.check_and_insert("kv", BucketId::new(1)).unwrap(),
            RegisterOutcome::AlreadyRegistered
        );
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn id_rebind_returns_bucket_conflict() {
        let mut r = Registry::default();
        r.check_and_insert("kv", BucketId::new(1)).unwrap();
        match r.check_and_insert("meta", BucketId::new(1)) {
            Err(BackendError::BucketConflict {
                id,
                existing,
                requested,
            }) => {
                assert_eq!(id, BucketId::new(1));
                assert_eq!(existing, "kv");
                assert_eq!(requested, "meta");
            }
            other => panic!("expected BucketConflict, got {other:?}"),
        }
    }

    #[test]
    fn name_rebind_returns_bucket_name_conflict() {
        let mut r = Registry::default();
        r.check_and_insert("kv", BucketId::new(1)).unwrap();
        match r.check_and_insert("kv", BucketId::new(2)) {
            Err(BackendError::BucketNameConflict {
                name,
                existing_id,
                requested_id,
            }) => {
                assert_eq!(name, "kv");
                assert_eq!(existing_id, BucketId::new(1));
                assert_eq!(requested_id, BucketId::new(2));
            }
            other => panic!("expected BucketNameConflict, got {other:?}"),
        }
    }

    #[test]
    fn id_conflict_checked_before_name_conflict() {
        // Two separate pre-existing rows: ("a", 1) and ("b", 2).
        // Then attempt ("a", 2). Both the id (2 → "b") and the
        // name ("a" → 1) are rebind conflicts; the registry
        // documents that id-rebind wins.
        let mut r = Registry::default();
        r.check_and_insert("a", BucketId::new(1)).unwrap();
        r.check_and_insert("b", BucketId::new(2)).unwrap();
        match r.check_and_insert("a", BucketId::new(2)) {
            Err(BackendError::BucketConflict { existing, .. }) => {
                assert_eq!(existing, "b");
            }
            other => panic!("expected BucketConflict (id-rebind priority), got {other:?}"),
        }
    }

    #[test]
    fn force_insert_returns_prior_id() {
        let mut r = Registry::default();
        assert!(r.force_insert("kv".into(), BucketId::new(1)).is_none());
        assert!(r.contains_id(BucketId::new(1)));
        // Simulate a (buggy or corrupt) second row for the same
        // name with a different id. Caller flags this as
        // corruption; the helper reports the displaced id.
        let prior = r.force_insert("kv".into(), BucketId::new(5));
        assert_eq!(prior, Some(BucketId::new(1)));
        assert!(r.contains_id(BucketId::new(5)));
    }
}
