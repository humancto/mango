//! [`ReadSnapshot`] impl for the in-memory reference backend
//! (ROADMAP:821).
//!
//! Snapshot consistency is provided by clone-on-snapshot: the
//! snapshot owns its own `Arc<HashMap<BucketId, BTreeMap<…>>>`,
//! cloned from the live backend state at `snapshot()` time.
//! Subsequent commits never observe the snapshot's clone, and the
//! snapshot's clone never observes subsequent commits. This is
//! `O(total_keys)` per snapshot — acceptable for a reference impl.
//!
//! Mirrors the lifetime story of
//! [`crate::redb::snapshot::RedbSnapshot`] (concrete iterator type
//! is `'static` w.r.t. the dyn-trait `'a`; the borrow ties the
//! iterator to `&self` via the `BTreeMap` iterator's own `'a`).
//!
//! # Registry semantics
//!
//! The bucket-id registry is read **live** from the backend's
//! shared state on every `get`/`range`, mirroring
//! `RedbSnapshot::get`/`range` which call
//! `self.inner.registry.read().contains_id(bucket)` per call. The
//! registry only ever *grows* (bucket ids are never unregistered),
//! so a read that observes a post-snapshot bucket id against a
//! pre-snapshot data state correctly returns `Ok(None)` — the key
//! does not exist in the snapshot's cut. See the trait-doc on
//! [`crate::backend::ReadSnapshot::get`] /
//! [`crate::backend::ReadSnapshot::range`] for the pinned contract.

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::RwLock;

use crate::backend::{BackendError, BucketId, RangeIter, ReadSnapshot};
use crate::inmem::InMemState;

/// Cloned snapshot of every bucket's `BTreeMap`. Held behind
/// `Arc` so multiple snapshots share the same clone if produced
/// from the same commit cut (currently they don't — each
/// `snapshot()` clones afresh — but the type leaves that door
/// open).
pub(super) type SnapshotBuckets = Arc<HashMap<BucketId, BTreeMap<Vec<u8>, Bytes>>>;

/// Point-in-time read snapshot of an [`crate::InMemBackend`].
///
/// Construction (`InMemBackend::snapshot`) clones the bucket data
/// forest under a read lock; subsequent commits cannot mutate
/// what this snapshot sees. The bucket-id registry is read **live**
/// from the backend on each `get`/`range` call (matches
/// `RedbSnapshot`); see the module-level "Registry semantics" note.
#[derive(Debug)]
pub struct InMemSnapshot {
    pub(super) buckets: SnapshotBuckets,
    /// Shared handle back to the backend's state, used to read the
    /// live registry on each `get`/`range`. Mirrors
    /// `RedbSnapshot.inner` (which carries `Arc<Inner>` for the
    /// same purpose). The lock is held only briefly for a
    /// `contains_key` check; no long-held read locks here.
    pub(super) state: Arc<RwLock<InMemState>>,
}

impl InMemSnapshot {
    pub(super) fn new(buckets: SnapshotBuckets, state: Arc<RwLock<InMemState>>) -> Self {
        Self { buckets, state }
    }

    /// Live registry membership check. Acquires `state.read()` for
    /// the duration of a single `HashMap::contains_key` call —
    /// brief, never re-entered.
    fn registered(&self, bucket: BucketId) -> bool {
        self.state.read().buckets_by_id.contains_key(&bucket)
    }
}

impl ReadSnapshot for InMemSnapshot {
    fn get(&self, bucket: BucketId, key: &[u8]) -> Result<Option<Bytes>, BackendError> {
        if !self.registered(bucket) {
            return Err(BackendError::UnknownBucket(bucket));
        }
        // A registered bucket with no writes yet has no entry in
        // `buckets`; treat that as "no data" (matches RedbSnapshot's
        // TableDoesNotExist → None mapping). Buckets registered
        // *after* the snapshot was taken also land here — see the
        // module-level "Registry semantics" note.
        let Some(map) = self.buckets.get(&bucket) else {
            return Ok(None);
        };
        Ok(map.get(key).cloned())
    }

    fn range<'a>(
        &'a self,
        bucket: BucketId,
        start: &'a [u8],
        end: &'a [u8],
    ) -> Result<Box<dyn RangeIter<'a> + 'a>, BackendError> {
        // Mirror RedbSnapshot::range precedence exactly:
        // (1) start > end -> InvalidRange, (2) bucket existence,
        // (3) empty bucket -> empty iterator. Empty `end` does NOT
        // mean "unbounded" on the read side (that semantic is
        // delete_range-only); with non-empty `start` and empty
        // `end`, byte-comparison gives start > end, so InvalidRange
        // fires — matching redb.
        if start > end {
            return Err(BackendError::InvalidRange("start > end"));
        }
        if !self.registered(bucket) {
            return Err(BackendError::UnknownBucket(bucket));
        }
        let Some(map) = self.buckets.get(&bucket) else {
            return Ok(Box::new(EmptyRangeIter));
        };
        let bounds: (Bound<&[u8]>, Bound<&[u8]>) = (Bound::Included(start), Bound::Excluded(end));
        let inner = map.range::<[u8], _>(bounds);
        Ok(Box::new(InMemRangeIter { inner }))
    }
}

/// Iterator adapter wrapping `BTreeMap::range`. Lifetime `'a` is
/// the snapshot's; items are owned `Bytes` clones. `Send` because
/// `Vec<u8>: Sync` and `Bytes: Sync`.
struct InMemRangeIter<'a> {
    inner: std::collections::btree_map::Range<'a, Vec<u8>, Bytes>,
}

impl Iterator for InMemRangeIter<'_> {
    type Item = Result<(Bytes, Bytes), BackendError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .next()
            .map(|(k, v)| Ok((Bytes::copy_from_slice(k), v.clone())))
    }
}

impl<'a> RangeIter<'a> for InMemRangeIter<'a> {}

/// Yields nothing. Used when the bucket is registered but no
/// writes have created its `BTreeMap` entry yet — same shape as
/// `redb::snapshot::EmptyRangeIter`.
struct EmptyRangeIter;

impl Iterator for EmptyRangeIter {
    type Item = Result<(Bytes, Bytes), BackendError>;

    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

impl RangeIter<'_> for EmptyRangeIter {}
