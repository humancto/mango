#![cfg(loom)]
// Match the unit-test allow block at the top of `mod tests` in
// src/sharded_key_index.rs. Workspace lints (unwrap_used,
// expect_used, panic, indexing_slicing, arithmetic_side_effects)
// are deny-by-default; integration tests get a local allow because
// they assert against panics and use unwrap to keep the loom model
// bodies tight.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
//! Loom model for `ShardedKeyIndex`. See `docs/loom.md` for the
//! canonical invocation. Properties verified:
//!
//! 1. No torn reads (Models 1, 2).
//! 2. `compact × put` no deadlock + correct final state, both
//!    low-shard-tombstoned/high-shard-put (Model 3) and the inverse
//!    (Model 6).
//! 3. `since × compact` same-shard non-tearing (Model 4).
//! 4. Concurrent readers same-shard parallelise (Model 5).
//! 5. `tombstone × get` same-shard correctness (Model 7).
//! 6. `since × put` same-shard non-tearing (Model 8).

use std::collections::HashSet;

use loom::sync::Arc;
use mango_mvcc::sharded_key_index::{LOOM_KEY_A, LOOM_KEY_B, LOOM_SEED};
use mango_mvcc::{KeyHistoryError, KeyIndexError, Revision, ShardedKeyIndex};

/// Model 1 — `put × put` on two different shards.
///
/// Both threads' puts must be observable post-join under any
/// interleaving. The shards are disjoint so the two write-locks
/// never contend.
#[test]
fn model_1_put_put_two_shards_no_torn_reads() {
    loom::model(|| {
        let idx = Arc::new(ShardedKeyIndex::with_seed(LOOM_SEED));
        let i_a = Arc::clone(&idx);
        let i_b = Arc::clone(&idx);
        let t_a = loom::thread::spawn(move || {
            i_a.put(LOOM_KEY_A, Revision::new(1, 0)).unwrap();
        });
        let t_b = loom::thread::spawn(move || {
            i_b.put(LOOM_KEY_B, Revision::new(2, 0)).unwrap();
        });
        t_a.join().unwrap();
        t_b.join().unwrap();
        let at_a = idx.get(LOOM_KEY_A, 1).unwrap();
        let at_b = idx.get(LOOM_KEY_B, 2).unwrap();
        assert_eq!(at_a.modified, Revision::new(1, 0));
        assert_eq!(at_b.modified, Revision::new(2, 0));
    });
}

/// Model 2 — `put × get` same shard, happens-before via
/// pre-population.
///
/// Pre-populating `LOOM_KEY_A` at `(1,0)` happens-before any spawn,
/// so the racing `get(LOOM_KEY_A, 1)` must see at least `(1,0)`.
/// Asserts `at.modified == (1,0)` because `find_generation(1)`
/// stops at the rev with `main <= 1` even if `(2,0)` has already
/// been written.
#[test]
fn model_2_put_then_get_same_shard_happens_before() {
    loom::model(|| {
        let idx = Arc::new(ShardedKeyIndex::with_seed(LOOM_SEED));
        idx.put(LOOM_KEY_A, Revision::new(1, 0)).unwrap();
        let i_writer = Arc::clone(&idx);
        let i_reader = Arc::clone(&idx);
        let t_w = loom::thread::spawn(move || {
            i_writer.put(LOOM_KEY_A, Revision::new(2, 0)).unwrap();
        });
        let t_r = loom::thread::spawn(move || {
            let at = i_reader.get(LOOM_KEY_A, 1).unwrap();
            assert_eq!(at.modified, Revision::new(1, 0));
        });
        t_w.join().unwrap();
        t_r.join().unwrap();
        let at = idx.get(LOOM_KEY_A, 2).unwrap();
        assert_eq!(at.modified, Revision::new(2, 0));
    });
}

/// Model 3 — `compact × put`, low-shard tombstoned, high-shard
/// put.
///
/// `shard_for(LOOM_KEY_A) < shard_for(LOOM_KEY_B)` per
/// `pre_compute_l841_routing_keys`. With `compact(3)` against
/// `LOOM_KEY_A`'s `[{(1,0),(2,0)}, {empty}]`, gen 0's `last_main=2 <
/// 3`, so `gen_idx` advances to 1 (empty); `walk_desc` returns None;
/// `generations.drain(0..1)` removes gen 0; final state
/// `[{empty}]`, `is_empty()=true`, `compact` returns true; shard's
/// `retain` removes the entry. `available` is empty.
#[test]
fn model_3_compact_concurrent_with_put_low_shard_tombstoned() {
    loom::model(|| {
        let idx = Arc::new(ShardedKeyIndex::with_seed(LOOM_SEED));
        idx.put(LOOM_KEY_A, Revision::new(1, 0)).unwrap();
        idx.tombstone(LOOM_KEY_A, Revision::new(2, 0)).unwrap();
        let i_put = Arc::clone(&idx);
        let i_compact = Arc::clone(&idx);
        let t_put = loom::thread::spawn(move || {
            i_put.put(LOOM_KEY_B, Revision::new(10, 0)).unwrap();
        });
        let t_compact = loom::thread::spawn(move || {
            let mut available: HashSet<Revision> = HashSet::new();
            i_compact.compact(3, &mut available);
            available
        });
        t_put.join().unwrap();
        let available = t_compact.join().unwrap();
        assert!(matches!(
            idx.get(LOOM_KEY_A, 3),
            Err(KeyIndexError::KeyNotFound)
        ));
        let at_b = idx.get(LOOM_KEY_B, 10).unwrap();
        assert_eq!(at_b.modified, Revision::new(10, 0));
        assert!(
            available.is_empty(),
            "available should be empty for fully-retired entry: {available:?}"
        );
    });
}

/// Model 4 — `since × compact` same shard, non-tearing.
///
/// Setup `[(1,0),(2,0),(3,0),(5,0)]` then `compact(3)`:
/// `walk_desc` finds `(3,0)` at idx 2 → `drain(0..2)` → revs
/// become `[(3,0),(5,0)]`. The racing `since(0)` returns the
/// pre-compact suffix or the post-compact suffix — never an
/// interleaved torn slice.
#[test]
fn model_4_since_concurrent_with_compact_same_shard() {
    loom::model(|| {
        let idx = Arc::new(ShardedKeyIndex::with_seed(LOOM_SEED));
        idx.put(LOOM_KEY_A, Revision::new(1, 0)).unwrap();
        idx.put(LOOM_KEY_A, Revision::new(2, 0)).unwrap();
        idx.put(LOOM_KEY_A, Revision::new(3, 0)).unwrap();
        idx.put(LOOM_KEY_A, Revision::new(5, 0)).unwrap();
        let i_since = Arc::clone(&idx);
        let i_compact = Arc::clone(&idx);
        let t_since = loom::thread::spawn(move || {
            let mut buf = Vec::new();
            i_since.since(LOOM_KEY_A, 0, &mut buf);
            buf
        });
        let t_compact = loom::thread::spawn(move || {
            let mut available: HashSet<Revision> = HashSet::new();
            i_compact.compact(3, &mut available);
            available
        });
        let buf = t_since.join().unwrap();
        let available = t_compact.join().unwrap();
        let pre = vec![
            Revision::new(1, 0),
            Revision::new(2, 0),
            Revision::new(3, 0),
            Revision::new(5, 0),
        ];
        let post = vec![Revision::new(3, 0), Revision::new(5, 0)];
        assert!(
            buf == pre || buf == post,
            "torn since: {buf:?} (must be {pre:?} or {post:?})"
        );
        assert!(
            available.contains(&Revision::new(3, 0)),
            "compact should retain the floor rev (3,0): {available:?}"
        );
    });
}

/// Model 5 — interleaved readers same shard.
///
/// Two `get` calls against the same key on the same shard. Asserts
/// both readers observe the pre-state `(1,0)` regardless of how
/// loom interleaves their lock acquisitions.
///
/// Note: this model is functional smoke for the read path — the
/// assertion holds equally under `Mutex`, so it does NOT pin the
/// L840 `RwLock`-vs-`Mutex` design choice. Loom enumerates
/// schedules sequentially; it does not model wall-clock parallelism,
/// so a "two readers in parallel" property is not directly
/// observable here. Per-core read scaling on `RwLock` is verified
/// by Phase 6's bench harness, not by loom.
#[test]
fn model_5_concurrent_readers_same_shard() {
    loom::model(|| {
        let idx = Arc::new(ShardedKeyIndex::with_seed(LOOM_SEED));
        idx.put(LOOM_KEY_A, Revision::new(1, 0)).unwrap();
        let i_a = Arc::clone(&idx);
        let i_b = Arc::clone(&idx);
        let t_a = loom::thread::spawn(move || i_a.get(LOOM_KEY_A, 1).unwrap().modified);
        let t_b = loom::thread::spawn(move || i_b.get(LOOM_KEY_A, 1).unwrap().modified);
        let mod_a = t_a.join().unwrap();
        let mod_b = t_b.join().unwrap();
        assert_eq!(mod_a, Revision::new(1, 0));
        assert_eq!(mod_b, Revision::new(1, 0));
    });
}

/// Model 6 — R1 inverse: `compact × put`, high-shard tombstoned,
/// low-shard put.
///
/// `compact`'s ascending shard walk hits `LOOM_KEY_A`'s (low) shard
/// before `LOOM_KEY_B`'s (high). So a put on `LOOM_KEY_A` interleaves
/// with the compact's walk through the low half, and `LOOM_KEY_B`'s
/// retirement happens later in the same walk. Combined with Model
/// 3, both relative orderings of `put_shard_idx` vs the current
/// position of compact's walk are exercised.
#[test]
fn model_6_compact_concurrent_with_put_high_shard_tombstoned() {
    loom::model(|| {
        let idx = Arc::new(ShardedKeyIndex::with_seed(LOOM_SEED));
        idx.put(LOOM_KEY_B, Revision::new(1, 0)).unwrap();
        idx.tombstone(LOOM_KEY_B, Revision::new(2, 0)).unwrap();
        let i_put = Arc::clone(&idx);
        let i_compact = Arc::clone(&idx);
        let t_put = loom::thread::spawn(move || {
            i_put.put(LOOM_KEY_A, Revision::new(10, 0)).unwrap();
        });
        let t_compact = loom::thread::spawn(move || {
            let mut available: HashSet<Revision> = HashSet::new();
            i_compact.compact(3, &mut available);
            available
        });
        t_put.join().unwrap();
        let available = t_compact.join().unwrap();
        let at_a = idx.get(LOOM_KEY_A, 10).unwrap();
        assert_eq!(at_a.modified, Revision::new(10, 0));
        assert!(matches!(
            idx.get(LOOM_KEY_B, 3),
            Err(KeyIndexError::KeyNotFound)
        ));
        assert!(
            available.is_empty(),
            "available should be empty for fully-retired entry: {available:?}"
        );
    });
}

/// Model 7 — `tombstone × get` same shard.
///
/// The get either observes pre-tombstone state (`Ok` with
/// `modified=(1,0)`) or post-tombstone state
/// (`Err(History(RevisionNotFound))` via `find_generation(2)`'s
/// post-tombstone-gap branch at `key_history.rs:311-317`). Never
/// a torn intermediate state.
#[test]
fn model_7_tombstone_concurrent_with_get_same_shard() {
    loom::model(|| {
        let idx = Arc::new(ShardedKeyIndex::with_seed(LOOM_SEED));
        idx.put(LOOM_KEY_A, Revision::new(1, 0)).unwrap();
        let i_tomb = Arc::clone(&idx);
        let i_get = Arc::clone(&idx);
        let t_tomb = loom::thread::spawn(move || {
            i_tomb.tombstone(LOOM_KEY_A, Revision::new(2, 0)).unwrap();
        });
        let t_get = loom::thread::spawn(move || i_get.get(LOOM_KEY_A, 2));
        t_tomb.join().unwrap();
        let result = t_get.join().unwrap();
        match result {
            Ok(at) => assert_eq!(at.modified, Revision::new(1, 0)),
            Err(KeyIndexError::History(KeyHistoryError::RevisionNotFound)) => {}
            Err(other) => panic!("unexpected error from racing get: {other:?}"),
        }
        // Post-join: the tombstone is durable.
        assert!(matches!(
            idx.get(LOOM_KEY_A, 2),
            Err(KeyIndexError::History(KeyHistoryError::RevisionNotFound))
        ));
    });
}

/// Model 8 — `since × put` same shard.
///
/// The since either observes pre-put state
/// (`[(1,0),(3,0)]`) or post-put state
/// (`[(1,0),(3,0),(5,0)]`). Never a torn slice that omits an
/// earlier rev or duplicates one.
#[test]
fn model_8_since_concurrent_with_put_same_shard() {
    loom::model(|| {
        let idx = Arc::new(ShardedKeyIndex::with_seed(LOOM_SEED));
        idx.put(LOOM_KEY_A, Revision::new(1, 0)).unwrap();
        idx.put(LOOM_KEY_A, Revision::new(3, 0)).unwrap();
        let i_put = Arc::clone(&idx);
        let i_since = Arc::clone(&idx);
        let t_put = loom::thread::spawn(move || {
            i_put.put(LOOM_KEY_A, Revision::new(5, 0)).unwrap();
        });
        let t_since = loom::thread::spawn(move || {
            let mut buf = Vec::new();
            i_since.since(LOOM_KEY_A, 0, &mut buf);
            buf
        });
        t_put.join().unwrap();
        let buf = t_since.join().unwrap();
        let pre = vec![Revision::new(1, 0), Revision::new(3, 0)];
        let post = vec![
            Revision::new(1, 0),
            Revision::new(3, 0),
            Revision::new(5, 0),
        ];
        assert!(
            buf == pre || buf == post,
            "torn since: {buf:?} (must be {pre:?} or {post:?})"
        );
        let at = idx.get(LOOM_KEY_A, 5).unwrap();
        assert_eq!(at.modified, Revision::new(5, 0));
    });
}
