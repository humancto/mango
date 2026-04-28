//! In-memory reference [`crate::Backend`] impl (ROADMAP:821).
//!
//! `InMemBackend` is a `BTreeMap`-backed second `Backend` impl.
//! Its purpose is to validate the trait boundary frozen in ADR
//! 0002 §6 — the engine-swap dry-run test
//! (`tests/engine_swap_dryrun.rs`) migrates state from a
//! [`crate::RedbBackend`] into an `InMemBackend` and asserts every
//! observable read and every error variant agrees. If the trait
//! ever leaks engine-specific behavior, this is where it surfaces.
//!
//! It is NOT a production engine. There is no durability, no
//! crash recovery, and no concurrency model beyond a single
//! `parking_lot::RwLock`. The on-disk size accessor returns 0;
//! `defragment` is a no-op; `force_fsync = true` is observably a
//! no-op (documented; exercised in T2 of the dry-run).
//!
//! # Visibility
//!
//! Gated behind the `test-utils` Cargo feature so the public
//! surface of `mango-storage` under default features is
//! unaffected. `cargo-public-api` (workspace policy) sees no
//! addition.
//!
//! # Atomicity
//!
//! `commit_batch` and `commit_group` use **stage-then-swap** —
//! ops are applied to a clone of every touched bucket, validated
//! end-to-end, and the touched entries are atomically replaced
//! into `InMemState.buckets_by_id`. A panic mid-apply leaves the
//! visible state untouched, matching `RedbBackend`'s transactional
//! commit contract.
//!
//! # Lock discipline
//!
//! `parking_lot::RwLock` is non-poisoning and non-reentrant. The
//! apply loop never calls any `&self` method that would re-enter
//! the lock (it operates on owned clones until the final swap),
//! so deadlock is impossible.

pub(crate) mod batch;
pub(crate) mod snapshot;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::RwLock;

use crate::backend::{Backend, BackendConfig, BackendError, BucketId, CommitStamp};
use batch::{InMemBatch, StagedOp};
use snapshot::{InMemSnapshot, SnapshotBuckets};

/// Shared state. Held behind `Arc<RwLock<…>>` so the public
/// `InMemBackend` handle can be cheaply cloned and `close()` can
/// take `&self`.
#[derive(Debug)]
struct InMemState {
    buckets_by_id: HashMap<BucketId, BucketEntry>,
    buckets_by_name: HashMap<String, BucketId>,
    closed: bool,
    /// Strictly-monotonic commit cursor. Always returned from
    /// `commit_batch`/`commit_group` via `checked_add(1)` —
    /// workspace `arithmetic_side_effects = deny` rejects naked
    /// `+= 1`. Mirrors `RedbBackend::commit_seq` semantics.
    next_seq: u64,
}

#[derive(Debug, Clone)]
struct BucketEntry {
    name: String,
    data: BTreeMap<Vec<u8>, Bytes>,
}

/// In-memory `Backend` implementation. See module docs.
#[derive(Debug, Clone)]
pub struct InMemBackend {
    state: Arc<RwLock<InMemState>>,
}

impl InMemBackend {
    /// Snapshot the registered bucket-id set under a read lock.
    /// Used by `snapshot()` to give `InMemSnapshot::range`/`get`
    /// faithful `UnknownBucket` errors even for buckets registered
    /// after the snapshot's clone of `buckets_by_id`.
    fn registered_ids_locked(state: &InMemState) -> Arc<HashSet<BucketId>> {
        Arc::new(state.buckets_by_id.keys().copied().collect())
    }

    /// Snapshot the bucket data forest under a read lock.
    fn buckets_locked(state: &InMemState) -> SnapshotBuckets {
        let map: HashMap<BucketId, BTreeMap<Vec<u8>, Bytes>> = state
            .buckets_by_id
            .iter()
            .map(|(id, entry)| (*id, entry.data.clone()))
            .collect();
        Arc::new(map)
    }
}

impl Backend for InMemBackend {
    type Snapshot = InMemSnapshot;
    type Batch = InMemBatch;

    fn open(config: BackendConfig) -> Result<Self, BackendError> {
        // Mirror RedbBackend's read-only rejection so error-taxonomy
        // parity holds in T3. When the MVCC layer formalizes
        // read-only, both impls update together.
        if config.read_only {
            return Err(BackendError::Other(
                "read-only not yet supported; see ROADMAP:817 follow-up".to_owned(),
            ));
        }
        // `data_dir` is ignored — InMem has no on-disk backing.
        let _ = config.data_dir;
        Ok(Self {
            state: Arc::new(RwLock::new(InMemState {
                buckets_by_id: HashMap::new(),
                buckets_by_name: HashMap::new(),
                closed: false,
                next_seq: 0,
            })),
        })
    }

    fn close(&self) -> Result<(), BackendError> {
        // Idempotent. Setting `closed = true` is the single
        // observable effect; no resources to release.
        let mut s = self.state.write();
        s.closed = true;
        Ok(())
    }

    fn register_bucket(&self, name: &str, id: BucketId) -> Result<(), BackendError> {
        let mut s = self.state.write();
        if s.closed {
            return Err(BackendError::Closed);
        }
        // Conflict precedence pinned to RedbBackend's registry
        // (`crates/mango-storage/src/redb/registry.rs::check_only`):
        // 1. id-rebind to a different name -> BucketConflict.
        // 2. name-rebind to a different id -> BucketNameConflict.
        // 3. matching (name, id) -> Ok(()) (idempotent).
        if let Some(existing) = s.buckets_by_id.get(&id) {
            if existing.name == name {
                return Ok(());
            }
            return Err(BackendError::BucketConflict {
                id,
                existing: existing.name.clone(),
                requested: name.to_owned(),
            });
        }
        if let Some(existing_id) = s.buckets_by_name.get(name) {
            return Err(BackendError::BucketNameConflict {
                name: name.to_owned(),
                existing_id: *existing_id,
                requested_id: id,
            });
        }
        s.buckets_by_id.insert(
            id,
            BucketEntry {
                name: name.to_owned(),
                data: BTreeMap::new(),
            },
        );
        s.buckets_by_name.insert(name.to_owned(), id);
        Ok(())
    }

    fn snapshot(&self) -> Result<Self::Snapshot, BackendError> {
        let s = self.state.read();
        if s.closed {
            return Err(BackendError::Closed);
        }
        let buckets = Self::buckets_locked(&s);
        let registered_ids = Self::registered_ids_locked(&s);
        Ok(InMemSnapshot::new(buckets, registered_ids))
    }

    fn begin_batch(&self) -> Result<Self::Batch, BackendError> {
        let s = self.state.read();
        if s.closed {
            return Err(BackendError::Closed);
        }
        Ok(InMemBatch::new())
    }

    fn commit_batch(
        &self,
        batch: Self::Batch,
        force_fsync: bool,
    ) -> impl core::future::Future<Output = Result<CommitStamp, BackendError>> + Send {
        // Sync prologue: extract staged ops BEFORE constructing the
        // future, so the `!Send` `InMemBatch` never enters the
        // future's capture set. Mirrors the RedbBackend pattern.
        let staged = batch.into_staged();
        let state = Arc::clone(&self.state);
        // No durable medium; `force_fsync` is observably a no-op
        // (T2 of the dry-run pins this contract).
        let _ = force_fsync;
        async move { commit_staged(&state, staged) }
    }

    fn commit_group(
        &self,
        batches: Vec<Self::Batch>,
    ) -> impl core::future::Future<Output = Result<CommitStamp, BackendError>> + Send {
        let mut merged: Vec<StagedOp> = Vec::new();
        for b in batches {
            merged.extend(b.into_staged());
        }
        let state = Arc::clone(&self.state);
        async move { commit_staged(&state, merged) }
    }

    fn size_on_disk(&self) -> Result<u64, BackendError> {
        let s = self.state.read();
        if s.closed {
            return Err(BackendError::Closed);
        }
        // Documented contract: in-mem has no disk.
        Ok(0)
    }

    fn defragment(&self) -> impl core::future::Future<Output = Result<(), BackendError>> + Send {
        let state = Arc::clone(&self.state);
        async move {
            let s = state.read();
            if s.closed {
                return Err(BackendError::Closed);
            }
            // Documented no-op.
            Ok(())
        }
    }
}

/// Apply staged ops with stage-then-swap atomicity.
///
/// 1. Acquire write lock; capture `closed` cut.
/// 2. Validate every op (`UnknownBucket`, `InvalidRange`) BEFORE any
///    mutation. Validation errors leave state untouched.
/// 3. Clone every `BucketEntry.data` whose `BucketId` is touched
///    by any op. The clones are the staging area.
/// 4. Apply ops to the staging clones. Empty `end` on
///    `DeleteRange` means "unbounded upper" per the engine-neutral
///    contract — see `crate::redb::mod::apply_staged` lines
///    296-310 for the canonical definition; mirrored here exactly.
/// 5. Swap the staged clones back into `state.buckets_by_id`.
/// 6. Bump `next_seq` via `checked_add(1)` and return the new
///    stamp.
///
/// A panic anywhere in steps 3-5 leaves visible state untouched
/// because the staging clones are in-scope locals; only the final
/// step writes back. Matches `RedbBackend`'s transactional contract.
fn commit_staged(
    state: &Arc<RwLock<InMemState>>,
    ops: Vec<StagedOp>,
) -> Result<CommitStamp, BackendError> {
    let mut s = state.write();
    if s.closed {
        return Err(BackendError::Closed);
    }

    // Validate every op: bucket exists, range bounds sane.
    for op in &ops {
        let b = op_bucket(op);
        if !s.buckets_by_id.contains_key(&b) {
            return Err(BackendError::UnknownBucket(b));
        }
        if let StagedOp::DeleteRange { start, end, .. } = op {
            // Empty `end` means "unbounded upper"; any start is
            // legal in that case (mirrors validate_ops in
            // crate::redb::mod).
            if !end.is_empty() && start > end {
                return Err(BackendError::InvalidRange("start > end"));
            }
        }
    }

    // Stage: clone touched buckets. Use the Entry API so the
    // existence-validated lookup happens exactly once per bucket
    // and we never call `expect()` on the result.
    let mut touched: HashMap<BucketId, BTreeMap<Vec<u8>, Bytes>> = HashMap::new();
    for op in &ops {
        let b = op_bucket(op);
        if let std::collections::hash_map::Entry::Vacant(slot) = touched.entry(b) {
            // Existence validated in the loop above; an absent
            // entry here would be a logic bug, surfaced as
            // `UnknownBucket` rather than a panic.
            let Some(entry) = s.buckets_by_id.get(&b) else {
                return Err(BackendError::UnknownBucket(b));
            };
            slot.insert(entry.data.clone());
        }
    }

    // Apply ops to the clones.
    for op in ops {
        let b = op_bucket(&op);
        let Some(map) = touched.get_mut(&b) else {
            // Same logic-bug surface as above; convert to error
            // rather than panic.
            return Err(BackendError::UnknownBucket(b));
        };
        match op {
            StagedOp::Put { key, value, .. } => {
                map.insert(key, Bytes::from(value));
            }
            StagedOp::Delete { key, .. } => {
                map.remove(&key);
            }
            StagedOp::DeleteRange { start, end, .. } => {
                if end.is_empty() {
                    // Unbounded upper.
                    let to_remove: Vec<Vec<u8>> = map
                        .range::<[u8], _>((Bound::Included(start.as_slice()), Bound::Unbounded))
                        .map(|(k, _)| k.clone())
                        .collect();
                    for k in to_remove {
                        map.remove(&k);
                    }
                } else {
                    let to_remove: Vec<Vec<u8>> = map
                        .range::<[u8], _>((
                            Bound::Included(start.as_slice()),
                            Bound::Excluded(end.as_slice()),
                        ))
                        .map(|(k, _)| k.clone())
                        .collect();
                    for k in to_remove {
                        map.remove(&k);
                    }
                }
            }
        }
    }

    // Swap: replace touched buckets atomically (under the same
    // write lock).
    for (b, data) in touched {
        if let Some(entry) = s.buckets_by_id.get_mut(&b) {
            entry.data = data;
        }
    }

    // Mint a strictly-monotonic stamp. Empty commit also bumps —
    // matches RedbBackend (`commit_staged` in redb/mod.rs:527).
    let prev = s.next_seq;
    let next = prev
        .checked_add(1)
        .ok_or_else(|| BackendError::Other("commit_seq overflow".to_owned()))?;
    s.next_seq = next;
    Ok(CommitStamp::new(next))
}

/// Extract the `BucketId` from any `StagedOp` variant.
fn op_bucket(op: &StagedOp) -> BucketId {
    match *op {
        StagedOp::Put { bucket, .. }
        | StagedOp::Delete { bucket, .. }
        | StagedOp::DeleteRange { bucket, .. } => bucket,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use super::*;
    use crate::backend::{ReadSnapshot, WriteBatch};

    #[allow(dead_code)]
    fn _assert_inmem_backend_send_sync_static() {
        fn needs<T: Send + Sync + 'static>() {}
        needs::<InMemBackend>();
    }

    fn open() -> InMemBackend {
        InMemBackend::open(BackendConfig::new("/unused".into(), false)).unwrap()
    }

    #[tokio::test]
    async fn open_read_only_is_rejected() {
        let err =
            InMemBackend::open(BackendConfig::new("/unused".into(), true)).expect_err("rejected");
        match err {
            BackendError::Other(msg) => {
                assert!(msg.contains("read-only"), "msg = {msg}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_bucket_idempotent_then_id_conflict() {
        let b = open();
        b.register_bucket("kv", BucketId::new(1)).unwrap();
        b.register_bucket("kv", BucketId::new(1)).unwrap(); // idempotent
        let err = b
            .register_bucket("meta", BucketId::new(1))
            .expect_err("conflict");
        match err {
            BackendError::BucketConflict {
                id,
                existing,
                requested,
            } => {
                assert_eq!(id, BucketId::new(1));
                assert_eq!(existing, "kv");
                assert_eq!(requested, "meta");
            }
            other => panic!("expected BucketConflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_bucket_name_conflict() {
        let b = open();
        b.register_bucket("kv", BucketId::new(1)).unwrap();
        let err = b
            .register_bucket("kv", BucketId::new(2))
            .expect_err("name conflict");
        match err {
            BackendError::BucketNameConflict {
                name,
                existing_id,
                requested_id,
            } => {
                assert_eq!(name, "kv");
                assert_eq!(existing_id, BucketId::new(1));
                assert_eq!(requested_id, BucketId::new(2));
            }
            other => panic!("expected BucketNameConflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn id_conflict_checked_before_name_conflict() {
        let b = open();
        b.register_bucket("a", BucketId::new(1)).unwrap();
        b.register_bucket("b", BucketId::new(2)).unwrap();
        let err = b
            .register_bucket("a", BucketId::new(2))
            .expect_err("id conflict wins");
        match err {
            BackendError::BucketConflict { existing, .. } => assert_eq!(existing, "b"),
            other => panic!("expected BucketConflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn batch_round_trip_via_commit_then_snapshot() {
        let b = open();
        b.register_bucket("kv", BucketId::new(1)).unwrap();
        let mut batch = b.begin_batch().unwrap();
        batch.put(BucketId::new(1), b"k1", b"v1").unwrap();
        batch.put(BucketId::new(1), b"k2", b"v2").unwrap();
        let stamp1 = b.commit_batch(batch, false).await.unwrap();
        assert_eq!(stamp1, CommitStamp::new(1));

        let snap = b.snapshot().unwrap();
        assert_eq!(
            snap.get(BucketId::new(1), b"k1").unwrap(),
            Some(Bytes::from_static(b"v1"))
        );
        assert_eq!(
            snap.get(BucketId::new(1), b"k2").unwrap(),
            Some(Bytes::from_static(b"v2"))
        );
        assert_eq!(snap.get(BucketId::new(1), b"absent").unwrap(), None);
    }

    #[tokio::test]
    async fn force_fsync_true_is_observable_noop_returning_ok() {
        let b = open();
        b.register_bucket("kv", BucketId::new(1)).unwrap();
        let mut batch = b.begin_batch().unwrap();
        batch.put(BucketId::new(1), b"k", b"v").unwrap();
        let s = b.commit_batch(batch, true).await.unwrap();
        assert_eq!(s, CommitStamp::new(1));
    }

    #[tokio::test]
    async fn empty_commit_bumps_seq_strictly() {
        let b = open();
        let s1 = b
            .commit_batch(b.begin_batch().unwrap(), false)
            .await
            .unwrap();
        let s2 = b
            .commit_batch(b.begin_batch().unwrap(), false)
            .await
            .unwrap();
        assert!(s1 < s2);
        assert_eq!(s2.seq, s1.seq + 1);
    }

    #[tokio::test]
    async fn unknown_bucket_on_get() {
        let b = open();
        let snap = b.snapshot().unwrap();
        let err = snap.get(BucketId::new(99), b"k").expect_err("unknown");
        assert!(matches!(err, BackendError::UnknownBucket(id) if id == BucketId::new(99)));
    }

    #[tokio::test]
    async fn invalid_range_on_snapshot_range() {
        let b = open();
        b.register_bucket("kv", BucketId::new(1)).unwrap();
        let snap = b.snapshot().unwrap();
        let err = snap
            .range(BucketId::new(1), b"z", b"a")
            .err()
            .expect("invalid");
        assert!(matches!(err, BackendError::InvalidRange(_)));
    }

    #[tokio::test]
    async fn delete_range_unbounded_upper_with_empty_end() {
        // Mirrors redb behavior: end == [] in DeleteRange means
        // "delete from start to +infinity".
        let b = open();
        b.register_bucket("kv", BucketId::new(1)).unwrap();
        let mut batch = b.begin_batch().unwrap();
        batch.put(BucketId::new(1), b"a", b"1").unwrap();
        batch.put(BucketId::new(1), b"m", b"2").unwrap();
        batch.put(BucketId::new(1), b"z", b"3").unwrap();
        let _ = b.commit_batch(batch, false).await.unwrap();

        let mut batch = b.begin_batch().unwrap();
        batch.delete_range(BucketId::new(1), b"m", b"").unwrap();
        let _ = b.commit_batch(batch, false).await.unwrap();

        let snap = b.snapshot().unwrap();
        assert_eq!(
            snap.get(BucketId::new(1), b"a").unwrap(),
            Some(Bytes::from_static(b"1"))
        );
        assert_eq!(snap.get(BucketId::new(1), b"m").unwrap(), None);
        assert_eq!(snap.get(BucketId::new(1), b"z").unwrap(), None);
    }

    #[tokio::test]
    async fn close_is_idempotent_and_blocks_subsequent_ops() {
        let b = open();
        b.register_bucket("kv", BucketId::new(1)).unwrap();
        b.close().unwrap();
        b.close().unwrap();
        let err = b.snapshot().expect_err("closed");
        assert!(matches!(err, BackendError::Closed));
        let err = b.begin_batch().expect_err("closed");
        assert!(matches!(err, BackendError::Closed));
    }

    #[tokio::test]
    async fn snapshot_isolated_from_post_snapshot_commits() {
        let b = open();
        b.register_bucket("kv", BucketId::new(1)).unwrap();
        let mut batch = b.begin_batch().unwrap();
        batch.put(BucketId::new(1), b"k", b"v1").unwrap();
        let _ = b.commit_batch(batch, false).await.unwrap();

        let snap = b.snapshot().unwrap();

        let mut batch = b.begin_batch().unwrap();
        batch.put(BucketId::new(1), b"k", b"v2").unwrap();
        let _ = b.commit_batch(batch, false).await.unwrap();

        // Snapshot still sees v1.
        assert_eq!(
            snap.get(BucketId::new(1), b"k").unwrap(),
            Some(Bytes::from_static(b"v1"))
        );
    }

    #[tokio::test]
    async fn size_on_disk_is_zero() {
        let b = open();
        assert_eq!(b.size_on_disk().unwrap(), 0);
    }

    #[tokio::test]
    async fn defragment_is_noop_ok() {
        let b = open();
        b.defragment().await.unwrap();
    }

    #[tokio::test]
    async fn commit_group_is_atomic_across_batches() {
        let b = open();
        b.register_bucket("kv", BucketId::new(1)).unwrap();
        let mut b1 = b.begin_batch().unwrap();
        b1.put(BucketId::new(1), b"a", b"1").unwrap();
        let mut b2 = b.begin_batch().unwrap();
        b2.put(BucketId::new(1), b"b", b"2").unwrap();
        let stamp = b.commit_group(vec![b1, b2]).await.unwrap();
        assert_eq!(stamp, CommitStamp::new(1));

        let snap = b.snapshot().unwrap();
        assert_eq!(
            snap.get(BucketId::new(1), b"a").unwrap(),
            Some(Bytes::from_static(b"1"))
        );
        assert_eq!(
            snap.get(BucketId::new(1), b"b").unwrap(),
            Some(Bytes::from_static(b"2"))
        );
    }

    #[tokio::test]
    async fn unknown_bucket_in_batch_is_rejected_at_commit() {
        let b = open();
        let mut batch = b.begin_batch().unwrap();
        batch.put(BucketId::new(99), b"k", b"v").unwrap();
        let err = b.commit_batch(batch, false).await.expect_err("unknown");
        assert!(matches!(err, BackendError::UnknownBucket(id) if id == BucketId::new(99)));
    }
}
