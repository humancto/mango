//! End-to-end smoke test for the MVCC KV API against a real
//! [`RedbBackend`] (L844 plan §7.2).
//!
//! Most `MvccStore` unit tests in `crates/mango-mvcc/src/store/`
//! drive the in-memory backend (`mango-storage`'s `test-utils`
//! `InMemBackend`). That gives fast, deterministic coverage of
//! the MVCC logic itself but skips the real-backend integration:
//!
//! - actual disk I/O via `redb`,
//! - bucket registration against a persistent file,
//! - snapshot/iteration of encoded keys at scale (1k entries),
//! - the L852 boundary — opening a non-empty file rejects with
//!   `OpenError::NonEmptyBackend { found_revs: ... }`.
//!
//! The flow exercises the full surface a future `MvccStore` user
//! (the etcd gRPC server in L854) will rely on:
//!
//!   1. open against a tempdir (registers buckets),
//!   2. 1k Puts (sequential, sub = 0 each — distinct keys),
//!   3. range-scan (count-only and full),
//!   4. delete half via `DeleteRange`,
//!   5. range-scan again (post-delete visibility),
//!   6. compact at the highest revision,
//!   7. drop the store, reopen → expect
//!      `OpenError::NonEmptyBackend`.
//!
//! Under `--cfg madsim` this file is excluded — same rationale as
//! `crates/mango-storage/tests/redb_backend.rs`: real redb's
//! mmap+fsync under the simulator's virtual time is a category
//! error.

#![cfg(not(madsim))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::arithmetic_side_effects,
    missing_docs,
    reason = "test code: panics are the assertion mechanism, arithmetic is bounded by loop counters"
)]

use mango_mvcc::error::OpenError;
use mango_mvcc::store::range::RangeRequest;
use mango_mvcc::store::MvccStore;
use mango_storage::{BackendConfig, RedbBackend};
use tempfile::TempDir;

/// Number of distinct keys seeded; chosen to exercise multi-page
/// redb scans without blowing the test runtime past 5s (plan N4).
const N: usize = 1_000;

fn open_redb(dir: &TempDir) -> RedbBackend {
    use mango_storage::Backend;
    RedbBackend::open(BackendConfig::new(dir.path().to_path_buf(), false))
        .expect("open RedbBackend")
}

fn key(i: usize) -> Vec<u8> {
    // Zero-padded so byte-lex order matches numeric order — makes
    // the "delete the lower half" range bound trivial to construct.
    format!("k{i:08}").into_bytes()
}

fn value(i: usize) -> Vec<u8> {
    format!("v{i}").into_bytes()
}

/// `RangeRequest` is `#[non_exhaustive]` from outside the crate, so
/// callers cannot use struct-expression init. Build via
/// `default()` + per-field mutation.
fn req_full() -> RangeRequest {
    let mut r = RangeRequest::default();
    r.key = key(0);
    r.end = key(N);
    r
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redb_kv_smoke_end_to_end() {
    let tmp = TempDir::new().expect("tempdir");

    // ---------- 1. open + 1k puts ----------
    // `usize` → `i64` / `u64` conversions go through `try_from` per
    // workspace policy (clippy::cast_possible_wrap). N is 1_000, so
    // the conversion never panics in practice.
    let total_main: i64 = i64::try_from(N).expect("N fits in i64");
    let total_count: u64 = u64::try_from(N).expect("N fits in u64");
    {
        let backend = open_redb(&tmp);
        let store = MvccStore::open(backend).expect("open mvcc store");

        for i in 0..N {
            let rev = store.put(&key(i), &value(i)).await.expect("put");
            let i_i64 = i64::try_from(i).expect("i fits in i64");
            // Each Put on a distinct key allocates one main, sub=0.
            // Numbering starts at 1, so the i-th put is at main = i+1.
            assert_eq!(
                rev.main(),
                i_i64 + 1,
                "put #{i}: expected main = {}, got {}",
                i + 1,
                rev.main()
            );
            assert_eq!(rev.sub(), 0, "put #{i}: expected sub = 0");
        }
        let highest_rev_main = store.current_revision();
        assert_eq!(highest_rev_main, total_main);

        // ---------- 2. range-scan (full) ----------
        let full = store.range(req_full()).expect("range full");
        assert_eq!(full.kvs.len(), N, "full range count");
        assert_eq!(full.count, total_count);
        assert!(!full.more);
        // First and last KV bytes match what we seeded.
        assert_eq!(full.kvs.first().expect("first kv").key.as_ref(), key(0));
        assert_eq!(full.kvs.last().expect("last kv").key.as_ref(), key(N - 1));

        // count_only yields the same count without copying values.
        let mut req = req_full();
        req.count_only = true;
        let count_only = store.range(req).expect("range count_only");
        assert!(count_only.kvs.is_empty(), "count_only yields no kvs");
        assert_eq!(count_only.count, total_count);

        // ---------- 3. delete the lower half ----------
        let half = N / 2;
        let half_u64 = u64::try_from(half).expect("half fits in u64");
        let (deleted, del_rev) = store
            .delete_range(&key(0), &key(half))
            .await
            .expect("delete_range lower half");
        assert_eq!(deleted, half_u64);
        // delete_range allocates exactly one main, sub-stream per
        // physical write — the returned rev's main is the next
        // after the puts.
        assert_eq!(del_rev.main(), total_main + 1);
        assert_eq!(del_rev.sub(), 0);

        // ---------- 4. range-scan again ----------
        let post_delete = store.range(req_full()).expect("range post-delete");
        assert_eq!(post_delete.kvs.len(), N - half, "post-delete kvs len");
        let surviving_u64 = u64::try_from(N - half).expect("surviving fits in u64");
        assert_eq!(post_delete.count, surviving_u64);
        // First surviving key is now `key(half)`.
        assert_eq!(
            post_delete
                .kvs
                .first()
                .expect("first surviving")
                .key
                .as_ref(),
            key(half)
        );

        // ---------- 5. compact at the latest revision ----------
        let compact_at = store.current_revision();
        store
            .compact(compact_at)
            .await
            .expect("compact at current rev");

        // After compact, range at the compacted rev still works
        // (B1 — `<= compacted` is the boundary, not `<`).
        let mut req_at_rev = req_full();
        req_at_rev.revision = Some(compact_at);
        let after_compact = store.range(req_at_rev).expect("range at compacted rev");
        assert_eq!(after_compact.count, surviving_u64);

        // Drop the store + backend so redb's file lock releases.
        drop(store);
    }

    // ---------- 6. reopen → NonEmptyBackend ----------
    let backend2 = open_redb(&tmp);
    match MvccStore::open(backend2) {
        Err(OpenError::NonEmptyBackend { found_revs }) => {
            assert!(found_revs > 0, "expected found_revs > 0, got {found_revs}");
        }
        Ok(_) => panic!("expected NonEmptyBackend on non-empty reopen"),
        Err(other) => panic!("expected NonEmptyBackend, got {other:?}"),
    }
}
