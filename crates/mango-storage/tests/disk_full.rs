//! ROADMAP:827 — disk-full reliability test.
//!
//! Wraps [`redb::backends::FileBackend`] in an arm-gated wrapper that
//! returns ENOSPC from `set_len()` and `write()` when armed. Pure
//! in-process — no `LD_PRELOAD`, no child re-exec, runs on macOS and
//! Linux equivalently.
//!
//! MIRROR-WITH `crash_recovery_eio.rs` (rust-expert N4) — divergence
//! between these two test files means one of them is wrong. When you
//! change one, look at the other.
//!
//! # ROADMAP:827 reading
//!
//! ROADMAP:827 reads in full as: "fill the data dir to 100%, attempt
//! a write; assert the server enters read-only mode, raises NOSPACE,
//! never crashes, never corrupts; free space; assert the server
//! recovers cleanly and accepts writes."
//!
//! There is no "server" yet at Phase 1 — that maps to [`RedbBackend`].
//! The roadmap's "server enters read-only mode" + "raises NOSPACE"
//! reads as etcd-shape `Alarm(NOSPACE)` semantics: a latched,
//! durable, operator-disarmed flag. The [`Backend`] trait has no
//! `BackendError::NoSpace` variant and no alarm/quota machinery —
//! that's a Phase 6 operability item (cross-ref `ROADMAP.md:1051-1052`,
//! "refuse writes when DB size exceeds configured quota; raise
//! NOSPACE alarm"). This test pins **only the verifiable trait-level
//! subset**:
//!
//! - **P1 (no silent data loss for acknowledged commits)** — every
//!   commit that returned `Ok(_)` *before* an ENOSPC is still visible
//!   after a production-path reopen.
//! - **P2 (failures are surfaced)** — armed commits return `Err(_)`.
//!   The shape is `Err(Io(ENOSPC))` (raw errno surfaced) OR
//!   `Err(Corruption(_))` (`PreviousIo` poisoning). Both are valid
//!   "reported failure" arms per the trait contract.
//! - **P3 (recovery via production-path reopen)** — after disarm +
//!   close + production-path reopen, the next commit succeeds and
//!   prior data is intact. **Strictly weaker than the roadmap's
//!   "recovers cleanly without restart"** — same-handle recovery is
//!   blocked by redb's `PreviousIo` poisoning (deferred).
//!
//! Each test additionally asserts **non-corruption explicitly** at
//! the post-ENOSPC reopen step (rust-expert M1): the reopen helper
//! has its own `Err(BackendError::Corruption(_)) => panic!(...)`
//! arm so failure attribution is unambiguous — "file is structurally
//! damaged" vs "reopen failed for some other I/O reason" vs "reopen
//! succeeded but data is missing" land on distinct panic strings.
//!
//! T1 additionally pins **two consecutive armed commits** on the
//! same handle to exercise the `PreviousIo` poisoning path. The
//! plan's stricter "first arm MUST be Io, second MUST be Corruption"
//! disjunction was loosened to the same shape used by
//! `crash_recovery_eio.rs::assert_eio_or_poisoned` (rust-expert N1):
//! ENOSPC may diverge from EIO if redb's `set_len`-failure path
//! handles `needs_recovery` differently from its `sync_data`-failure
//! path, and that divergence is itself the contribution. If a
//! future tightening confirms the second arm is reliably Corruption
//! across both errno paths, the helper should split into per-arm
//! variants for both this test and the EIO test in lock-step.
//!
//! # Out of scope
//!
//! - Real bounded-FS testing (loopback ext4, fixed-size tmpfs).
//!   Linux-only; tests kernel-side ENOSPC paths. Future chaos axis.
//! - `Alarm(NOSPACE)` / read-only mode (latched flag). Requires
//!   `BackendError::NoSpace` + alarm machinery + operator-disarm
//!   RPC, as a single coherent change. Phase 6 operability
//!   (`ROADMAP.md:1051-1052`).
//! - Same-handle auto-recovery after FS frees space. redb's
//!   `PreviousIo` poisoning prevents this by design.
//! - Operator-driven alarm disarm. No operator surface in Phase 1.
//! - Read-path ENOSPC. `read()` doesn't allocate.
//!
//! # Implementation notes
//!
//! 1. **`ENOSPC = 28`** on Linux/macOS. Used directly with a
//!    comment — keeps the test pure (no `libc` dep). Mirrors
//!    `EIO = 5` handling in `crash_recovery_eio.rs`.
//!
//! 2. **Injection on both `set_len` and `write`.** redb grows the
//!    file in two phases: `set_len` to extend, `write` to populate.
//!    ENOSPC most realistically surfaces from `set_len` (allocation),
//!    but delayed-allocation FSes can surface it from `write` too.
//!    Wrapper injects on both — overcoverage, not undercoverage.
//!
//! 3. **`Database::Drop` re-runs `sync_data` four times.** Same as
//!    EIO test; not a concern here because `sync_data` is NOT armed.
//!    The disarm-before-drop discipline still applies: enforced
//!    structurally via the [`DisarmOnDrop`] RAII guard, not manual
//!    stores. If a future contributor extends [`NoSpaceBackend`] to
//!    also inject on `sync_data`, the discipline is already in
//!    place.
//!
//! 4. **`PreviousIo` poisoning.** After the first injected ENOSPC,
//!    redb sets the in-memory `needs_recovery` flag on *that*
//!    [`redb::Database`] instance. Subsequent commits on the same
//!    handle return [`BackendError::Corruption`] (the `PreviousIo`
//!    path). A fresh `Database::create` from a production-path
//!    reopen has the flag clear and either repairs or finds clean
//!    state — that's how P3 works.
//!
//! 5. **Memory ordering.** `inject_nospace` is shared between the
//!    test thread (`store(true, Release)`) and the
//!    `tokio::task::spawn_blocking` worker that runs the redb
//!    commit (`load(Acquire)`). Release/Acquire establishes
//!    happens-before so the load on the worker sees the flip
//!    without UB. The [`DisarmOnDrop`] guard's `Drop` impl uses
//!    `Release` for the same reason — `Relaxed` would not pair
//!    with the worker's `Acquire`.

#![cfg(not(madsim))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use std::fs::OpenOptions;
use std::io::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use mango_storage::{
    Backend, BackendConfig, BackendError, BucketId, ReadSnapshot, RedbBackend, WriteBatch as _,
};
use redb::{backends::FileBackend, StorageBackend};
use tempfile::TempDir;

/// Linux/macOS errno for "no space left on device". Both platforms
/// use 28.
const ENOSPC: i32 = 28;

/// Bucket id used by every test.
const KV: BucketId = BucketId::new(1);
const KV2: BucketId = BucketId::new(2);

/// File name redb writes inside `BackendConfig::data_dir`. Mirrors
/// `crates/mango-storage/src/redb/mod.rs`'s `DB_FILENAME`. Kept
/// in sync manually — if that constant ever changes, this test
/// breaks visibly at the file path.
const DB_FILENAME: &str = "mango.redb";

// --- The injection wrapper ------------------------------------------

/// Wraps [`FileBackend`] and returns ENOSPC from
/// [`StorageBackend::set_len`] and [`StorageBackend::write`] when
/// armed. All other methods delegate verbatim. Pattern mirrors
/// `EioOnSyncBackend` in `crash_recovery_eio.rs`.
#[derive(Debug)]
struct NoSpaceBackend {
    inner: FileBackend,
    inject_nospace: Arc<AtomicBool>,
}

impl NoSpaceBackend {
    fn new(file: std::fs::File, inject_nospace: Arc<AtomicBool>) -> Self {
        Self {
            inner: FileBackend::new(file).expect("file lock conflict in test fixture"),
            inject_nospace,
        }
    }
}

impl StorageBackend for NoSpaceBackend {
    fn len(&self) -> Result<u64, Error> {
        self.inner.len()
    }
    fn read(&self, off: u64, out: &mut [u8]) -> Result<(), Error> {
        self.inner.read(off, out)
    }
    fn set_len(&self, len: u64) -> Result<(), Error> {
        // Acquire pairs with `inject_nospace.store(true, Release)` in
        // the test bodies (and with the `DisarmOnDrop` guard's Release
        // store). The Release happens-before the load that observes
        // `true`, so this load sees the flip without UB even when the
        // redb commit runs on a `spawn_blocking` worker thread.
        if self.inject_nospace.load(Ordering::Acquire) {
            Err(Error::from_raw_os_error(ENOSPC))
        } else {
            self.inner.set_len(len)
        }
    }
    fn write(&self, off: u64, data: &[u8]) -> Result<(), Error> {
        if self.inject_nospace.load(Ordering::Acquire) {
            Err(Error::from_raw_os_error(ENOSPC))
        } else {
            self.inner.write(off, data)
        }
    }
    fn close(&self) -> Result<(), Error> {
        self.inner.close()
    }
    fn sync_data(&self) -> Result<(), Error> {
        self.inner.sync_data()
    }
}

// --- Disarm-on-drop guard (#63 + rust-expert N3) --------------------

/// RAII guard that flips an arm flag back to `false` on drop. Held
/// across the armed window so an early-return between arm and drop
/// (e.g., a future `?` propagation) cannot leak armed state to
/// `Database::Drop`'s `sync_data` calls.
///
/// `Release` ordering pairs with the wrapper's `Acquire` load on
/// `inject_nospace`. `Relaxed` would not establish happens-before
/// across the `spawn_blocking` boundary that runs the engine's
/// drop-time fsync sequence.
struct DisarmOnDrop<'a>(&'a AtomicBool);

impl Drop for DisarmOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

// --- Helpers --------------------------------------------------------

fn db_file(data_dir: &Path) -> PathBuf {
    data_dir.join(DB_FILENAME)
}

/// Open the backend via the production code path. Used for clean
/// baselines.
fn open_production(data_dir: &Path) -> RedbBackend {
    RedbBackend::open(BackendConfig::new(data_dir.to_path_buf(), false))
        .expect("production-path open failed")
}

/// Open the backend with the ENOSPC-injection wrapper. The
/// `data_dir` must already contain a clean `mango.redb` (production-
/// path open + close performed earlier in the test).
///
/// `inject_nospace = false` at construction so the open-path
/// `mem.begin_writable()` flush passes through.
fn open_with_injection(data_dir: &Path, inject_nospace: Arc<AtomicBool>) -> RedbBackend {
    let path = db_file(data_dir);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .expect("open db file for injection wrapper");
    let backend = NoSpaceBackend::new(file, inject_nospace);
    RedbBackend::with_backend(backend, path).expect("RedbBackend::with_backend")
}

/// Snapshot-probe expecting a specific value. Mirrors
/// `assert_equals` in `crash_recovery_eio.rs`.
fn assert_equals(
    snap: &impl ReadSnapshot,
    bucket: BucketId,
    key: &[u8],
    expected: &[u8],
    ctx: &str,
) {
    match snap.get(bucket, key) {
        Ok(Some(v)) => assert_eq!(
            v,
            Bytes::copy_from_slice(expected),
            "{ctx}: key {key:?} value mismatch after ENOSPC+reopen",
        ),
        Ok(None) => panic!(
            "{ctx}: prior-committed key {key:?} missing after ENOSPC+reopen — \
             this is a silent-data-loss bug",
        ),
        Err(BackendError::UnknownBucket(b)) => {
            panic!("{ctx}: registry lost across ENOSPC+reopen (bucket {b:?} missing)",)
        }
        Err(e) => panic!("{ctx}: snapshot probe failed: {e}"),
    }
}

/// Assert the result is `Err(Io)` with errno == ENOSPC, OR
/// `Err(Corruption(_))` (`PreviousIo` path). Both are valid
/// "reported failure" shapes per the trait contract under ENOSPC.
/// Mirrors `assert_eio_or_poisoned` in `crash_recovery_eio.rs`.
fn assert_enospc_or_poisoned<T>(r: Result<T, BackendError>, ctx: &str) {
    match r {
        Ok(_) => panic!("{ctx}: commit succeeded under armed ENOSPC injection"),
        Err(BackendError::Io(io)) => assert_eq!(
            io.raw_os_error(),
            Some(ENOSPC),
            "{ctx}: expected ENOSPC, got {io:?}",
        ),
        Err(BackendError::Corruption(_)) => {} // PreviousIo path
        Err(e) => panic!("{ctx}: unexpected error variant: {e}"),
    }
}

/// Open via the production path AND assert the result is not
/// [`BackendError::Corruption`]. The corruption arm has its own
/// distinct panic so post-ENOSPC reopen failure attribution is
/// unambiguous (rust-expert M1 — "structurally damaged" lands on a
/// separate panic string from "reopen failed for some other I/O
/// reason").
fn assert_reopen_ok(data_dir: &Path, ctx: &str) -> RedbBackend {
    match RedbBackend::open(BackendConfig::new(data_dir.to_path_buf(), false)) {
        Ok(b) => b,
        Err(BackendError::Corruption(c)) => panic!(
            "{ctx}: post-ENOSPC reopen returned Corruption — file is \
             structurally damaged: {c}",
        ),
        Err(e) => panic!("{ctx}: post-ENOSPC reopen failed: {e}"),
    }
}

// --- T1: commit under ENOSPC reports failure; prior survives; reopen recovers
//
// T1 is the only test that probes a second armed commit on the same
// handle to exercise redb's `PreviousIo` behavior. T2-T4 only assert
// the first-arm shape, then disarm + reopen + recover.

#[tokio::test(flavor = "multi_thread")]
async fn t1_commit_under_enospc_reports_failure_and_prior_survives() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path();

    // 1. Production-path open, register bucket, commit a known
    //    pre-arm batch. Close.
    {
        let backend = open_production(path);
        backend.register_bucket("kv", KV).expect("register kv");
        let mut batch = backend.begin_batch().expect("begin_batch");
        batch.put(KV, b"k_pre", b"v_pre").expect("put pre");
        let _ = backend
            .commit_batch(batch, true)
            .await
            .expect("pre-arm commit");
        backend.close().expect("close clean baseline");
    }

    // 2. Injection-wrapper open with inject_nospace = false so the
    //    open-time flush passes through. Snapshot-probe baseline.
    // 3. Arm via store(true, Release). Hold a `DisarmOnDrop` guard so
    //    the disarm is structural, not manual.
    // 4. First armed commit → P2: assert_enospc_or_poisoned.
    // 5. Second armed commit on the same handle → P2 (PreviousIo
    //    poisoning likely active; same disjunction applies).
    let inject_nospace = Arc::new(AtomicBool::new(false));
    {
        let backend = open_with_injection(path, Arc::clone(&inject_nospace));

        let snap = backend.snapshot().expect("snap baseline");
        assert_equals(&snap, KV, b"k_pre", b"v_pre", "T1 baseline");
        drop(snap);

        inject_nospace.store(true, Ordering::Release);
        let _disarm = DisarmOnDrop(&inject_nospace);

        let mut batch = backend.begin_batch().expect("begin_batch armed");
        batch
            .put(KV, b"k_uncommitted", b"v_uncommitted")
            .expect("put uncommitted");
        let r = backend.commit_batch(batch, true).await;
        assert_enospc_or_poisoned(r, "T1 first armed commit");

        let mut batch2 = backend.begin_batch().expect("begin_batch armed 2");
        batch2
            .put(KV, b"k_uncommitted_2", b"v2")
            .expect("put uncommitted 2");
        let r2 = backend.commit_batch(batch2, true).await;
        assert_enospc_or_poisoned(r2, "T1 second armed commit");

        // DisarmOnDrop fires here.
        let _ = backend.close(); // may Err under PreviousIo; OK either way
    }

    // 6. Production-path reopen → P3 (no Corruption, M1 explicit).
    let backend = assert_reopen_ok(path, "T1 post-ENOSPC reopen");
    // 7. P1: prior committed bytes survive.
    let snap = backend.snapshot().expect("snapshot post-reopen");
    assert_equals(&snap, KV, b"k_pre", b"v_pre", "T1 post-reopen prior (P1)");
    drop(snap);
    // 8. P3 (forward progress): commit a fresh batch.
    let mut batch = backend.begin_batch().expect("begin_batch post-reopen");
    batch.put(KV, b"k_post", b"v_post").expect("put post");
    let _ = backend
        .commit_batch(batch, true)
        .await
        .expect("post-reopen commit must succeed");
    backend.close().expect("close post-reopen");
    // 9. Reopen + snapshot → both keys visible.
    let backend = open_production(path);
    let snap = backend.snapshot().expect("snapshot final");
    assert_equals(&snap, KV, b"k_pre", b"v_pre", "T1 final k_pre");
    assert_equals(&snap, KV, b"k_post", b"v_post", "T1 final k_post");
}

// --- T2: multi-op commit under ENOSPC; prior commits survive ----

#[tokio::test(flavor = "multi_thread")]
async fn t2_multi_op_commit_under_enospc_reports_failure_and_prior_survives() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path();

    // 1. Five successful single-key commits via production path.
    {
        let backend = open_production(path);
        backend.register_bucket("kv", KV).expect("register kv");
        for i in 0u8..5 {
            let mut batch = backend.begin_batch().expect("begin_batch");
            let k = [b'k', b'0' + i];
            let v = [b'v', b'0' + i];
            batch.put(KV, &k, &v).expect("put");
            let _ = backend.commit_batch(batch, true).await.expect("commit");
        }
        backend.close().expect("close baseline");
    }

    // 2-3. Injection wrapper, verify baseline. Arm. Commit a multi-op
    //      batch with 3 puts → P2.
    let inject_nospace = Arc::new(AtomicBool::new(false));
    {
        let backend = open_with_injection(path, Arc::clone(&inject_nospace));

        let snap = backend.snapshot().expect("snap baseline");
        for i in 0u8..5 {
            let k = [b'k', b'0' + i];
            let v = [b'v', b'0' + i];
            assert_equals(&snap, KV, &k, &v, "T2 baseline visibility");
        }
        drop(snap);

        inject_nospace.store(true, Ordering::Release);
        let _disarm = DisarmOnDrop(&inject_nospace);

        let mut batch = backend.begin_batch().expect("begin_batch armed");
        batch.put(KV, b"k5", b"v5").expect("put k5");
        batch.put(KV, b"k6", b"v6").expect("put k6");
        batch.put(KV, b"k7", b"v7").expect("put k7");
        let r = backend.commit_batch(batch, true).await;
        assert_enospc_or_poisoned(r, "T2 armed multi-op commit");

        let _ = backend.close();
    }

    // 4. Production-path reopen → P3 not Corruption (M1).
    let backend = assert_reopen_ok(path, "T2 post-ENOSPC reopen");
    // 5. P1: all 5 prior keys present.
    let snap = backend.snapshot().expect("snapshot post-reopen");
    for i in 0u8..5 {
        let k = [b'k', b'0' + i];
        let v = [b'v', b'0' + i];
        assert_equals(&snap, KV, &k, &v, "T2 post-reopen prior (P1)");
    }
    drop(snap);
    // 6. P3 forward progress.
    let mut batch = backend.begin_batch().expect("begin_batch post-reopen");
    batch.put(KV, b"k_post", b"v_post").expect("put post");
    let _ = backend
        .commit_batch(batch, true)
        .await
        .expect("T2 post-reopen commit must succeed");
}

// --- T3: commit_group under ENOSPC; seed survives + recovers ----

#[tokio::test(flavor = "multi_thread")]
async fn t3_commit_group_under_enospc_reports_failure_and_seed_survives() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path();

    // 1. Production-path open, register bucket, commit one seed.
    {
        let backend = open_production(path);
        backend.register_bucket("kv", KV).expect("register kv");
        let mut batch = backend.begin_batch().expect("begin_batch");
        batch.put(KV, b"k_pre", b"v_pre").expect("put pre");
        let _ = backend
            .commit_batch(batch, true)
            .await
            .expect("seed commit");
        backend.close().expect("close baseline");
    }

    // 2-3. Injection wrapper. Verify seed. Arm. commit_group of 3
    //      batches → P2.
    let inject_nospace = Arc::new(AtomicBool::new(false));
    {
        let backend = open_with_injection(path, Arc::clone(&inject_nospace));

        let snap = backend.snapshot().expect("snap baseline");
        assert_equals(&snap, KV, b"k_pre", b"v_pre", "T3 baseline seed");
        drop(snap);

        inject_nospace.store(true, Ordering::Release);
        let _disarm = DisarmOnDrop(&inject_nospace);

        let mut a = backend.begin_batch().expect("begin_batch a");
        a.put(KV, b"k_a", b"v_a").expect("put k_a");
        let mut b = backend.begin_batch().expect("begin_batch b");
        b.put(KV, b"k_b", b"v_b").expect("put k_b");
        let mut c = backend.begin_batch().expect("begin_batch c");
        c.put(KV, b"k_c", b"v_c").expect("put k_c");
        let r = backend.commit_group(vec![a, b, c]).await;
        assert_enospc_or_poisoned(r, "T3 armed commit_group");

        let _ = backend.close();
    }

    // 4. Reopen → P3 not Corruption.
    let backend = assert_reopen_ok(path, "T3 post-ENOSPC reopen");
    // 5. P1: seed survives.
    let snap = backend.snapshot().expect("snapshot post-reopen");
    assert_equals(&snap, KV, b"k_pre", b"v_pre", "T3 post-reopen seed (P1)");
    drop(snap);
    // 6. P3: another commit_group of fresh batches succeeds.
    let mut a = backend.begin_batch().expect("begin_batch a2");
    a.put(KV, b"k_a2", b"v_a2").expect("put k_a2");
    let mut b = backend.begin_batch().expect("begin_batch b2");
    b.put(KV, b"k_b2", b"v_b2").expect("put k_b2");
    let _ = backend
        .commit_group(vec![a, b])
        .await
        .expect("T3 post-reopen commit_group must succeed");
}

// --- T4: register_bucket under ENOSPC; existing survives + recovers

#[tokio::test(flavor = "multi_thread")]
async fn t4_register_bucket_under_enospc_reports_failure_and_existing_survives() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path();

    // 1. Production-path open, register "kv", commit pre-arm key.
    {
        let backend = open_production(path);
        backend.register_bucket("kv", KV).expect("register kv");
        let mut batch = backend.begin_batch().expect("begin_batch baseline");
        batch.put(KV, b"kpre", b"vpre").expect("put baseline");
        let _ = backend
            .commit_batch(batch, true)
            .await
            .expect("baseline commit");
        backend.close().expect("close baseline");
    }

    // 2-3. Injection wrapper. Verify baseline. Arm. register_bucket
    //      "kv2" → P2.
    let inject_nospace = Arc::new(AtomicBool::new(false));
    {
        let backend = open_with_injection(path, Arc::clone(&inject_nospace));

        let snap = backend.snapshot().expect("snap baseline");
        assert_equals(&snap, KV, b"kpre", b"vpre", "T4 baseline visibility");
        drop(snap);

        inject_nospace.store(true, Ordering::Release);
        let _disarm = DisarmOnDrop(&inject_nospace);

        let r = backend.register_bucket("kv2", KV2);
        match r {
            Ok(()) => panic!("T4: register_bucket succeeded under armed ENOSPC"),
            Err(BackendError::Io(io)) => {
                assert_eq!(
                    io.raw_os_error(),
                    Some(ENOSPC),
                    "T4: expected ENOSPC, got {io:?}",
                );
            }
            Err(BackendError::Corruption(_)) => {} // PreviousIo
            Err(e) => panic!("T4: unexpected register_bucket error: {e}"),
        }

        let _ = backend.close();
    }

    // 4. Reopen → P3 not Corruption.
    let backend = assert_reopen_ok(path, "T4 post-ENOSPC reopen");
    // 5. P1: "kv" survives with its prior key.
    let snap = backend.snapshot().expect("snap post-reopen");
    assert_equals(
        &snap,
        KV,
        b"kpre",
        b"vpre",
        "T4 post-reopen KV survives (P1)",
    );
    drop(snap);
    // 6. P3: register_bucket("kv2", KV2) succeeds; commit a put to KV2.
    backend
        .register_bucket("kv2", KV2)
        .expect("T4 post-reopen register_bucket must succeed");
    let mut batch = backend.begin_batch().expect("begin_batch post-reopen");
    batch.put(KV2, b"k2_post", b"v2_post").expect("put");
    let _ = backend
        .commit_batch(batch, true)
        .await
        .expect("T4 post-reopen commit must succeed");
}
