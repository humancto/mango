//! [`ReadSnapshot`] impl for the redb backend.
//!
//! A [`RedbSnapshot`] wraps a `redb::ReadTransaction` plus a shared
//! handle back to the backend's state (for registry lookups). Read
//! transactions in redb 4.x are `'static` and `Send + Sync` (see
//! `redb-4.1.0/src/transactions.rs` for the `ReadTransaction`
//! declaration); snapshots can therefore move freely across tasks
//! and outlive the method that produced them.
//!
//! Snapshot isolation is redb's guarantee: a `ReadTransaction`
//! observes the database state as of the moment `begin_read()`
//! returned, regardless of subsequent commits. Our snapshot does
//! not need to snapshot the in-memory registry alongside — the
//! registry only *grows* (bucket ids are never unregistered), so
//! a read that observes a post-snapshot bucket id against a
//! pre-snapshot data state correctly returns `None` (the key does
//! not exist in the snapshot's cut).

use std::sync::Arc;

use bytes::Bytes;

use crate::backend::{BackendError, BucketId, RangeIter, ReadSnapshot};
use crate::redb::registry::physical_table_name;
use crate::redb::value_compression;
use crate::redb::{map_storage_error, map_table_error, Inner};

/// Point-in-time read snapshot. Produced by
/// [`crate::Backend::snapshot`]. See [`ReadSnapshot`] for the
/// contract.
#[derive(Debug)]
pub struct RedbSnapshot {
    txn: ::redb::ReadTransaction,
    inner: Arc<Inner>,
}

impl RedbSnapshot {
    pub(super) fn new(txn: ::redb::ReadTransaction, inner: Arc<Inner>) -> Self {
        Self { txn, inner }
    }
}

impl ReadSnapshot for RedbSnapshot {
    fn get(&self, bucket: BucketId, key: &[u8]) -> Result<Option<Bytes>, BackendError> {
        if !self.inner.registry.read().contains_id(bucket) {
            return Err(BackendError::UnknownBucket(bucket));
        }
        let name = physical_table_name(bucket);
        let td: ::redb::TableDefinition<&[u8], &[u8]> = ::redb::TableDefinition::new(&name);
        let table = match self.txn.open_table(td) {
            Ok(t) => t,
            // A bucket registered in the registry but absent from
            // redb means no write ever hit it — that's not an
            // error, it's "no data yet". Return None to match.
            Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(map_table_error(e)),
        };
        match table.get(key).map_err(map_storage_error)? {
            // ROADMAP:830: stored bytes carry a 1-byte compression
            // tag prefix; decode is config-blind (dispatches on the
            // tag), so a database written under any mode is readable
            // here. Errors here surface as `BackendError::Corruption`.
            Some(v) => Ok(Some(value_compression::decode(v.value())?)),
            None => Ok(None),
        }
    }

    fn range<'a>(
        &'a self,
        bucket: BucketId,
        start: &'a [u8],
        end: &'a [u8],
    ) -> Result<Box<dyn RangeIter<'a> + 'a>, BackendError> {
        if start > end {
            return Err(BackendError::InvalidRange("start > end"));
        }
        if !self.inner.registry.read().contains_id(bucket) {
            return Err(BackendError::UnknownBucket(bucket));
        }
        let name = physical_table_name(bucket);
        let td: ::redb::TableDefinition<&[u8], &[u8]> = ::redb::TableDefinition::new(&name);
        let table = match self.txn.open_table(td) {
            Ok(t) => t,
            // No writes have landed into this bucket yet. Return an
            // empty iterator rather than erroring — this matches
            // the `get`-on-empty-bucket semantics above.
            Err(::redb::TableError::TableDoesNotExist(_)) => {
                return Ok(Box::new(EmptyRangeIter));
            }
            Err(e) => return Err(map_table_error(e)),
        };
        let iter = table
            .range::<&[u8]>(start..end)
            .map_err(map_storage_error)?;
        Ok(Box::new(RedbRangeIter { inner: iter }))
    }
}

/// Iterator adapter wrapping `redb::Range`. The `redb::Range` type
/// itself is `'static` in 4.x (see the `ReadOnlyTable::range`
/// signature at `redb-4.1.0/src/table.rs:508-515`), so this struct
/// has no self-referential lifetime concern: the range holds its
/// own `Arc<TransactionGuard>` internally, keeping the snapshot
/// alive.
struct RedbRangeIter {
    inner: ::redb::Range<'static, &'static [u8], &'static [u8]>,
}

impl Iterator for RedbRangeIter {
    type Item = Result<(Bytes, Bytes), BackendError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next()? {
            Ok((k, v)) => {
                let kb = Bytes::copy_from_slice(k.value());
                // ROADMAP:830: see the matching comment in
                // `RedbSnapshot::get`. Codec corruption surfaces here
                // as `BackendError::Corruption` and ends iteration on
                // the caller side as soon as they handle the `Err`.
                let vb = match value_compression::decode(v.value()) {
                    Ok(b) => b,
                    Err(e) => return Some(Err(e)),
                };
                Some(Ok((kb, vb)))
            }
            Err(e) => Some(Err(map_storage_error(e))),
        }
    }
}

// Lifetime `'a` is unused by our concrete type (nothing borrows
// from the snapshot); the trait requires it for the `dyn` trait
// object but we satisfy it for any `'a` by not capturing anything.
// The same reasoning applies to `EmptyRangeIter` below.
impl RangeIter<'_> for RedbRangeIter {}

/// Iterator that yields nothing. Used when the requested bucket
/// is registered but no writes have created the underlying redb
/// table yet.
struct EmptyRangeIter;

impl Iterator for EmptyRangeIter {
    type Item = Result<(Bytes, Bytes), BackendError>;

    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

impl RangeIter<'_> for EmptyRangeIter {}
