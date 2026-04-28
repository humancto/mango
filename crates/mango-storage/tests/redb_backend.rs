//! Integration tests for [`mango_storage::RedbBackend`] (ROADMAP:817).
//!
//! Every test opens a real `redb` database in a `tempfile::TempDir` —
//! the `TempDir` drops at the end of the test, cleaning up the file.
//! Tests are organized by behavior surface:
//!
//! - lifecycle: open / close / reopen / closed-errors
//! - registry: `register_bucket` idempotence and conflict paths
//! - write path: put / delete / `delete_range` round-trips
//! - read path: snapshot isolation, unknown-bucket, invalid-range
//! - commit semantics: group atomicity, stamp monotonicity
//! - utility: `size_on_disk`, defragment, read-only open
//! - persistence: registry survives reopen
//! - cross-cutting: mini-oracle against `BTreeMap<Vec<u8>, Vec<u8>>`
//!
//! The tests deliberately exercise the `Backend` trait through the
//! `mango_storage::Backend` import, so the trait contract is what is
//! verified — not just the impl.
//!
//! Under `--cfg madsim` this file is excluded — madsim-tokio does not
//! expose the multi-thread runtime we use in the concurrency test, and
//! exercising redb's real mmap+fsync under the simulator's virtual
//! time would be a category error. The dedicated madsim smoke test
//! lives in `madsim_backend_smoke.rs`.

#![cfg(not(madsim))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use mango_storage::{
    Backend, BackendConfig, BackendError, BucketId, CommitStamp, ReadSnapshot, RedbBackend,
    WriteBatch,
};
use tempfile::TempDir;

const KV: BucketId = BucketId::new(1);
const META: BucketId = BucketId::new(2);

fn open(dir: &TempDir) -> RedbBackend {
    RedbBackend::open(BackendConfig::new(dir.path().to_path_buf(), false)).expect("open")
}

async fn put(b: &RedbBackend, bucket: BucketId, k: &[u8], v: &[u8]) {
    let mut batch = b.begin_batch().expect("begin_batch");
    batch.put(bucket, k, v).expect("put");
    let _ = b.commit_batch(batch, true).await.expect("commit");
}

fn get(b: &RedbBackend, bucket: BucketId, k: &[u8]) -> Option<Vec<u8>> {
    let snap = b.snapshot().expect("snapshot");
    snap.get(bucket, k)
        .expect("get")
        .map(|bytes| bytes.to_vec())
}

// ---------- lifecycle ------------------------------------------------

#[test]
fn open_creates_data_dir_if_missing() {
    let tmp = TempDir::new().unwrap();
    let nested = tmp.path().join("sub/a/b");
    assert!(!nested.exists());
    let b = RedbBackend::open(BackendConfig::new(nested.clone(), false)).unwrap();
    assert!(nested.is_dir());
    b.close().unwrap();
}

#[test]
fn close_idempotent_then_closed_errors() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.close().unwrap();
    b.close().unwrap();
    b.close().unwrap();
    assert!(matches!(
        b.register_bucket("kv", KV),
        Err(BackendError::Closed)
    ));
    assert!(matches!(b.snapshot(), Err(BackendError::Closed)));
    assert!(matches!(b.begin_batch(), Err(BackendError::Closed)));
}

#[tokio::test]
async fn reopen_after_close_sees_prior_writes() {
    let tmp = TempDir::new().unwrap();
    {
        let b = open(&tmp);
        b.register_bucket("kv", KV).unwrap();
        put(&b, KV, b"alpha", b"1").await;
        b.close().unwrap();
    }
    let b2 = open(&tmp);
    assert_eq!(get(&b2, KV, b"alpha").as_deref(), Some(&b"1"[..]));
}

#[test]
fn read_only_open_returns_other() {
    let tmp = TempDir::new().unwrap();
    let res = RedbBackend::open(BackendConfig::new(tmp.path().to_path_buf(), true));
    match res {
        Err(BackendError::Other(msg)) => {
            assert!(
                msg.contains("read-only"),
                "unexpected Other message: {msg:?}"
            );
        }
        Ok(_) => panic!("expected Other(read-only ...)"),
        Err(other) => panic!("expected Other(read-only ...), got {other:?}"),
    }
}

// ---------- registry -------------------------------------------------

#[test]
fn register_bucket_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    b.register_bucket("kv", KV).unwrap();
    b.register_bucket("kv", KV).unwrap();
}

#[test]
fn register_bucket_conflict_rebinding_id() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    match b.register_bucket("meta", KV) {
        Err(BackendError::BucketConflict {
            id,
            existing,
            requested,
        }) => {
            assert_eq!(id, KV);
            assert_eq!(existing, "kv");
            assert_eq!(requested, "meta");
        }
        other => panic!("expected BucketConflict, got {other:?}"),
    }
}

#[test]
fn register_bucket_conflict_rebinding_name() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    match b.register_bucket("kv", META) {
        Err(BackendError::BucketNameConflict {
            name,
            existing_id,
            requested_id,
        }) => {
            assert_eq!(name, "kv");
            assert_eq!(existing_id, KV);
            assert_eq!(requested_id, META);
        }
        other => panic!("expected BucketNameConflict, got {other:?}"),
    }
}

#[test]
fn registry_persists_across_reopen() {
    // Both conflict variants must fire after hydration, proving the
    // on-disk registry was read back. `BucketConflict` (id-rebind) is
    // checked before `BucketNameConflict` in the registry, so to fire
    // the name-conflict path we pick a fresh id (`99`) whose id-slot is
    // unused and exercise only the name collision.
    const FRESH_ID: BucketId = BucketId::new(99);
    let tmp = TempDir::new().unwrap();
    {
        let b = open(&tmp);
        b.register_bucket("kv", KV).unwrap();
        b.register_bucket("meta", META).unwrap();
        b.close().unwrap();
    }
    let b2 = open(&tmp);
    assert!(matches!(
        b2.register_bucket("kv", FRESH_ID),
        Err(BackendError::BucketNameConflict { .. })
    ));
    assert!(matches!(
        b2.register_bucket("kv2", KV),
        Err(BackendError::BucketConflict { .. })
    ));
    b2.register_bucket("kv", KV).unwrap();
}

// ---------- write path -----------------------------------------------

#[tokio::test]
async fn put_get_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    put(&b, KV, b"k", b"v").await;
    assert_eq!(get(&b, KV, b"k").as_deref(), Some(&b"v"[..]));
    assert_eq!(get(&b, KV, b"absent"), None);
}

#[tokio::test]
async fn delete_removes_key() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    put(&b, KV, b"k", b"v").await;
    assert_eq!(get(&b, KV, b"k").as_deref(), Some(&b"v"[..]));

    let mut batch = b.begin_batch().unwrap();
    batch.delete(KV, b"k").unwrap();
    let _ = b.commit_batch(batch, true).await.unwrap();
    assert_eq!(get(&b, KV, b"k"), None);
}

#[tokio::test]
async fn delete_range_removes_half_open_interval() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    for k in [b"a", b"b", b"c", b"d", b"e"] {
        put(&b, KV, k, b"v").await;
    }

    let mut batch = b.begin_batch().unwrap();
    batch.delete_range(KV, b"b", b"d").unwrap();
    let _ = b.commit_batch(batch, true).await.unwrap();

    assert_eq!(get(&b, KV, b"a").as_deref(), Some(&b"v"[..]));
    assert_eq!(get(&b, KV, b"b"), None);
    assert_eq!(get(&b, KV, b"c"), None);
    // `d` is the exclusive upper bound — survives.
    assert_eq!(get(&b, KV, b"d").as_deref(), Some(&b"v"[..]));
    assert_eq!(get(&b, KV, b"e").as_deref(), Some(&b"v"[..]));
}

#[tokio::test]
async fn delete_range_with_empty_end_deletes_to_max() {
    // `end = []` means "unbounded upper" per the engine-neutral
    // DeleteRange contract (mirrors bbolt's `len(end) == 0`
    // semantics). Regression test for the wrapper fix: prior to this
    // commit, `retain_in(b"b"..b"", _)` was the empty range on redb
    // and deleted nothing, while bbolt deleted b..∞. That silent
    // divergence surfaces the moment the differential harness's
    // proptest strategy draws `end == b""`.
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    for k in [b"a", b"b", b"c", b"d", b"e"] {
        put(&b, KV, k, b"v").await;
    }

    let mut batch = b.begin_batch().unwrap();
    batch.delete_range(KV, b"b", b"").unwrap();
    let _ = b.commit_batch(batch, true).await.unwrap();

    assert_eq!(get(&b, KV, b"a").as_deref(), Some(&b"v"[..]));
    assert_eq!(get(&b, KV, b"b"), None);
    assert_eq!(get(&b, KV, b"c"), None);
    assert_eq!(get(&b, KV, b"d"), None);
    assert_eq!(get(&b, KV, b"e"), None);
}

#[tokio::test]
async fn delete_range_with_empty_start_and_empty_end_deletes_all() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    for k in [b"a", b"b", b"c"] {
        put(&b, KV, k, b"v").await;
    }

    let mut batch = b.begin_batch().unwrap();
    batch.delete_range(KV, b"", b"").unwrap();
    let _ = b.commit_batch(batch, true).await.unwrap();

    assert_eq!(get(&b, KV, b"a"), None);
    assert_eq!(get(&b, KV, b"b"), None);
    assert_eq!(get(&b, KV, b"c"), None);
}

#[tokio::test]
async fn delete_range_allows_empty_end_with_any_start() {
    // `validate_ops` rejects `start > end` only when `end` is
    // non-empty. This tests the skip-path: start = "z" (very high),
    // end = "" (unbounded upper) must NOT be rejected as
    // `InvalidRange("start > end")`.
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    for k in [b"a", b"y", b"z"] {
        put(&b, KV, k, b"v").await;
    }

    let mut batch = b.begin_batch().unwrap();
    batch.delete_range(KV, b"z", b"").unwrap();
    // Commit MUST succeed — must not error as InvalidRange.
    let _ = b.commit_batch(batch, true).await.unwrap();

    assert_eq!(get(&b, KV, b"a").as_deref(), Some(&b"v"[..]));
    assert_eq!(get(&b, KV, b"y").as_deref(), Some(&b"v"[..]));
    assert_eq!(get(&b, KV, b"z"), None);
}

#[tokio::test]
async fn unknown_bucket_on_put_errors_from_commit() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    // No register_bucket — KV is unknown.
    let mut batch = b.begin_batch().unwrap();
    batch.put(KV, b"k", b"v").unwrap();
    match b.commit_batch(batch, true).await {
        Err(BackendError::UnknownBucket(id)) => assert_eq!(id, KV),
        other => panic!("expected UnknownBucket, got {other:?}"),
    }
}

#[tokio::test]
async fn invalid_range_on_delete_range_errors_from_commit() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    let mut batch = b.begin_batch().unwrap();
    batch.delete_range(KV, b"z", b"a").unwrap();
    match b.commit_batch(batch, true).await {
        Err(BackendError::InvalidRange(_)) => {}
        other => panic!("expected InvalidRange, got {other:?}"),
    }
}

// ---------- read path ------------------------------------------------

#[tokio::test]
async fn snapshot_isolation_observes_pre_commit_state() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    put(&b, KV, b"k", b"old").await;

    let snap = b.snapshot().unwrap();
    put(&b, KV, b"k", b"new").await;

    assert_eq!(get(&b, KV, b"k").as_deref(), Some(&b"new"[..]));
    assert_eq!(
        snap.get(KV, b"k").unwrap().map(|b| b.to_vec()).as_deref(),
        Some(&b"old"[..])
    );
}

#[test]
fn snapshot_range_on_unregistered_bucket_errors() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    let snap = b.snapshot().unwrap();
    // `Result<Box<dyn RangeIter>, BackendError>` does not implement
    // Debug (the Ok variant does not), so the catch-all binds the
    // error only after splitting Ok first. Semicolon forces temporaries
    // to drop before `snap`, which the borrow of `snap.range(...)`
    // otherwise keeps live past end-of-scope.
    match snap.range(KV, b"a", b"z") {
        Ok(_) => panic!("expected UnknownBucket"),
        Err(BackendError::UnknownBucket(id)) => assert_eq!(id, KV),
        Err(other) => panic!("expected UnknownBucket, got {other:?}"),
    };
}

#[test]
fn snapshot_range_invalid_range_errors() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    let snap = b.snapshot().unwrap();
    match snap.range(KV, b"z", b"a") {
        Ok(_) => panic!("expected InvalidRange"),
        Err(BackendError::InvalidRange(_)) => {}
        Err(other) => panic!("expected InvalidRange, got {other:?}"),
    };
}

#[tokio::test]
async fn snapshot_range_returns_half_open_interval() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    for (k, v) in [
        (b"a".as_slice(), b"1".as_slice()),
        (b"b", b"2"),
        (b"c", b"3"),
        (b"d", b"4"),
    ] {
        put(&b, KV, k, v).await;
    }
    let snap = b.snapshot().unwrap();
    let iter = snap.range(KV, b"b", b"d").unwrap();
    let got: Vec<(Vec<u8>, Vec<u8>)> = iter
        .map(|r| {
            let (k, v) = r.unwrap();
            (k.to_vec(), v.to_vec())
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (b"b".to_vec(), b"2".to_vec()),
            (b"c".to_vec(), b"3".to_vec()),
        ]
    );
}

// ---------- commit semantics -----------------------------------------

#[tokio::test]
async fn commit_stamp_is_monotonic_across_commits() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();

    let mut prior = None::<mango_storage::CommitStamp>;
    for i in 0u8..5 {
        let mut batch = b.begin_batch().unwrap();
        batch.put(KV, &[i], b"v").unwrap();
        let stamp = b.commit_batch(batch, true).await.unwrap();
        if let Some(p) = prior {
            assert!(p < stamp, "stamp did not advance: {p:?} -> {stamp:?}");
        }
        prior = Some(stamp);
    }
}

#[tokio::test]
async fn commit_group_atomic_applies_every_batch() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();

    let mut a = b.begin_batch().unwrap();
    a.put(KV, b"a", b"1").unwrap();
    let mut c = b.begin_batch().unwrap();
    c.put(KV, b"b", b"2").unwrap();
    c.delete(KV, b"a").unwrap();

    let _ = b.commit_group(vec![a, c]).await.unwrap();

    // `a`'s put is shadowed by `c`'s delete — group order preserved.
    assert_eq!(get(&b, KV, b"a"), None);
    assert_eq!(get(&b, KV, b"b").as_deref(), Some(&b"2"[..]));
}

#[tokio::test]
async fn commit_group_empty_still_increments_stamp() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    let s0 = b.commit_group(Vec::new()).await.unwrap();
    let s1 = b.commit_group(Vec::new()).await.unwrap();
    assert!(s0 < s1, "empty group did not bump stamp: {s0:?} vs {s1:?}");
}

#[tokio::test]
async fn commit_group_partial_failure_aborts_whole_group() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    // META is unregistered; the whole group MUST abort.
    let mut good = b.begin_batch().unwrap();
    good.put(KV, b"k", b"v").unwrap();
    let mut bad = b.begin_batch().unwrap();
    bad.put(META, b"k", b"v").unwrap();

    let res = b.commit_group(vec![good, bad]).await;
    assert!(matches!(res, Err(BackendError::UnknownBucket(_))));
    // No visible effect — `good`'s put must not have landed.
    assert_eq!(get(&b, KV, b"k"), None);
}

#[tokio::test]
async fn failed_commit_does_not_bump_commit_seq() {
    // Documented contract: `CommitStamp` is strictly monotonic across
    // *successful* commits. A commit that aborts in the sync prologue
    // (here: UnknownBucket) MUST NOT bump the sequence, otherwise
    // callers observing stamps would see gaps that don't correspond
    // to real commits. Observed indirectly: the next successful
    // commit must return stamp #1 (not #2).
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    let mut bad = b.begin_batch().unwrap();
    bad.put(META, b"k", b"v").unwrap(); // META unregistered
    assert!(matches!(
        b.commit_batch(bad, true).await,
        Err(BackendError::UnknownBucket(_))
    ));
    let mut good = b.begin_batch().unwrap();
    good.put(KV, b"k", b"v").unwrap();
    let stamp = b.commit_batch(good, true).await.unwrap();
    assert_eq!(
        stamp,
        CommitStamp::new(1),
        "failed commit bumped commit_seq",
    );
}

// ---------- utility --------------------------------------------------

#[tokio::test]
async fn size_on_disk_grows_after_writes() {
    // redb pre-allocates an initial region (~1 MiB on current 4.x),
    // so small writes land inside that reservation without growing
    // the file. Push enough bytes that growth is unambiguous. The
    // trait's advisory-only contract means we assert `s1 >= s0` in
    // the post-small-write case, but require strict growth after a
    // multi-MiB write so the accessor is actually exercising the
    // filesystem metadata path.
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    let s0 = b.size_on_disk().unwrap();
    assert!(s0 > 0, "freshly-opened file has zero size");
    let mut batch = b.begin_batch().unwrap();
    // ~4 MiB of values: 2048 * 2048 bytes. Spills past the initial
    // region regardless of redb's current allocation heuristic.
    for i in 0u32..2048 {
        let k = i.to_be_bytes();
        batch.put(KV, &k, &[0xAB; 2048]).unwrap();
    }
    let _ = b.commit_batch(batch, true).await.unwrap();
    let s1 = b.size_on_disk().unwrap();
    assert!(s1 > s0, "size did not grow: {s0} -> {s1}");
}

#[tokio::test]
async fn defragment_on_closed_backend_returns_closed() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.close().unwrap();
    assert!(matches!(b.defragment().await, Err(BackendError::Closed)));
}

#[tokio::test]
async fn defragment_smoke_returns_ok() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    for i in 0u32..64 {
        let k = i.to_be_bytes();
        put(&b, KV, &k, &[0u8; 32]).await;
    }
    b.defragment().await.unwrap();
    assert_eq!(get(&b, KV, &0u32.to_be_bytes()).map(|v| v.len()), Some(32));
}

// ---------- cross-cutting oracle -------------------------------------

#[tokio::test]
async fn btreemap_oracle_matches_backend_for_mixed_workload() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();

    let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    // Deterministic pseudo-random sequence; no proptest yet — that
    // lives in the differential harness (ROADMAP:819+).
    for i in 0u16..200 {
        let k = vec![(i & 0xFF) as u8, ((i >> 8) & 0xFF) as u8];
        let v = vec![(i.wrapping_mul(31) & 0xFF) as u8; (i as usize & 0x07) + 1];
        put(&b, KV, &k, &v).await;
        oracle.insert(k, v);
    }
    let to_delete: Vec<Vec<u8>> = oracle.keys().step_by(7).cloned().collect();
    let mut batch = b.begin_batch().unwrap();
    for k in &to_delete {
        batch.delete(KV, k).unwrap();
    }
    let _ = b.commit_batch(batch, true).await.unwrap();
    for k in &to_delete {
        oracle.remove(k);
    }

    let snap = b.snapshot().unwrap();
    for (k, v) in &oracle {
        let got = snap.get(KV, k).unwrap();
        assert_eq!(got.as_deref(), Some(v.as_slice()), "mismatch at {k:?}");
    }
    for k in &to_delete {
        assert_eq!(snap.get(KV, k).unwrap(), None);
    }
}

// ---------- concurrency sanity --------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_committers_get_distinct_stamps() {
    // Small, deterministic concurrency sanity test — not a chaos run
    // (that's Phase 1's 7-day gate). Two tokio tasks both commit; the
    // stamps MUST differ. The redb write-lock serializes them, which
    // is exactly the etcd batch-tx serialization this layer mirrors.
    let tmp = TempDir::new().unwrap();
    let b = Arc::new(open(&tmp));
    b.register_bucket("kv", KV).unwrap();

    let b1 = Arc::clone(&b);
    let b2 = Arc::clone(&b);
    // RedbBatch is !Send (PhantomData<*const ()>), so it must drop
    // before the `.await` inside `tokio::spawn`. Constructing the
    // future in an inner block moves the batch into `commit_batch`
    // and drops the binding, then awaits the Send future.
    let t1 = tokio::spawn(async move {
        let fut = {
            let mut batch = b1.begin_batch().unwrap();
            batch.put(KV, b"t1", b"v").unwrap();
            b1.commit_batch(batch, true)
        };
        fut.await.unwrap()
    });
    let t2 = tokio::spawn(async move {
        let fut = {
            let mut batch = b2.begin_batch().unwrap();
            batch.put(KV, b"t2", b"v").unwrap();
            b2.commit_batch(batch, true)
        };
        fut.await.unwrap()
    });
    let s1 = t1.await.unwrap();
    let s2 = t2.await.unwrap();
    assert_ne!(s1, s2, "stamps collided: {s1:?} vs {s2:?}");
}

#[test]
fn dropping_uncommitted_batch_is_visible_noop() {
    let tmp = TempDir::new().unwrap();
    let b = open(&tmp);
    b.register_bucket("kv", KV).unwrap();
    {
        let mut batch = b.begin_batch().unwrap();
        batch.put(KV, b"k", b"never").unwrap();
        // `batch` is dropped here without commit.
    }
    assert_eq!(get(&b, KV, b"k"), None);
}
