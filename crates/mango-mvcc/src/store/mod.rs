//! User-facing MVCC store (L844).
//!
//! [`MvccStore`] wraps a [`mango_storage::Backend`] with the
//! etcd-shape `KV` API. This commit lands the struct skeleton plus
//! [`MvccStore::open`] and [`MvccStore::current_revision`]; `Put` /
//! `Range` / `DeleteRange` / `Txn` / `Compact` arrive in subsequent
//! commits per the L844 plan §8 commit sequence.
//!
//! # Locking model
//!
//! Single-writer / multi-reader, mirroring Raft's serial apply.
//!
//! - `writer: tokio::sync::Mutex<WriterState>` — held for the
//!   entire write op (`Put` / `DeleteRange` / `Txn` / `Compact`).
//!   Async-aware mutex so the guard is `Send` across
//!   `commit_batch().await` (`parking_lot` guards are `!Send`).
//! - `index: ShardedKeyIndex` — own per-shard `parking_lot::RwLock`
//!   s; reads parallel with writes.
//! - `keys_in_order: parking_lot::RwLock<BTreeMap<...>>` — ordered
//!   live-key set used by `Range`. **No `.await` is held under
//!   this lock.** `BTreeMap` (not `BTreeSet`) so a future watch
//!   cache can extend the value side without re-typing.
//! - `current_main: AtomicI64` — highest fully-published revision.
//!   Release-stored at end of every successful commit; Acquire-
//!   loaded by `Range` and `current_revision`.
//! - `compacted: AtomicI64` — compacted floor. Release-stored
//!   after the on-disk delete commit in `Compact`; Acquire-loaded
//!   by `Range`. Zero = none.
//!
//! # Lock ordering
//!
//! `writer` → `keys_in_order` (write) → `index` shard locks. Never
//! the reverse. The `Range` path takes only `keys_in_order` (read)
//! → `index` shard locks (read), no `writer` involvement.
//!
//! # Type parameter
//!
//! `B: Backend` is generic so tests run against `InMemBackend` and
//! production uses `RedbBackend`. `Backend` is not object-safe;
//! `dyn Backend` does not exist (use `MvccStore<impl Backend>`
//! instead).

pub mod lease;
pub mod range;
pub mod txn;
mod writer;

pub use lease::LeaseId;
pub use range::{KeyValue, RangeRequest, RangeResult};
pub use txn::{Compare, CompareOp, RequestOp, ResponseOp, TxnRequest, TxnResponse};

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};

use mango_storage::{Backend, ReadSnapshot};

use crate::bucket::{register, KEY_BUCKET_ID};
use crate::error::OpenError;
use crate::sharded_key_index::ShardedKeyIndex;

use self::writer::WriterState;

/// Best-effort cap on the number of pre-existing key-bucket
/// entries reported in [`OpenError::NonEmptyBackend::found_revs`].
/// Higher values pay an iteration cost on a non-empty backend that
/// the caller is going to discard anyway.
const NON_EMPTY_PROBE_CAP: u64 = 1024;

/// Upper-bound sentinel for the `KEY_BUCKET_ID` emptiness probe.
/// MVCC encoded keys are 17 (`Put`) or 18 (`Tombstone`) bytes (see
/// `crate::encoding`); a 32-byte all-`0xFF` slice is strictly
/// greater than any 18-byte sequence (longer prefix wins on byte
/// comparison), so `[start = &[], end = NON_EMPTY_PROBE_END)`
/// covers every valid encoded key. The Backend `range` semantic is
/// half-open `[start, end)` with no infinity sentinel; without an
/// explicit upper bound the probe would be empty.
const NON_EMPTY_PROBE_END: &[u8] = &[0xFF; 32];

/// User-facing MVCC store.
///
/// See module docs for locking model and lock ordering.
///
/// **L844 only opens against an empty backend** —
/// [`Self::open`] returns [`OpenError::NonEmptyBackend`] otherwise.
/// Restart-from-disk recovery lands in L852.
pub struct MvccStore<B: Backend> {
    /// Underlying storage backend. Owned (not `Arc`) so callers
    /// place this struct behind their own `Arc` if they want
    /// shared access.
    backend: B,
    /// Per-key revision history. Point-lookup; sharded.
    /// Read at construction time, populated by writer impls in
    /// later commits.
    #[allow(dead_code)]
    index: ShardedKeyIndex,
    /// Ordered live-key set, used by `Range`. The L846 substrate
    /// (will be wrapped in `arc_swap::ArcSwap<Arc<...>>` then),
    /// not a stopgap. Map (not Set) so the watch cache can extend
    /// the value side without a re-typing migration. The
    /// `zero_sized_map_values` clippy lint flags the `()` value
    /// type — silenced here because the type is forward-design
    /// for L859's watch cache, per the L844 plan §4.1.
    /// Populated by writer impls in later commits.
    #[allow(dead_code, clippy::zero_sized_map_values)]
    keys_in_order: parking_lot::RwLock<BTreeMap<Box<[u8]>, ()>>,
    /// Writer serialization. Async-aware mutex because the guard
    /// is held across `commit_batch().await` (`parking_lot` guards
    /// are `!Send`).
    /// Populated by writer impls in later commits.
    #[allow(dead_code)]
    writer: tokio::sync::Mutex<WriterState>,
    /// Highest fully-published revision. Release-stored at end of
    /// every successful commit; Acquire-loaded by `Range` /
    /// [`Self::current_revision`].
    current_main: AtomicI64,
    /// Compacted floor. Release-stored after on-disk delete commit
    /// in `Compact`; Acquire-loaded by `Range`. `0` = none.
    /// Read at construction time, set by `Compact` in a later
    /// commit.
    #[allow(dead_code)]
    compacted: AtomicI64,
}

impl<B: Backend> std::fmt::Debug for MvccStore<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MvccStore")
            .field("current_main", &self.current_main.load(Ordering::Relaxed))
            .field("compacted", &self.compacted.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl<B: Backend> MvccStore<B> {
    /// Open a store against `backend`.
    ///
    /// Registers the MVCC buckets (`key`, `key_index`) if missing,
    /// then probes the `key` bucket for pre-existing data. L844
    /// only supports opening against an empty backend; recovery
    /// lands in L852.
    ///
    /// # Errors
    ///
    /// - [`OpenError::Backend`] if bucket registration or snapshot
    ///   acquisition fails.
    /// - [`OpenError::NonEmptyBackend`] if any keys exist in the
    ///   `key` bucket. `found_revs` is best-effort, capped at
    ///   1024.
    pub fn open(backend: B) -> Result<Self, OpenError> {
        register(&backend)?;

        let snap = backend.snapshot()?;
        let mut iter = snap.range(KEY_BUCKET_ID, &[], NON_EMPTY_PROBE_END)?;
        let mut found: u64 = 0;
        for item in iter.by_ref() {
            // Surface backend-side iteration errors as Backend.
            item?;
            found = found.saturating_add(1);
            if found >= NON_EMPTY_PROBE_CAP {
                break;
            }
        }
        drop(iter);
        drop(snap);
        if found > 0 {
            return Err(OpenError::NonEmptyBackend { found_revs: found });
        }

        // `keys_in_order` carries `BTreeMap<Box<[u8]>, ()>`; clippy's
        // `zero_sized_map_values` flags this constructor too. Same
        // suppression rationale as the field decl: forward-design
        // for L859's watch cache.
        #[allow(clippy::zero_sized_map_values)]
        let keys_in_order = parking_lot::RwLock::new(BTreeMap::new());
        Ok(Self {
            backend,
            index: ShardedKeyIndex::new(),
            keys_in_order,
            writer: tokio::sync::Mutex::new(WriterState::new()),
            current_main: AtomicI64::new(0),
            compacted: AtomicI64::new(0),
        })
    }

    /// Highest fully-published revision. Returns `0` on a fresh
    /// store.
    ///
    /// Acquire-loaded; pairs with the writer's Release-store at
    /// the end of every successful commit.
    #[must_use]
    pub fn current_revision(&self) -> i64 {
        self.current_main.load(Ordering::Acquire)
    }

    /// Borrow the underlying backend. Used by writer impls in
    /// subsequent commits; left `pub(crate)` to avoid leaking the
    /// backend into callers (they passed it in).
    #[allow(dead_code)]
    pub(crate) fn backend(&self) -> &B {
        &self.backend
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

    use super::MvccStore;
    use crate::bucket::{KEY_BUCKET_ID, KEY_INDEX_BUCKET_ID};
    use crate::error::OpenError;
    use mango_storage::{Backend, BackendConfig, InMemBackend};

    /// Compile-time check: `MvccStore<InMemBackend>` is
    /// `Send + Sync`. A future change that breaks this (e.g.
    /// adding a `Cell` field) fails the build at this site.
    const _: () = {
        const fn assert_send<T: Send>() {}
        const fn assert_sync<T: Sync>() {}
        const fn check() {
            assert_send::<MvccStore<InMemBackend>>();
            assert_sync::<MvccStore<InMemBackend>>();
        }
        check();
    };

    fn fresh_backend() -> InMemBackend {
        InMemBackend::open(BackendConfig::new("/unused".into(), false))
            .expect("inmem open never fails")
    }

    #[test]
    fn open_against_fresh_backend_succeeds() {
        let backend = fresh_backend();
        let store = MvccStore::open(backend).expect("fresh open");
        assert_eq!(store.current_revision(), 0);
    }

    #[test]
    fn open_is_idempotent_on_bucket_registration() {
        let backend = fresh_backend();
        // Pre-register the buckets — open() should still succeed
        // because Backend::register_bucket is idempotent.
        backend
            .register_bucket("key", KEY_BUCKET_ID)
            .expect("pre-register key");
        backend
            .register_bucket("key_index", KEY_INDEX_BUCKET_ID)
            .expect("pre-register key_index");
        let store = MvccStore::open(backend).expect("idempotent open");
        assert_eq!(store.current_revision(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_rejects_non_empty_backend() {
        let backend = fresh_backend();
        backend
            .register_bucket("key", KEY_BUCKET_ID)
            .expect("register");
        // Seed one entry into the key bucket.
        let mut batch = backend.begin_batch().expect("begin");
        {
            use mango_storage::WriteBatch as _;
            batch
                .put(KEY_BUCKET_ID, b"\x00\x00\x00\x00\x00\x00\x00\x01", b"v")
                .expect("put");
        }
        let _ = backend.commit_batch(batch, false).await.expect("commit");

        let err = MvccStore::open(backend).expect_err("non-empty rejected");
        match err {
            OpenError::NonEmptyBackend { found_revs } => {
                assert!(found_revs >= 1, "found_revs = {found_revs}");
            }
            other => panic!("expected NonEmptyBackend, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_non_empty_caps_at_1024() {
        let backend = fresh_backend();
        backend
            .register_bucket("key", KEY_BUCKET_ID)
            .expect("register");
        // Seed 2000 entries — cap should clamp the report at 1024.
        let mut batch = backend.begin_batch().expect("begin");
        {
            use mango_storage::WriteBatch as _;
            for i in 0_u64..2000 {
                let key = i.to_be_bytes();
                batch.put(KEY_BUCKET_ID, &key, b"v").expect("put");
            }
        }
        let _ = backend.commit_batch(batch, false).await.expect("commit");

        let err = MvccStore::open(backend).expect_err("non-empty rejected");
        match err {
            OpenError::NonEmptyBackend { found_revs } => {
                assert_eq!(found_revs, 1024, "cap not enforced: {found_revs}");
            }
            other => panic!("expected NonEmptyBackend, got {other:?}"),
        }
    }

    #[test]
    fn current_revision_is_zero_on_fresh_store() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        assert_eq!(store.current_revision(), 0);
        // Multiple loads are stable.
        assert_eq!(store.current_revision(), 0);
    }
}
