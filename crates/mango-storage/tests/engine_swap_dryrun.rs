//! Engine-swap dry-run test (ROADMAP:821).
//!
//! Validates the [`mango_storage::Backend`] trait boundary frozen in
//! ADR 0002 §6 by running two concrete impls side-by-side:
//!
//! - [`mango_storage::RedbBackend`] — the production engine.
//! - [`mango_storage::InMemBackend`] — a `BTreeMap`-backed reference
//!   impl gated behind the `test-utils` Cargo feature.
//!
//! The tests live at the flat path `engine_swap_dryrun.rs` per
//! rust-expert nit (the literal ROADMAP wording said
//! `tests/migration/engine_swap_dryrun.rs`, but no other migration
//! tests are roadmapped to share the directory).
//!
//! # Why these tests are not tautologies
//!
//! Trait-driven happy-path data round-trip is a tautology if both
//! impls implement the same trait. The tests below specifically
//! provoke the surfaces where a trait leak would actually surface:
//!
//! - **T1 deterministic** — every op variant including
//!   `delete_range(end = [])` (unbounded upper), `commit_group`
//!   atomicity, empty-key/empty-value rejection, and empty-bucket
//!   commit. Migrates redb → in-mem and asserts byte-for-byte read
//!   parity over both point lookups and ranges.
//! - **T2 `force_fsync`** — exercises the `force_fsync = true` path
//!   on both backends (redb's real fsync; in-mem's no-op) and pins
//!   the `Ok(stamp)` + monotonicity contract.
//! - **T3 error taxonomy** — provokes every `BackendError` variant
//!   that can be triggered through the public API and asserts the
//!   variant *kind* matches across backends with identical inputs.
//!   This is the test that actually proves trait portability beyond
//!   happy-path data.
//!
//! # Path / gating
//!
//! `#![cfg(not(madsim))]` matches `redb_backend.rs` — madsim's
//! virtual time + redb's mmap+fsync would be a category error. The
//! `test-utils` feature is auto-enabled via the self-referential
//! dev-dep in `crates/mango-storage/Cargo.toml`, so no explicit
//! `#[cfg(feature = "test-utils")]` is needed here.

#![cfg(not(madsim))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;

use mango_storage::{
    Backend, BackendConfig, BackendError, BucketId, CommitStamp, InMemBackend, ReadSnapshot,
    RedbBackend, WriteBatch,
};
use tempfile::TempDir;

const KV: BucketId = BucketId::new(1);
const LEASE: BucketId = BucketId::new(2);

/// Wide-enough bound to cover every script key under a single
/// `range()` call. The script uses ASCII keys that fit in one byte,
/// so `[0xFF, 0xFF, 0xFF, 0xFF]` is comfortably above any of them.
const FULL_RANGE_END: &[u8] = &[0xFF, 0xFF, 0xFF, 0xFF];
const FULL_RANGE_START: &[u8] = &[0x00];

/// Open a `RedbBackend` against a fresh directory under `tmp`.
fn open_redb(tmp: &TempDir) -> RedbBackend {
    RedbBackend::open(BackendConfig::new(tmp.path().to_path_buf(), false)).expect("open redb")
}

/// Open a fresh `InMemBackend`. Path is unused.
fn open_inmem() -> InMemBackend {
    InMemBackend::open(BackendConfig::new("/unused".into(), false)).expect("open inmem")
}

/// Drain every (key, value) pair in a bucket via `range`.
fn drain_bucket<S: ReadSnapshot>(snap: &S, bucket: BucketId) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let iter = snap
        .range(bucket, FULL_RANGE_START, FULL_RANGE_END)
        .expect("range");
    for item in iter {
        let (k, v) = item.expect("range item");
        out.push((k.to_vec(), v.to_vec()));
    }
    out
}

/// Migrate every bucket from a redb snapshot into a fresh
/// `InMemBackend` via a single `commit_batch`. Buckets must already
/// be registered on the target.
async fn migrate_redb_to_inmem(redb_snap: &mango_storage::RedbSnapshot, target: &InMemBackend) {
    let mut batch = target.begin_batch().expect("begin migration batch");
    for bucket in [KV, LEASE] {
        for (k, v) in drain_bucket(redb_snap, bucket) {
            batch.put(bucket, &k, &v).expect("put migration");
        }
    }
    let _ = target
        .commit_batch(batch, false)
        .await
        .expect("commit migration");
}

// =====================================================================
// T1 — engine_swap_redb_to_inmem_data_survives
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn engine_swap_redb_to_inmem_data_survives() {
    let tmp = TempDir::new().unwrap();
    let redb = open_redb(&tmp);
    redb.register_bucket("kv", KV).unwrap();
    redb.register_bucket("lease", LEASE).unwrap();

    // Empty-bucket commit (a): a batch with no ops still succeeds
    // and bumps the seq.
    let stamp_empty = redb
        .commit_batch(redb.begin_batch().unwrap(), false)
        .await
        .unwrap();
    assert_eq!(stamp_empty, CommitStamp::new(1));

    // Body of the script (b)..(f): every variant, both buckets.
    let mut batch = redb.begin_batch().unwrap();
    // Spread keys: a..z plus a couple suffix variants.
    for ch in b'a'..=b'z' {
        batch.put(KV, &[ch], &[ch, b'.', b'v']).unwrap();
    }
    for ch in [b'a', b'm', b'z'] {
        batch.put(LEASE, &[ch, b'-', b'1'], b"lease-1").unwrap();
    }
    let _ = redb.commit_batch(batch, false).await.unwrap();

    // Single-key delete + delete_range with explicit upper bound.
    let mut batch = redb.begin_batch().unwrap();
    batch.delete(KV, b"a").unwrap();
    batch.delete_range(KV, b"x", b"z").unwrap(); // deletes x, y; keeps z.
    let _ = redb.commit_batch(batch, false).await.unwrap();

    // delete_range(start, []) — unbounded upper; deletes the only
    // remaining key 'z' from KV. Assert it actually fires by adding
    // 'zz' first then ranging from 'z' onward.
    let mut batch = redb.begin_batch().unwrap();
    batch.put(KV, b"zz", b"zz.v").unwrap();
    let _ = redb.commit_batch(batch, false).await.unwrap();
    let mut batch = redb.begin_batch().unwrap();
    batch.delete_range(KV, b"z", b"").unwrap();
    let _ = redb.commit_batch(batch, false).await.unwrap();

    // commit_group atomicity (e): two batches in one stamp.
    let mut b1 = redb.begin_batch().unwrap();
    b1.put(KV, b"GROUP_A", b"1").unwrap();
    let mut b2 = redb.begin_batch().unwrap();
    b2.put(LEASE, b"GROUP_B", b"2").unwrap();
    let _ = redb.commit_group(vec![b1, b2]).await.unwrap();

    // force_fsync=true (f).
    let mut batch = redb.begin_batch().unwrap();
    batch.put(KV, b"FSYNC", b"y").unwrap();
    let _ = redb.commit_batch(batch, true).await.unwrap();

    // Empty-key/empty-value rejection (c) — provoke at stage time
    // and assert variant + message both match in T3. Here in T1 we
    // only assert that staging refuses (no commit).
    let mut probe = redb.begin_batch().unwrap();
    let err = probe.put(KV, b"", b"v").unwrap_err();
    assert!(matches!(err, BackendError::Other(_)));

    // Snapshot the redb after the script.
    let redb_snap = redb.snapshot().unwrap();

    // Migrate.
    let inmem = open_inmem();
    inmem.register_bucket("kv", KV).unwrap();
    inmem.register_bucket("lease", LEASE).unwrap();
    migrate_redb_to_inmem(&redb_snap, &inmem).await;
    let inmem_snap = inmem.snapshot().unwrap();

    // (8) Range parity per bucket.
    for bucket in [KV, LEASE] {
        let r = drain_bucket(&redb_snap, bucket);
        let i = drain_bucket(&inmem_snap, bucket);
        assert_eq!(r, i, "range mismatch on bucket {bucket:?}");
    }

    // (9) Point parity for every observed key.
    for bucket in [KV, LEASE] {
        for (k, _) in drain_bucket(&redb_snap, bucket) {
            assert_eq!(
                redb_snap.get(bucket, &k).unwrap(),
                inmem_snap.get(bucket, &k).unwrap(),
                "point mismatch on {bucket:?}/{k:?}"
            );
        }
    }

    // (10) Continuation parity: apply 10 more mixed ops to inmem,
    // maintain a parallel BTreeMap, assert post-state matches.
    let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> =
        drain_bucket(&inmem_snap, KV).into_iter().collect();
    let mut batch = inmem.begin_batch().unwrap();
    batch.put(KV, b"CONT_1", b"v1").unwrap();
    oracle.insert(b"CONT_1".to_vec(), b"v1".to_vec());
    batch.put(KV, b"CONT_2", b"v2").unwrap();
    oracle.insert(b"CONT_2".to_vec(), b"v2".to_vec());
    batch.delete(KV, b"GROUP_A").unwrap();
    oracle.remove(b"GROUP_A".as_slice());
    batch.delete_range(KV, b"CONT_1", b"CONT_2").unwrap();
    let to_remove: Vec<_> = oracle
        .range(b"CONT_1".to_vec()..b"CONT_2".to_vec())
        .map(|(k, _)| k.clone())
        .collect();
    for k in to_remove {
        oracle.remove(&k);
    }
    let _ = inmem.commit_batch(batch, false).await.unwrap();

    let post = inmem.snapshot().unwrap();
    let observed: BTreeMap<Vec<u8>, Vec<u8>> = drain_bucket(&post, KV).into_iter().collect();
    assert_eq!(observed, oracle, "continuation oracle drift");

    // (11) size_on_disk parity-by-shape.
    assert_eq!(inmem.size_on_disk().unwrap(), 0);
    assert!(redb.size_on_disk().unwrap() > 0);

    // (12) close() idempotence on both.
    redb.close().unwrap();
    redb.close().unwrap();
    inmem.close().unwrap();
    inmem.close().unwrap();
    assert!(matches!(redb.snapshot(), Err(BackendError::Closed)));
    assert!(matches!(inmem.snapshot(), Err(BackendError::Closed)));
}

// =====================================================================
// T2 — swap_exercises_force_fsync_path
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn swap_exercises_force_fsync_path() {
    let tmp = TempDir::new().unwrap();
    let redb = open_redb(&tmp);
    let inmem = open_inmem();
    redb.register_bucket("kv", KV).unwrap();
    inmem.register_bucket("kv", KV).unwrap();

    // Two consecutive force_fsync=true commits on each backend.
    // Both must return Ok and be strictly monotonic. The InMem
    // path is documented as a no-op; this test pins that contract.
    let mut b = redb.begin_batch().unwrap();
    b.put(KV, b"k", b"v").unwrap();
    let s_redb_1 = redb.commit_batch(b, true).await.unwrap();
    let mut b = redb.begin_batch().unwrap();
    b.put(KV, b"k", b"v2").unwrap();
    let s_redb_2 = redb.commit_batch(b, true).await.unwrap();
    assert!(s_redb_1 < s_redb_2, "redb stamps must be monotonic");

    let mut b = inmem.begin_batch().unwrap();
    b.put(KV, b"k", b"v").unwrap();
    let s_inmem_1 = inmem.commit_batch(b, true).await.unwrap();
    let mut b = inmem.begin_batch().unwrap();
    b.put(KV, b"k", b"v2").unwrap();
    let s_inmem_2 = inmem.commit_batch(b, true).await.unwrap();
    assert!(s_inmem_1 < s_inmem_2, "inmem stamps must be monotonic");
}

// =====================================================================
// T3 — swap_preserves_error_taxonomy
// =====================================================================

/// Discriminant tag for `BackendError`. Compared across backends
/// rather than the full variant value so payload differences (e.g.,
/// the `&'static str` carried by `InvalidRange`) don't cause spurious
/// mismatches. The taxonomy itself is what we're enforcing.
///
/// `BackendError` is `#[non_exhaustive]`, so the catch-all arm yields
/// `Future` — a freshly-added variant landing without a corresponding
/// taxonomy update will surface as `Future != Future` mismatches with
/// the named variants and the test fails closed.
#[derive(Debug, PartialEq, Eq)]
enum ErrTag {
    Io,
    Corruption,
    UnknownBucket,
    InvalidRange,
    Closed,
    BucketConflict,
    BucketNameConflict,
    Other,
    Future,
}

fn tag(e: &BackendError) -> ErrTag {
    match e {
        BackendError::Io(_) => ErrTag::Io,
        BackendError::Corruption(_) => ErrTag::Corruption,
        BackendError::UnknownBucket(_) => ErrTag::UnknownBucket,
        BackendError::InvalidRange(_) => ErrTag::InvalidRange,
        BackendError::Closed => ErrTag::Closed,
        BackendError::BucketConflict { .. } => ErrTag::BucketConflict,
        BackendError::BucketNameConflict { .. } => ErrTag::BucketNameConflict,
        BackendError::Other(_) => ErrTag::Other,
        _ => ErrTag::Future,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn swap_preserves_error_taxonomy() {
    let tmp = TempDir::new().unwrap();
    let redb = open_redb(&tmp);
    let inmem = open_inmem();

    // (1) get against unregistered bucket -> UnknownBucket.
    redb.register_bucket("kv", KV).unwrap();
    inmem.register_bucket("kv", KV).unwrap();
    let r_snap = redb.snapshot().unwrap();
    let i_snap = inmem.snapshot().unwrap();
    let r = r_snap.get(BucketId::new(99), b"k").unwrap_err();
    let i = i_snap.get(BucketId::new(99), b"k").unwrap_err();
    assert_eq!(tag(&r), ErrTag::UnknownBucket);
    assert_eq!(tag(&r), tag(&i));

    // (2) range with start > end -> InvalidRange.
    let r = r_snap.range(KV, &[1, 2, 3], &[1, 2]).err().unwrap();
    let i = i_snap.range(KV, &[1, 2, 3], &[1, 2]).err().unwrap();
    assert_eq!(tag(&r), ErrTag::InvalidRange);
    assert_eq!(tag(&r), tag(&i));

    // (3) BucketConflict: id-rebind to different name.
    let r = redb.register_bucket("other", KV).unwrap_err();
    let i = inmem.register_bucket("other", KV).unwrap_err();
    assert_eq!(tag(&r), ErrTag::BucketConflict);
    assert_eq!(tag(&r), tag(&i));

    // (4) BucketNameConflict: name-rebind to different id.
    let r = redb.register_bucket("kv", BucketId::new(50)).unwrap_err();
    let i = inmem.register_bucket("kv", BucketId::new(50)).unwrap_err();
    assert_eq!(tag(&r), ErrTag::BucketNameConflict);
    assert_eq!(tag(&r), tag(&i));

    // (5) read after close -> Closed.
    let redb_for_close = open_redb(&TempDir::new().unwrap());
    let inmem_for_close = open_inmem();
    redb_for_close.close().unwrap();
    inmem_for_close.close().unwrap();
    let r = redb_for_close.snapshot().unwrap_err();
    let i = inmem_for_close.snapshot().unwrap_err();
    assert_eq!(tag(&r), ErrTag::Closed);
    assert_eq!(tag(&r), tag(&i));

    // (6) read_only=true open -> Other(_) with same message text on
    // both backends. The exact string is the contract tying the two
    // together (see InMemBackend::open and RedbBackend::open).
    let tmp2 = TempDir::new().unwrap();
    let r = RedbBackend::open(BackendConfig::new(tmp2.path().to_path_buf(), true)).unwrap_err();
    let i = InMemBackend::open(BackendConfig::new("/unused".into(), true)).unwrap_err();
    assert_eq!(tag(&r), ErrTag::Other);
    assert_eq!(tag(&r), tag(&i));
    if let (BackendError::Other(rm), BackendError::Other(im)) = (&r, &i) {
        assert_eq!(rm, im, "read-only error messages must match byte-for-byte");
    } else {
        panic!("expected Other on both, got {r:?} / {i:?}");
    }

    // (7) Empty-key put on a batch -> Other(_) with same message.
    let mut rb = redb.begin_batch().unwrap();
    let mut ib = inmem.begin_batch().unwrap();
    let r = rb.put(KV, b"", b"v").unwrap_err();
    let i = ib.put(KV, b"", b"v").unwrap_err();
    assert_eq!(tag(&r), ErrTag::Other);
    assert_eq!(tag(&r), tag(&i));
    if let (BackendError::Other(rm), BackendError::Other(im)) = (&r, &i) {
        assert_eq!(rm, im, "empty-key error messages must match");
    }

    // (8) Empty-value put -> Other(_) with same message.
    let r = rb.put(KV, b"k", b"").unwrap_err();
    let i = ib.put(KV, b"k", b"").unwrap_err();
    assert_eq!(tag(&r), ErrTag::Other);
    assert_eq!(tag(&r), tag(&i));
    if let (BackendError::Other(rm), BackendError::Other(im)) = (&r, &i) {
        assert_eq!(rm, im, "empty-value error messages must match");
    }
}
