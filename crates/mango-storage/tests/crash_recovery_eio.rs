//! ROADMAP:826 — fsync-EIO crash-recovery test.
//!
//! Wraps [`redb::backends::FileBackend`] in an arm-gated wrapper that
//! returns EIO from `sync_data()` when armed. Pure in-process — no
//! `LD_PRELOAD`, no child re-exec, runs on macOS and Linux equivalently.
//!
//! # ROADMAP:826 reading
//!
//! "Backend either commits cleanly or reports failure; no silent data
//! loss." This test pins **two** verifiable properties:
//!
//! - **P1 (no silent data loss for acknowledged commits)** — every
//!   commit that returned `Ok(_)` *before* an EIO is still visible
//!   after a production-path reopen.
//! - **P2 (failures are surfaced)** — a commit that hits an EIO from
//!   `sync_data` returns `Err(_)` and never silently appears as
//!   successful to the caller.
//!
//! The test does **not** assert that the bytes from a *failed* commit
//! are absent from the on-disk file after reopen. That is a stronger
//! property requiring write-buffering at the wrapper level (real
//! `FileBackend::write` calls go through to the OS page cache, which
//! the kernel flushes on its own schedule and on subsequent successful
//! `sync_data` calls — including the four that `Database::Drop`
//! issues). The `Backend` trait's contract is that a failed commit
//! returns `Err` so the user knows not to rely on that data — *not*
//! that the bytes never reach the disk. The test enforces what the
//! contract actually says.
//!
//! # Why a custom `StorageBackend` instead of `LD_PRELOAD`
//!
//! The first attempt (closed PR #60) intercepted `fdatasync(2)` via
//! `LD_PRELOAD`. redb's `Database::new` calls
//! `mem.begin_writable()` → `storage.flush()` *unconditionally* on
//! every open — including when reopening an already-clean on-disk
//! database. With "always-EIO" arming at the syscall level, the
//! injection child died at `RedbBackend::open()` itself, before any
//! commit path ran.
//!
//! Per-test arming via [`AtomicBool`] sidesteps this: the wrapper
//! is constructed disarmed, the test passes through the open-time
//! flush cleanly, and the test arms the wrapper only at the precise
//! moment the EIO is wanted.
//!
//! # Out of scope
//!
//! - `set_len`/`write` EIO injection. ROADMAP:826 names the durability
//!   boundary (`sync_data`); file-grow / write-time EIO is tracked as
//!   future fault-injection work.
//! - Read-path EIO injection. Separate concern.
//! - Countdown-style injection. Single-shot is sufficient for
//!   ROADMAP:826's "either commits cleanly or reports failure" bar.
//!
//! # Implementation notes
//!
//! 1. **Two-phase commit means two `sync_data` calls per
//!    `WriteTransaction::commit()`** (`redb` 4.1
//!    `page_manager.rs:626` + `:636`). Always-armed injection therefore
//!    fails at the first; the second never runs. A future
//!    countdown-style wrapper would need to account for this.
//!
//! 2. **`Database::Drop` re-runs `sync_data` up to four times** via
//!    `mem.close()` (`page_manager.rs:1164` + `:1167`) and
//!    `ensure_allocator_state_table_and_trim()` (`db.rs:1063` +
//!    `:1068`). Every test disarms the wrapper *before* dropping the
//!    backend so drop-time fsync passes through cleanly. Without that
//!    discipline the EIOs at drop are silently swallowed (return code
//!    is ignored in `Database::Drop`), but failure attribution becomes
//!    confusing and lock release timing on macOS gets murky.
//!
//! 3. **File lock semantics across reopen**: each test does
//!    open(injection) → close+drop → open(production). On Linux/macOS,
//!    the `flock(2)` advisory lock taken by `FileBackend::new` is
//!    released at *fd close* by the kernel — `File::Drop` is sufficient
//!    even when `FileBackend::close()` short-circuits on EIO from
//!    `mem.close`. We rely on this.
//!
//! 4. **`PreviousIo` poisoning**: after the first injected EIO from
//!    `sync_data`, redb sets an in-memory `needs_recovery` flag on
//!    *that* `Database` instance. Subsequent commits return
//!    [`BackendError::Corruption`] (`PreviousIo` path), not
//!    [`BackendError::Io`]. The poison flag is in-memory only —
//!    a fresh `Database::create` (from a production-path reopen)
//!    has it clear and either repairs the file or finds clean state.
//!
//! 5. **Memory ordering**: the `inject_eio` flag is shared between
//!    the test thread (which `store(true, Release)`s) and the
//!    `tokio::task::spawn_blocking` worker that runs the redb commit
//!    (which `load(Acquire)`s in the wrapper's `sync_data`). The
//!    Release/Acquire pair establishes happens-before so the load on
//!    the worker sees the flip without UB.

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

/// Linux/macOS errno for I/O error. Both platforms use 5.
const EIO: i32 = 5;

/// Bucket id used by every test. Matches the convention in
/// `crash_recovery_panic.rs`.
const KV: BucketId = BucketId::new(1);
const KV2: BucketId = BucketId::new(2);

/// File name redb writes inside `BackendConfig::data_dir`. Mirrors
/// `crates/mango-storage/src/redb/mod.rs:77`'s `DB_FILENAME`. Kept
/// in sync manually — if that constant ever changes, this test
/// breaks visibly at the file path.
const DB_FILENAME: &str = "mango.redb";

// --- The injection wrapper ------------------------------------------

/// Wraps [`FileBackend`] and returns [`EIO`] from
/// [`StorageBackend::sync_data`] when armed. All other methods
/// delegate verbatim. Pattern taken from redb's own test suite
/// (`FailingBackend` in `redb-4.1.0/src/db.rs:1276`).
#[derive(Debug)]
struct EioOnSyncBackend {
    inner: FileBackend,
    inject_eio: Arc<AtomicBool>,
}

impl EioOnSyncBackend {
    fn new(file: std::fs::File, inject_eio: Arc<AtomicBool>) -> Self {
        Self {
            inner: FileBackend::new(file).expect("file lock conflict in test fixture"),
            inject_eio,
        }
    }
}

impl StorageBackend for EioOnSyncBackend {
    fn len(&self) -> Result<u64, Error> {
        self.inner.len()
    }
    fn read(&self, off: u64, out: &mut [u8]) -> Result<(), Error> {
        self.inner.read(off, out)
    }
    fn set_len(&self, len: u64) -> Result<(), Error> {
        self.inner.set_len(len)
    }
    fn write(&self, off: u64, data: &[u8]) -> Result<(), Error> {
        self.inner.write(off, data)
    }
    fn close(&self) -> Result<(), Error> {
        self.inner.close()
    }
    fn sync_data(&self) -> Result<(), Error> {
        // Acquire pairs with `inject_eio.store(true, Release)` in the
        // test bodies. The Release happens-before the load that
        // observes `true`, so this load sees the flip without UB even
        // when the redb commit runs on a `spawn_blocking` worker
        // thread.
        if self.inject_eio.load(Ordering::Acquire) {
            Err(Error::from_raw_os_error(EIO))
        } else {
            self.inner.sync_data()
        }
    }
}

// --- Helpers --------------------------------------------------------

fn db_file(data_dir: &Path) -> PathBuf {
    data_dir.join(DB_FILENAME)
}

/// Open the backend via the production code path. Used for clean
/// baselines and for the post-EIO reopen assertion.
fn open_production(data_dir: &Path) -> RedbBackend {
    RedbBackend::open(BackendConfig::new(data_dir.to_path_buf(), false))
        .expect("production-path open failed")
}

/// Open the backend with the EIO-injection wrapper. The `data_dir`
/// must already contain a clean `mango.redb` (production-path open
/// + close performed earlier in the test).
///
/// `inject_eio = false` at construction so the open-path
/// `mem.begin_writable()` flush passes through to the real
/// `File::sync_data`.
fn open_with_injection(data_dir: &Path, inject_eio: Arc<AtomicBool>) -> RedbBackend {
    let path = db_file(data_dir);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .expect("open db file for injection wrapper");
    let backend = EioOnSyncBackend::new(file, inject_eio);
    RedbBackend::with_backend(backend, path).expect("RedbBackend::with_backend")
}

/// Snapshot-probe expecting a specific value. Panics with a distinct
/// message for every non-matching shape so failure attribution is
/// precise. Mirrors `crash_recovery_panic.rs:201-206`.
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
            "{ctx}: key {key:?} value mismatch after EIO+reopen",
        ),
        Ok(None) => panic!(
            "{ctx}: prior-committed key {key:?} missing after EIO+reopen — \
             this is a silent-data-loss bug",
        ),
        Err(BackendError::UnknownBucket(b)) => {
            panic!("{ctx}: registry lost across EIO+reopen (bucket {b:?} missing)")
        }
        Err(e) => panic!("{ctx}: snapshot probe failed: {e}"),
    }
}

/// Assert the result is `Err(Io(EIO))` *or* `Err(Corruption(_))`
/// (`PreviousIo` path). Both are valid "reported failure" shapes per
/// the trait contract.
fn assert_eio_or_poisoned<T>(r: Result<T, BackendError>, ctx: &str) {
    match r {
        Ok(_) => panic!("{ctx}: commit succeeded under armed EIO injection"),
        Err(BackendError::Io(io)) => assert_eq!(
            io.raw_os_error(),
            Some(EIO),
            "{ctx}: expected EIO, got {io:?}",
        ),
        Err(BackendError::Corruption(_)) => {} // PreviousIo path
        Err(e) => panic!("{ctx}: unexpected error variant: {e}"),
    }
}

// --- T1: commit under EIO reports failure; prior bytes survive ------

#[tokio::test(flavor = "multi_thread")]
async fn t1_commit_under_eio_reports_failure_and_prior_survives() {
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

    // 2. Injection-wrapper open with inject_eio = false so the open-
    //    time `mem.begin_writable()` flush passes through. Capture a
    //    pre-arm commit stamp (M1) so we can assert later that the
    //    failed commit did NOT bump commit_seq.
    // 3. Arm.
    // 4. Stage k_uncommitted, attempt commit → P2: expect Err.
    let inject_eio = Arc::new(AtomicBool::new(false));
    {
        let backend = open_with_injection(path, Arc::clone(&inject_eio));

        let s0 = {
            let batch = backend.begin_batch().expect("begin_batch s0");
            backend
                .commit_batch(batch, true)
                .await
                .expect("pre-arm empty commit (s0)")
        };

        inject_eio.store(true, Ordering::Release);

        let mut batch = backend.begin_batch().expect("begin_batch armed");
        batch
            .put(KV, b"k_uncommitted", b"v_uncommitted")
            .expect("put uncommitted");
        let r = backend.commit_batch(batch, true).await;
        assert_eio_or_poisoned(r, "T1 armed commit");

        // S2: disarm BEFORE drop. `Database::Drop` runs `sync_data`
        // up to four times via mem.close + ensure_allocator_state.
        inject_eio.store(false, Ordering::Release);

        // M1: under PreviousIo poisoning the next commit on this
        // handle may also fail with Corruption; try once and accept
        // either outcome. The load-bearing property is "failed commit
        // didn't bump commit_seq" — only verifiable via a *successful*
        // next stamp, so when poisoning prevents that we just skip
        // the assertion.
        let post_batch = backend.begin_batch().expect("begin_batch post");
        match backend.commit_batch(post_batch, true).await {
            Ok(s1) => assert_eq!(
                s1.seq,
                s0.seq.checked_add(1).expect("seq overflow"),
                "T1: failed commit must not bump commit_seq (got s0={}, s1={})",
                s0.seq,
                s1.seq,
            ),
            Err(BackendError::Corruption(_)) => {
                // Engine is poisoned post-EIO. Property holds (the
                // reopen below verifies prior commits survive).
            }
            Err(e) => panic!("T1 post-arm empty commit unexpected error: {e}"),
        }

        backend.close().expect("close injection wrapper");
    }

    // 5-6. Production-path reopen. P1: prior committed bytes survive.
    let backend = open_production(path);
    let snap = backend.snapshot().expect("snapshot post-reopen");
    assert_equals(&snap, KV, b"k_pre", b"v_pre", "T1 post-reopen prior (P1)");
}

// --- T2: multi-op commit under EIO reports failure; prior commits survive

#[tokio::test(flavor = "multi_thread")]
async fn t2_multi_op_commit_under_eio_reports_failure_and_prior_survives() {
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

    // 2-3. Injection wrapper, verify baseline visible. Arm. Commit a
    //      multi-op batch with 3 puts → P2: expect Err.
    let inject_eio = Arc::new(AtomicBool::new(false));
    {
        let backend = open_with_injection(path, Arc::clone(&inject_eio));

        // Verify baseline.
        let snap = backend.snapshot().expect("snap baseline");
        for i in 0u8..5 {
            let k = [b'k', b'0' + i];
            let v = [b'v', b'0' + i];
            assert_equals(&snap, KV, &k, &v, "T2 baseline visibility");
        }
        drop(snap);

        inject_eio.store(true, Ordering::Release);

        let mut batch = backend.begin_batch().expect("begin_batch armed");
        batch.put(KV, b"k5", b"v5").expect("put k5");
        batch.put(KV, b"k6", b"v6").expect("put k6");
        batch.put(KV, b"k7", b"v7").expect("put k7");
        let r = backend.commit_batch(batch, true).await;
        assert_eio_or_poisoned(r, "T2 armed multi-op commit");

        inject_eio.store(false, Ordering::Release);
        backend.close().expect("close injection");
    }

    // 4-5. Production-path reopen. P1: all 5 prior keys present.
    //      The post-reopen state of k5/k6/k7 is intentionally not
    //      asserted — see module-doc "ROADMAP:826 reading".
    let backend = open_production(path);
    let snap = backend.snapshot().expect("snapshot post-reopen");
    for i in 0u8..5 {
        let k = [b'k', b'0' + i];
        let v = [b'v', b'0' + i];
        assert_equals(&snap, KV, &k, &v, "T2 post-reopen prior (P1)");
    }
}

// --- T3: commit_group under EIO reports failure; seed survives ----

#[tokio::test(flavor = "multi_thread")]
async fn t3_commit_group_under_eio_reports_failure_and_seed_survives() {
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

    // 2-3. Injection wrapper, verify seed visible. Arm. commit_group
    //      with 3 batches each touching a distinct key → P2: expect Err.
    let inject_eio = Arc::new(AtomicBool::new(false));
    {
        let backend = open_with_injection(path, Arc::clone(&inject_eio));

        let snap = backend.snapshot().expect("snap baseline");
        assert_equals(&snap, KV, b"k_pre", b"v_pre", "T3 baseline seed");
        drop(snap);

        inject_eio.store(true, Ordering::Release);

        let mut a = backend.begin_batch().expect("begin_batch a");
        a.put(KV, b"k_a", b"v_a").expect("put k_a");
        let mut b = backend.begin_batch().expect("begin_batch b");
        b.put(KV, b"k_b", b"v_b").expect("put k_b");
        let mut c = backend.begin_batch().expect("begin_batch c");
        c.put(KV, b"k_c", b"v_c").expect("put k_c");
        let r = backend.commit_group(vec![a, b, c]).await;
        assert_eio_or_poisoned(r, "T3 armed commit_group");

        inject_eio.store(false, Ordering::Release);
        backend.close().expect("close injection");
    }

    // 4-5. Production-path reopen. P1: seed survives. The post-reopen
    //      state of k_a/k_b/k_c is intentionally not asserted — see
    //      module-doc "ROADMAP:826 reading".
    let backend = open_production(path);
    let snap = backend.snapshot().expect("snapshot post-reopen");
    assert_equals(&snap, KV, b"k_pre", b"v_pre", "T3 post-reopen seed (P1)");
}

// --- T4: register_bucket under EIO reports failure; existing bucket survives

#[tokio::test(flavor = "multi_thread")]
async fn t4_register_bucket_under_eio_reports_failure_and_existing_survives() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path();

    // 1. Production-path open, register "kv" successfully, commit a
    //    pre-arm key, close.
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

    // 2-3. Injection wrapper. Verify "kv" usable. Arm. Try to
    //      register "kv2" (BucketId 2) → P2: expect Err.
    let inject_eio = Arc::new(AtomicBool::new(false));
    {
        let backend = open_with_injection(path, Arc::clone(&inject_eio));

        // Verify baseline: "kv" exists and "kpre" is visible.
        let snap = backend.snapshot().expect("snap baseline");
        assert_equals(&snap, KV, b"kpre", b"vpre", "T4 baseline visibility");
        drop(snap);

        inject_eio.store(true, Ordering::Release);

        let r = backend.register_bucket("kv2", KV2);
        match r {
            Ok(()) => panic!("T4: register_bucket succeeded under armed EIO"),
            Err(BackendError::Io(io)) => {
                assert_eq!(io.raw_os_error(), Some(EIO), "T4: expected EIO, got {io:?}");
            }
            Err(BackendError::Corruption(_)) => {} // PreviousIo
            Err(e) => panic!("T4: unexpected register_bucket error: {e}"),
        }

        inject_eio.store(false, Ordering::Release);
        backend.close().expect("close injection");
    }

    // 4-5. Production-path reopen. P1: "kv" survives with its prior
    //      key. The post-reopen state of the "kv2" registry slot is
    //      intentionally not asserted — see module-doc
    //      "ROADMAP:826 reading".
    let backend = open_production(path);
    let snap = backend.snapshot().expect("snap post-reopen");
    assert_equals(
        &snap,
        KV,
        b"kpre",
        b"vpre",
        "T4 post-reopen KV survives (P1)",
    );
}
