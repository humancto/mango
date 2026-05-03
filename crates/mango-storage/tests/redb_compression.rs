//! Integration tests for ROADMAP:830 block-level compression.
//!
//! Exercises the production `RedbBackend` write+read path under both
//! [`CompressionMode::None`] and [`CompressionMode::Lz4`]; pins the
//! cross-mode read contract (a database written under one mode is
//! readable under the other, in either direction) so the codec stays
//! config-blind for the life of the project.
//!
//! Under `--cfg madsim` this file is excluded — same reasoning as
//! `redb_backend.rs` (madsim-tokio does not expose the multi-thread
//! runtime; redb's real mmap+fsync is incompatible with virtual time).

#![cfg(not(madsim))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use mango_storage::{
    Backend, BackendConfig, BucketId, CompressionMode, ReadSnapshot, RedbBackend, WriteBatch,
};
use tempfile::TempDir;

const KV: BucketId = BucketId::new(1);

fn open_with(dir: &TempDir, mode: CompressionMode) -> RedbBackend {
    RedbBackend::open(BackendConfig::new(dir.path().to_path_buf(), false).with_compression(mode))
        .expect("open")
}

async fn put(b: &RedbBackend, k: &[u8], v: &[u8]) {
    let mut batch = b.begin_batch().expect("begin_batch");
    batch.put(KV, k, v).expect("put");
    let _ = b.commit_batch(batch, true).await.expect("commit");
}

fn get(b: &RedbBackend, k: &[u8]) -> Option<Vec<u8>> {
    let snap = b.snapshot().expect("snapshot");
    snap.get(KV, k).expect("get").map(|bytes| bytes.to_vec())
}

/// A 256-byte highly compressible payload: one byte repeated. Long
/// enough to clear the `COMPRESS_MIN_BYTES` floor and to give LZ4 a
/// shot at meaningful shrinkage.
fn compressible(seed: u8, len: usize) -> Vec<u8> {
    vec![seed; len]
}

// ---------- round-trip both modes ------------------------------------

#[tokio::test]
async fn roundtrip_compression_none() {
    let tmp = TempDir::new().unwrap();
    let b = open_with(&tmp, CompressionMode::None);
    b.register_bucket("kv", KV).unwrap();
    put(&b, b"k", b"v").await;
    assert_eq!(get(&b, b"k").as_deref(), Some(&b"v"[..]));
}

#[tokio::test]
async fn roundtrip_compression_lz4() {
    let tmp = TempDir::new().unwrap();
    let b = open_with(&tmp, CompressionMode::Lz4);
    b.register_bucket("kv", KV).unwrap();
    let payload = compressible(0xAA, 1024);
    put(&b, b"k", &payload).await;
    assert_eq!(get(&b, b"k").as_deref(), Some(payload.as_slice()));
}

// ---------- cross-mode reads -----------------------------------------
//
// The decoder is config-blind (dispatches on the on-disk tag byte
// alone), so a database opened under any `CompressionMode` must be
// able to read every value previously stored under any other mode.
// Both directions are pinned here — and we round-trip THROUGH a
// close so the read backend is genuinely the second open of the
// same on-disk file, not an Arc-shared in-process state.

#[tokio::test]
async fn cross_mode_lz4_written_none_read() {
    let tmp = TempDir::new().unwrap();
    let payload = compressible(0x11, 2048);
    {
        let b = open_with(&tmp, CompressionMode::Lz4);
        b.register_bucket("kv", KV).unwrap();
        put(&b, b"k", &payload).await;
        b.close().unwrap();
    }
    let b2 = open_with(&tmp, CompressionMode::None);
    assert_eq!(get(&b2, b"k").as_deref(), Some(payload.as_slice()));
}

#[tokio::test]
async fn cross_mode_none_written_lz4_read() {
    let tmp = TempDir::new().unwrap();
    let payload = compressible(0x22, 2048);
    {
        let b = open_with(&tmp, CompressionMode::None);
        b.register_bucket("kv", KV).unwrap();
        put(&b, b"k", &payload).await;
        b.close().unwrap();
    }
    let b2 = open_with(&tmp, CompressionMode::Lz4);
    assert_eq!(get(&b2, b"k").as_deref(), Some(payload.as_slice()));
}

// ---------- mixed-tag range scan -------------------------------------
//
// Writes some rows under `None`, then reopens under `Lz4` and writes
// more rows. A range scan on a single backend (the second one) must
// return both the RAW-tagged and LZ4-tagged values intermixed and
// fully decoded. Pins the iterator's per-row decode dispatch.

#[tokio::test]
async fn range_scan_mixed_tags_decodes_each_row() {
    let tmp = TempDir::new().unwrap();
    // First open: write rows whose stored bytes will be RAW-tagged.
    {
        let b = open_with(&tmp, CompressionMode::None);
        b.register_bucket("kv", KV).unwrap();
        put(&b, b"a", b"alpha").await;
        put(&b, b"c", b"charlie").await;
        b.close().unwrap();
    }
    // Second open under Lz4: writes will be LZ4-tagged. The same
    // table now contains both tag flavors. Use a payload long enough
    // to clear COMPRESS_MIN_BYTES so the encoder actually picks LZ4.
    let b = open_with(&tmp, CompressionMode::Lz4);
    let big_b = compressible(0xBB, 1024);
    let big_d = compressible(0xDD, 1024);
    put(&b, b"b", &big_b).await;
    put(&b, b"d", &big_d).await;

    // Existing redb_backend tests use a high-byte sentinel for
    // "everything"; matching that convention so the read covers all
    // four rows.
    let snap = b.snapshot().expect("snapshot");
    let iter = snap
        .range(KV, b"", b"\xff\xff\xff\xff\xff\xff\xff\xff")
        .expect("range");
    let collected: Vec<(Vec<u8>, Vec<u8>)> = iter
        .map(|r| {
            let (k, v) = r.expect("row");
            (k.to_vec(), v.to_vec())
        })
        .collect();

    assert_eq!(collected.len(), 4, "saw rows: {collected:?}");
    assert_eq!(collected[0].0, b"a");
    assert_eq!(collected[0].1, b"alpha");
    assert_eq!(collected[1].0, b"b");
    assert_eq!(collected[1].1, big_b);
    assert_eq!(collected[2].0, b"c");
    assert_eq!(collected[2].1, b"charlie");
    assert_eq!(collected[3].0, b"d");
    assert_eq!(collected[3].1, big_d);
}

// ---------- on-disk size shrinkage -----------------------------------
//
// Pins ROADMAP:830's "size-comparison number" intent: with
// compression on, a highly compressible payload occupies strictly
// fewer bytes on disk than the same payload stored raw. This is the
// existence proof that the codec is actually engaged in the write
// path; it is NOT a benchmark and does not assert a specific ratio.
//
// We compact both files before measuring — redb's append-mostly
// layout means an uncompacted file can easily be larger than its
// payload, regardless of compression. After `defragment`, the file
// is dense.

#[tokio::test]
async fn lz4_shrinks_size_on_disk_for_compressible_payload() {
    // 64 KiB of a single byte: compresses to a few hundred bytes.
    let payload = compressible(0x55, 64 * 1024);

    let none_size = {
        let tmp = TempDir::new().unwrap();
        let b = open_with(&tmp, CompressionMode::None);
        b.register_bucket("kv", KV).unwrap();
        for i in 0u16..16 {
            let key = i.to_be_bytes();
            put(&b, &key, &payload).await;
        }
        b.defragment().await.expect("defragment");
        let s = b.size_on_disk().expect("size_on_disk");
        b.close().unwrap();
        s
    };

    let lz4_size = {
        let tmp = TempDir::new().unwrap();
        let b = open_with(&tmp, CompressionMode::Lz4);
        b.register_bucket("kv", KV).unwrap();
        for i in 0u16..16 {
            let key = i.to_be_bytes();
            put(&b, &key, &payload).await;
        }
        b.defragment().await.expect("defragment");
        let s = b.size_on_disk().expect("size_on_disk");
        b.close().unwrap();
        s
    };

    assert!(
        lz4_size < none_size,
        "expected LZ4 file < None file; got lz4={lz4_size} none={none_size}"
    );
}

// ---------- registry table wire format unchanged ---------------------
//
// The bucket-name registry on disk is `&str → u16`. It is persisted
// by `register_bucket` through a dedicated code path that does NOT
// flow through `apply_staged` — and therefore is NOT routed through
// the value-compression codec. If a future refactor were to
// accidentally re-route registry writes through the user-data write
// path, the stored u16 would be tag-prefixed and this test would
// fail (because the redb-side `&str → u16` decoder expects exactly
// 2 bytes for the value).
//
// We pin this by opening the redb file directly (bypassing
// `RedbBackend`) and reading the registry table via the same
// `TableDefinition` the production code uses.

#[test]
fn registry_table_wire_format_unchanged_under_lz4_mode() {
    let tmp = TempDir::new().unwrap();

    // Use a non-trivial id so a tag byte (0x00 / 0x01) added in front
    // would corrupt the value into a different u16 — making the
    // test sensitive to silent re-routing of the registry path.
    const ID_A: BucketId = BucketId::new(0x1234);
    const ID_B: BucketId = BucketId::new(0x5678);

    {
        let b = open_with(&tmp, CompressionMode::Lz4);
        b.register_bucket("kv", ID_A).unwrap();
        b.register_bucket("meta", ID_B).unwrap();
        b.close().unwrap();
    }

    // Open the file directly via the `redb` crate.
    let db_path = tmp.path().join("mango.redb");
    use ::redb::ReadableDatabase as _;
    let db = ::redb::Database::open(&db_path).expect("redb open");
    let txn = db.begin_read().expect("begin_read");
    // Same TableDefinition the production code uses; if the registry
    // path were ever wrapped in the compression codec, the value
    // bytes would no longer fit a `u16` decoding and this open or
    // get would error.
    let table_def: ::redb::TableDefinition<&str, u16> =
        ::redb::TableDefinition::new("__mango_bucket_registry");
    let table = txn.open_table(table_def).expect("open registry table");

    let kv_id = table
        .get("kv")
        .expect("registry get kv")
        .expect("kv present")
        .value();
    let meta_id = table
        .get("meta")
        .expect("registry get meta")
        .expect("meta present")
        .value();

    assert_eq!(kv_id, ID_A.raw, "kv id corrupted: 0x{kv_id:04x}");
    assert_eq!(meta_id, ID_B.raw, "meta id corrupted: 0x{meta_id:04x}");
}

// ---------- proptest round-trip --------------------------------------
//
// Random bytes through the production write+read path under Lz4.
// Covers inputs below the threshold floor (RAW tag) and above it
// (RAW or LZ4 tag depending on compressibility). The cap on size and
// case count keeps the test fast in default mode; bumping
// `MANGO_COMPRESSION_THOROUGH=1` raises the cap.

// Plain `#[test]` (NOT `#[tokio::test]`): proptest's runner is
// synchronous, and nesting `block_on` inside a tokio test runtime
// panics with "Cannot start a runtime from within a runtime". Owning
// the runtime here side-steps that.
#[test]
fn proptest_random_bytes_roundtrip_under_lz4() {
    use proptest::prelude::*;

    let cases = if std::env::var("MANGO_COMPRESSION_THOROUGH").is_ok() {
        2_000
    } else {
        128
    };

    let mut runner = proptest::test_runner::TestRunner::new(proptest::test_runner::Config {
        cases,
        ..proptest::test_runner::Config::default()
    });

    let strat = (
        proptest::collection::vec(any::<u8>(), 0..1024),
        proptest::collection::vec(any::<u8>(), 0..1024),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let tmp = TempDir::new().unwrap();
    let b = open_with(&tmp, CompressionMode::Lz4);
    b.register_bucket("kv", KV).unwrap();

    runner
        .run(&strat, |(key, value)| {
            // Drop empty keys: redb rejects them under our
            // TableDefinition. The codec itself is tested for empty
            // values in unit tests.
            if key.is_empty() {
                return Ok(());
            }
            rt.block_on(async {
                put(&b, &key, &value).await;
            });
            let read = get(&b, &key);
            prop_assert_eq!(read.as_deref(), Some(value.as_slice()));
            Ok(())
        })
        .unwrap();
}
