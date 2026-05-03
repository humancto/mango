//! Sharded in-memory `KeyIndex`.
//!
//! 64-shard `[RwLock<HashMap<Box<[u8]>, KeyHistory>>; 64]` with one
//! shared `ahash::RandomState` used for both routing (key → shard)
//! and per-shard `HashMap` hashing. The same `RandomState` is cloned
//! into every shard so routing and in-shard lookup agree.
//!
//! Etcd's equivalent is `treeIndex` in `server/mvcc/index.go` at tag
//! `v3.5.16`: a `*btree.BTree` of `*keyIndex` under one
//! `sync.RWMutex`. Mango's design diverges by:
//!
//! - **Sharded** instead of single-mutex (Phase 6 read-only per-core
//!   bar `>= 14x at 16 cores`).
//! - **`HashMap`** instead of `BTree`, so no in-order range scan on
//!   the index. Range scan is served by the on-disk MVCC bucket (L844
//!   `Range` reads from L846's `arc_swap::ArcSwap<Snapshot>` of the
//!   on-disk bucket). The index is point-lookup only on the watch
//!   and write paths.
//! - **`ahash` per-process seed** (CSPRNG-derived via
//!   `ahash::RandomState::new()`) so we own the seeding policy and
//!   can pin it for tests.
//!
//! # Hashing & sharding contract
//!
//! `ShardedKeyIndex` owns one `ahash::RandomState`. The shard for a
//! key is `(self.hasher.hash_one(key) & 0x3F) as usize`. Every
//! per-shard `HashMap` uses `HashMap::with_hasher(self.hasher.clone())`
//! — the same `RandomState`, cloned. If shards each had their own
//! `RandomState` the routing would still find the right shard, but
//! the per-shard map would compute a different bucket for the same
//! key and `put` / `get` would silently disagree. The
//! `routing_hasher_and_shard_hasher_agree` test pins this.
//!
//! # `Box<[u8]>` keys
//!
//! Keys are stored as `Box<[u8]>` rather than `Bytes`. The map owns
//! the keys; callers pass `&[u8]`. This avoids the per-`put`
//! `Bytes::clone` (atomic refcount) cost. The L859 watch hub will
//! pay one `Bytes::copy_from_slice(&boxed_key)` per emitted event —
//! the deliberate trade for an allocation-quiet hot index path.
//!
//! # Loom-readiness
//!
//! `RwLock` is type-aliased: `loom::sync::RwLock` under
//! `cfg(loom)`, `parking_lot::RwLock` otherwise. Zero behavior
//! change in production builds; unblocks L841's loom tests without a
//! retrofit.
//!
//! # Example
//!
//! ```
//! use mango_mvcc::{KeyIndexError, Revision, ShardedKeyIndex};
//!
//! let idx = ShardedKeyIndex::new();
//! idx.put(b"foo", Revision::new(1, 0))?;
//! let at = idx.get(b"foo", 1)?;
//! assert_eq!(at.modified, Revision::new(1, 0));
//! # Ok::<(), KeyIndexError>(())
//! ```

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};

use ahash::RandomState;

#[cfg(loom)]
use loom::sync::RwLock;
#[cfg(not(loom))]
use parking_lot::RwLock;

use crate::key_history::{KeyAtRev, KeyHistory, KeyHistoryError};
use crate::revision::Revision;

/// Number of shards. Power of 2 so we can use a bit-mask instead of
/// modulo. Cluster-wide consistency requires every node to use the
/// same shard count, so this is `const` rather than configurable.
const SHARD_COUNT: usize = 64;

/// `SHARD_COUNT - 1` as a bit-mask for the routing hash.
const SHARD_MASK: u64 = (SHARD_COUNT as u64) - 1;

/// Per-shard map type. Aliased for `clippy::type_complexity` and to
/// keep the lock helpers readable.
type ShardMap = HashMap<Box<[u8]>, KeyHistory, RandomState>;

/// Shard index, `0..SHARD_COUNT`. Internal — callers do not pick a
/// shard.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub(crate) struct ShardId(u8);

impl ShardId {
    fn as_index(self) -> usize {
        usize::from(self.0)
    }
}

/// Sharded in-memory key → `KeyHistory` map. See module docs for the
/// design.
///
/// Construct with [`ShardedKeyIndex::new`] for production
/// (CSPRNG-seeded `RandomState`). All operations take `&self` and
/// internally lock the relevant shard. No call ever holds two shard
/// locks at once.
pub struct ShardedKeyIndex {
    /// Routing hasher AND per-shard hasher source. Cloned into each
    /// shard's `HashMap`. See module docs.
    hasher: RandomState,

    /// `[RwLock<HashMap>; 64]`. `Vec` rather than `[_; 64]` because
    /// `HashMap` is not `Default` for the `with_hasher` build —
    /// each entry is constructed with the cloned hasher, then
    /// pushed.
    shards: Vec<RwLock<ShardMap>>,
}

impl ShardedKeyIndex {
    /// Production constructor. Seeds the routing hasher from the OS
    /// CSPRNG via `ahash::RandomState::new()` (which uses ahash's
    /// `runtime-rng` default feature internally).
    #[must_use]
    pub fn new() -> Self {
        Self::from_hasher(RandomState::new())
    }

    /// Test-only constructor accepting a fixed 32-byte seed. The
    /// seed is split into four `u64`s (little-endian) and fed to
    /// `RandomState::with_seeds`. Used by L842's hostile-key `DoS`
    /// test which fixes the seed to construct a known-colliding key
    /// set, then re-seeds and confirms the collision is defeated.
    ///
    /// Gated `cfg(any(test, feature = "test-seed"))` — `cfg(test)`
    /// covers unit tests in this crate; the `test-seed` feature
    /// covers integration tests in other crates (which cannot see
    /// `cfg(test)` items).
    #[cfg(any(test, feature = "test-seed"))]
    #[must_use]
    pub fn with_seed(seed: [u8; 32]) -> Self {
        let s0 = u64::from_le_bytes(seed[0..8].try_into().unwrap_or([0; 8]));
        let s1 = u64::from_le_bytes(seed[8..16].try_into().unwrap_or([0; 8]));
        let s2 = u64::from_le_bytes(seed[16..24].try_into().unwrap_or([0; 8]));
        let s3 = u64::from_le_bytes(seed[24..32].try_into().unwrap_or([0; 8]));
        Self::from_hasher(RandomState::with_seeds(s0, s1, s2, s3))
    }

    fn from_hasher(hasher: RandomState) -> Self {
        let mut shards = Vec::with_capacity(SHARD_COUNT);
        for _ in 0..SHARD_COUNT {
            shards.push(RwLock::new(HashMap::with_hasher(hasher.clone())));
        }
        Self { hasher, shards }
    }

    /// Insert or extend the `KeyHistory` for `key` with `rev`.
    ///
    /// If `key` has no entry yet, a fresh `KeyHistory::new()` is
    /// created and `put` is called on it. Otherwise the existing
    /// entry's `put` is called.
    ///
    /// # Errors
    ///
    /// Forwards [`KeyHistoryError`] from the inner [`KeyHistory::put`]
    /// (currently only [`KeyHistoryError::NonMonotonic`]).
    pub fn put(&self, key: &[u8], rev: Revision) -> Result<(), KeyIndexError> {
        let shard = self.shard_for(key);
        let lock = self.shard(shard);
        let mut guard = write_lock(lock);
        let entry = guard.entry(key.into()).or_default();
        entry.put(rev)?;
        Ok(())
    }

    /// Tombstone the `KeyHistory` for `key` at `rev`.
    ///
    /// # Errors
    ///
    /// - [`KeyIndexError::KeyNotFound`] if `key` has no entry.
    /// - Forwards [`KeyHistoryError`] from the inner
    ///   [`KeyHistory::tombstone`].
    pub fn tombstone(&self, key: &[u8], rev: Revision) -> Result<(), KeyIndexError> {
        let shard = self.shard_for(key);
        let lock = self.shard(shard);
        let mut guard = write_lock(lock);
        let Some(entry) = guard.get_mut(key) else {
            return Err(KeyIndexError::KeyNotFound);
        };
        entry.tombstone(rev)?;
        Ok(())
    }

    /// Fetch the version visible at `at_rev`.
    ///
    /// # Errors
    ///
    /// - [`KeyIndexError::KeyNotFound`] if `key` has no entry.
    /// - Forwards [`KeyHistoryError`] from [`KeyHistory::get`]
    ///   (notably [`KeyHistoryError::RevisionNotFound`] if the entry
    ///   exists but no rev is visible at `at_rev`).
    pub fn get(&self, key: &[u8], at_rev: i64) -> Result<KeyAtRev, KeyIndexError> {
        let shard = self.shard_for(key);
        let lock = self.shard(shard);
        let guard = read_lock(lock);
        let Some(entry) = guard.get(key) else {
            return Err(KeyIndexError::KeyNotFound);
        };
        let at = entry.get(at_rev)?;
        Ok(at)
    }

    /// Append revisions of `key` since `since_rev` into `out`.
    ///
    /// Returns silently with no writes to `out` if `key` has no
    /// entry — the watch hub treats both "no key" and "no revs since"
    /// the same. See [`KeyHistory::since_into`] for the dedup-by-main
    /// semantics.
    pub fn since(&self, key: &[u8], since_rev: i64, out: &mut Vec<Revision>) {
        let shard = self.shard_for(key);
        let lock = self.shard(shard);
        let guard = read_lock(lock);
        if let Some(entry) = guard.get(key) {
            entry.since_into(since_rev, out);
        }
    }

    /// Compact every entry in the index at `at_rev`. Drops entries
    /// whose [`KeyHistory::compact`] returns `true`.
    ///
    /// Visits shards in ascending `ShardId` order, taking each
    /// shard's `write` lock in turn — never holds two shard locks
    /// simultaneously.
    ///
    /// **Caller contract:** `at_rev` MUST be the agreed compaction
    /// watermark, monotonically non-decreasing across calls.
    /// Concurrent `put` ops with `rev > at_rev` are safe regardless
    /// of shard visit order — the new rev is by contract greater
    /// than the watermark, so it is not a candidate for this pass.
    pub fn compact<S: BuildHasher>(&self, at_rev: i64, available: &mut HashSet<Revision, S>) {
        for shard_idx in 0..SHARD_COUNT {
            let Some(lock) = self.shards.get(shard_idx) else {
                unreachable!("SHARD_COUNT shards constructed in new()");
            };
            let mut guard = write_lock(lock);
            guard.retain(|_key, entry| !entry.compact(at_rev, available));
        }
    }

    /// Plan-only variant of [`ShardedKeyIndex::compact`]: populates
    /// `available` with what compaction would retain, but does not
    /// mutate the index.
    pub fn keep<S: BuildHasher>(&self, at_rev: i64, available: &mut HashSet<Revision, S>) {
        for shard_idx in 0..SHARD_COUNT {
            let Some(lock) = self.shards.get(shard_idx) else {
                unreachable!("SHARD_COUNT shards constructed in new()");
            };
            let guard = read_lock(lock);
            for entry in guard.values() {
                entry.keep(at_rev, available);
            }
        }
    }

    /// Reconstruct a single key's `KeyHistory` from on-disk state.
    /// For L852's recovery loop.
    ///
    /// # Errors
    ///
    /// - [`KeyIndexError::AlreadyExists`] if `key` already has an
    ///   entry. Recovery is idempotent only on an empty container.
    /// - Forwards [`KeyHistoryError`] from [`KeyHistory::restore`]
    ///   (input validation).
    pub fn restore(
        &self,
        key: &[u8],
        created: Revision,
        modified: Revision,
        ver: i64,
    ) -> Result<(), KeyIndexError> {
        let shard = self.shard_for(key);
        let lock = self.shard(shard);
        let mut guard = write_lock(lock);
        if guard.contains_key(key) {
            return Err(KeyIndexError::AlreadyExists);
        }
        let history = KeyHistory::restore(created, modified, ver)?;
        guard.insert(key.into(), history);
        Ok(())
    }

    /// Sum of per-shard entry counts.
    ///
    /// **Approximate under concurrent writes.** Each shard is
    /// individually consistent; the sum across shards is not
    /// snapshot-consistent. Test/diagnostic only.
    #[must_use]
    pub fn len(&self) -> usize {
        let mut total = 0_usize;
        for shard_idx in 0..SHARD_COUNT {
            let Some(lock) = self.shards.get(shard_idx) else {
                unreachable!("SHARD_COUNT shards constructed in new()");
            };
            let guard = read_lock(lock);
            total = total.saturating_add(guard.len());
        }
        total
    }

    /// `true` iff every shard is empty. Walks shards until the first
    /// non-empty; not snapshot-consistent across shards.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        for shard_idx in 0..SHARD_COUNT {
            let Some(lock) = self.shards.get(shard_idx) else {
                unreachable!("SHARD_COUNT shards constructed in new()");
            };
            let guard = read_lock(lock);
            if !guard.is_empty() {
                return false;
            }
        }
        true
    }

    /// Compute the routing shard for `key`.
    pub(crate) fn shard_for(&self, key: &[u8]) -> ShardId {
        let mut h = self.hasher.build_hasher();
        h.write(key);
        let masked = h.finish() & SHARD_MASK;
        let idx = u8::try_from(masked).unwrap_or_else(|_| {
            unreachable!("masked hash <= SHARD_MASK = 0x3F, fits u8");
        });
        ShardId(idx)
    }

    fn shard(&self, id: ShardId) -> &RwLock<ShardMap> {
        let idx = id.as_index();
        self.shards.get(idx).unwrap_or_else(|| {
            unreachable!("ShardId({idx}) out of range — invariant violation");
        })
    }
}

impl Default for ShardedKeyIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Acquire a `read` guard. Bridges `parking_lot::RwLock::read` (no
/// `Result`) and `loom::sync::RwLock::read` (returns `Result` from
/// poisoning). Mango disables poisoning by using `parking_lot` in
/// production; under `cfg(loom)` we panic on poisoning since loom
/// would have already failed the test.
#[cfg(not(loom))]
fn read_lock<T>(lock: &RwLock<T>) -> parking_lot::RwLockReadGuard<'_, T> {
    lock.read()
}

#[cfg(not(loom))]
fn write_lock<T>(lock: &RwLock<T>) -> parking_lot::RwLockWriteGuard<'_, T> {
    lock.write()
}

#[cfg(loom)]
fn read_lock<T>(lock: &RwLock<T>) -> loom::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|_| {
        unreachable!("loom RwLock poisoned — earlier panic should have failed the test");
    })
}

#[cfg(loom)]
fn write_lock<T>(lock: &RwLock<T>) -> loom::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|_| {
        unreachable!("loom RwLock poisoned — earlier panic should have failed the test");
    })
}

/// Errors returned by [`ShardedKeyIndex`] operations.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KeyIndexError {
    /// `key` has no entry in the index. Distinct from the inner
    /// `RevisionNotFound` (entry exists, no rev visible at the
    /// requested `at_rev`).
    #[error("key not found in index")]
    KeyNotFound,

    /// `restore` called on a key that already has an entry.
    /// Recovery is idempotent only on an empty container.
    #[error("restore on existing key")]
    AlreadyExists,

    /// Forwarded from the per-key [`KeyHistory`] operation.
    #[error(transparent)]
    History(#[from] KeyHistoryError),
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

    fn fixed_seed() -> [u8; 32] {
        let mut s = [0_u8; 32];
        for (i, b) in s.iter_mut().enumerate() {
            *b = u8::try_from(i).unwrap();
        }
        s
    }

    fn other_seed() -> [u8; 32] {
        let mut s = [0_u8; 32];
        for (i, b) in s.iter_mut().enumerate() {
            *b = u8::try_from(255 - i).unwrap();
        }
        s
    }

    #[test]
    fn new_is_empty() {
        let idx = ShardedKeyIndex::new();
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());
    }

    #[test]
    fn default_matches_new() {
        let idx: ShardedKeyIndex = ShardedKeyIndex::default();
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn put_then_get_round_trip() {
        let idx = ShardedKeyIndex::new();
        idx.put(b"foo", Revision::new(1, 0)).unwrap();
        let at = idx.get(b"foo", 1).unwrap();
        assert_eq!(at.modified, Revision::new(1, 0));
        assert_eq!(at.created, Revision::new(1, 0));
        assert_eq!(at.version, 1);
    }

    #[test]
    fn put_create_implicit_history() {
        let idx = ShardedKeyIndex::new();
        // Never-seen key: put should create the entry implicitly.
        idx.put(b"new-key", Revision::new(5, 0)).unwrap();
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn put_existing_key_extends_history() {
        let idx = ShardedKeyIndex::new();
        idx.put(b"k", Revision::new(1, 0)).unwrap();
        idx.put(b"k", Revision::new(2, 0)).unwrap();
        idx.put(b"k", Revision::new(3, 0)).unwrap();
        let at = idx.get(b"k", 3).unwrap();
        assert_eq!(at.version, 3);
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn put_non_monotonic_returns_history_error() {
        let idx = ShardedKeyIndex::new();
        idx.put(b"k", Revision::new(2, 0)).unwrap();
        let err = idx.put(b"k", Revision::new(1, 0)).unwrap_err();
        assert!(matches!(
            err,
            KeyIndexError::History(KeyHistoryError::NonMonotonic { .. })
        ));
    }

    #[test]
    fn get_on_missing_key_returns_key_not_found() {
        let idx = ShardedKeyIndex::new();
        let err = idx.get(b"absent", 1).unwrap_err();
        assert!(matches!(err, KeyIndexError::KeyNotFound));
    }

    #[test]
    fn get_on_present_key_after_tombstone_returns_history_error() {
        let idx = ShardedKeyIndex::new();
        idx.put(b"k", Revision::new(1, 0)).unwrap();
        idx.tombstone(b"k", Revision::new(2, 0)).unwrap();
        // (2,0) is the tombstone itself — not visible.
        let err = idx.get(b"k", 2).unwrap_err();
        assert!(matches!(
            err,
            KeyIndexError::History(KeyHistoryError::RevisionNotFound)
        ));
    }

    #[test]
    fn tombstone_on_missing_key_returns_err() {
        let idx = ShardedKeyIndex::new();
        let err = idx.tombstone(b"absent", Revision::new(1, 0)).unwrap_err();
        assert!(matches!(err, KeyIndexError::KeyNotFound));
    }

    #[test]
    fn tombstone_then_get_at_tombstone_returns_history_err() {
        let idx = ShardedKeyIndex::new();
        idx.put(b"k", Revision::new(1, 0)).unwrap();
        idx.tombstone(b"k", Revision::new(2, 0)).unwrap();
        let err = idx.get(b"k", 2).unwrap_err();
        assert!(matches!(
            err,
            KeyIndexError::History(KeyHistoryError::RevisionNotFound)
        ));
    }

    #[test]
    fn empty_key_is_valid() {
        let idx = ShardedKeyIndex::new();
        idx.put(b"", Revision::new(1, 0)).unwrap();
        let at = idx.get(b"", 1).unwrap();
        assert_eq!(at.modified, Revision::new(1, 0));
    }

    #[test]
    fn since_returns_silently_on_missing_key() {
        let idx = ShardedKeyIndex::new();
        let mut out: Vec<Revision> = Vec::new();
        idx.since(b"absent", 0, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn since_returns_revs_for_present_key() {
        let idx = ShardedKeyIndex::new();
        idx.put(b"k", Revision::new(1, 0)).unwrap();
        idx.put(b"k", Revision::new(2, 0)).unwrap();
        let mut out: Vec<Revision> = Vec::new();
        idx.since(b"k", 0, &mut out);
        assert_eq!(out, vec![Revision::new(1, 0), Revision::new(2, 0)]);
    }

    #[test]
    fn compact_drops_empty_entries() {
        let idx = ShardedKeyIndex::new();
        idx.put(b"k", Revision::new(1, 0)).unwrap();
        idx.tombstone(b"k", Revision::new(2, 0)).unwrap();
        assert_eq!(idx.len(), 1);

        let mut available: HashSet<Revision> = HashSet::new();
        idx.compact(10, &mut available);
        // After compact past the tombstone, KeyHistory::compact returns
        // true and the entry is removed.
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());
    }

    #[test]
    fn compact_preserves_live_entries() {
        let idx = ShardedKeyIndex::new();
        idx.put(b"alive", Revision::new(1, 0)).unwrap();
        idx.put(b"alive", Revision::new(2, 0)).unwrap();
        idx.put(b"alive", Revision::new(3, 0)).unwrap();

        let mut available: HashSet<Revision> = HashSet::new();
        idx.compact(2, &mut available);
        // Entry is still alive — gen still has revs > 2.
        assert_eq!(idx.len(), 1);
        let at = idx.get(b"alive", 3).unwrap();
        assert_eq!(at.modified, Revision::new(3, 0));
    }

    #[test]
    fn keep_does_not_mutate_index() {
        let idx = ShardedKeyIndex::new();
        idx.put(b"k", Revision::new(1, 0)).unwrap();
        idx.tombstone(b"k", Revision::new(2, 0)).unwrap();
        assert_eq!(idx.len(), 1);

        let mut available: HashSet<Revision> = HashSet::new();
        idx.keep(10, &mut available);
        assert_eq!(idx.len(), 1, "keep must not drop entries");
    }

    #[test]
    fn restore_constructs_entry() {
        let idx = ShardedKeyIndex::new();
        idx.restore(b"k", Revision::new(1, 0), Revision::new(7, 2), 3)
            .unwrap();
        let at = idx.get(b"k", 7).unwrap();
        assert_eq!(at.modified, Revision::new(7, 2));
        assert_eq!(at.created, Revision::new(1, 0));
        assert_eq!(at.version, 3);
    }

    #[test]
    fn restore_rejects_existing_key() {
        let idx = ShardedKeyIndex::new();
        idx.put(b"k", Revision::new(1, 0)).unwrap();
        let err = idx
            .restore(b"k", Revision::new(1, 0), Revision::new(2, 0), 2)
            .unwrap_err();
        assert!(matches!(err, KeyIndexError::AlreadyExists));
    }

    #[test]
    fn restore_invalid_input_forwards_history_error() {
        let idx = ShardedKeyIndex::new();
        let err = idx
            .restore(b"k", Revision::new(-1, 0), Revision::new(0, 0), 1)
            .unwrap_err();
        assert!(matches!(
            err,
            KeyIndexError::History(KeyHistoryError::RestoreInvalid { .. })
        ));
    }

    #[test]
    fn len_sums_across_shards() {
        let idx = ShardedKeyIndex::new();
        for i in 0..10_u32 {
            let key = format!("k{i}");
            idx.put(key.as_bytes(), Revision::new(i64::from(i) + 1, 0))
                .unwrap();
        }
        assert_eq!(idx.len(), 10);
    }

    // === Class 2: sharding contract ===

    #[test]
    fn same_seed_routes_same_shard() {
        let a = ShardedKeyIndex::with_seed(fixed_seed());
        let b = ShardedKeyIndex::with_seed(fixed_seed());
        for i in 0..1000_u32 {
            let key = format!("k{i}");
            assert_eq!(
                a.shard_for(key.as_bytes()),
                b.shard_for(key.as_bytes()),
                "same seed must route same key to same shard"
            );
        }
    }

    #[test]
    fn different_seeds_distribute_differently() {
        let a = ShardedKeyIndex::with_seed(fixed_seed());
        let b = ShardedKeyIndex::with_seed(other_seed());
        let mut differ = 0_u32;
        for i in 0..1000_u32 {
            let key = format!("k{i}");
            if a.shard_for(key.as_bytes()) != b.shard_for(key.as_bytes()) {
                differ += 1;
            }
        }
        // Two random hashers should disagree on > 1% of 1000 keys
        // (vastly under the < 1/64^1000 identical-assignment bound).
        assert!(
            differ > 10,
            "expected >10 keys to differ across seeds, got {differ}"
        );
    }

    #[test]
    fn production_new_distributes_keys() {
        let idx = ShardedKeyIndex::new();
        let mut counts = [0_u32; SHARD_COUNT];
        for i in 0..1000_u32 {
            let key = format!("k{i}");
            let shard = idx.shard_for(key.as_bytes()).as_index();
            counts[shard] += 1;
        }
        let mean = 1000_u32 / u32::try_from(SHARD_COUNT).unwrap(); // ~15
        let max = counts.iter().copied().max().unwrap();
        assert!(
            max <= mean * 3,
            "shard imbalance: max {max} vs mean {mean} (counts: {counts:?})"
        );
    }

    /// Regression for the routing-vs-per-shard-hasher contract: if
    /// the per-shard `HashMap` had its own `RandomState`, `put` and
    /// `get` would land in different `HashMap` buckets and lookups
    /// would silently fail. 10K random keys round-tripping pins
    /// this.
    #[test]
    fn routing_hasher_and_shard_hasher_agree() {
        let idx = ShardedKeyIndex::new();
        let mut keys: Vec<String> = Vec::with_capacity(10_000);
        for i in 0..10_000_u32 {
            keys.push(format!(
                "k-{i:08x}-suffix-{}",
                i.wrapping_mul(2_654_435_761)
            ));
        }
        for (i, k) in keys.iter().enumerate() {
            idx.put(
                k.as_bytes(),
                Revision::new(i64::try_from(i + 1).unwrap(), 0),
            )
            .unwrap();
        }
        assert_eq!(idx.len(), keys.len());
        for (i, k) in keys.iter().enumerate() {
            let at = idx
                .get(k.as_bytes(), i64::try_from(i + 1).unwrap())
                .unwrap_or_else(|e| {
                    panic!("get({k:?}) failed: {e:?} — routing/shard hasher disagree?")
                });
            assert_eq!(at.version, 1);
        }
    }
}
