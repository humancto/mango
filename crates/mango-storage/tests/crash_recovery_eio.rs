//! Crash-recovery test under simulated fsync EIO (ROADMAP:826).
//!
//! Proves [`mango_storage::RedbBackend`] reports failure
//! ([`BackendError::Io`] with `raw_os_error() == Some(libc::EIO)`,
//! or [`BackendError::Corruption`] on subsequent attempts) when
//! the underlying fsync syscall returns `EIO`, and that on reopen
//! no torn state is visible.
//!
//! The roadmap contract verbatim: *"backend either commits cleanly
//! or reports failure; no silent data loss."*
//!
//! ## Mechanism
//!
//! `LD_PRELOAD` shim at `tests/fixtures/eio_inject.c` overrides
//! `fsync(2)` and `fdatasync(2)` to fail with `EIO` when
//! `MANGO_TEST_INJECT_FSYNC_EIO=1` is set at child process start.
//! Same child-process re-exec idiom as ROADMAP:825
//! (`tests/crash_recovery_panic.rs`).
//!
//! The shim is compiled at parent test runtime via the system `cc`
//! compiler — no new Rust crate dependencies. Honors `${CC:-cc}`
//! for cross-compilation / sccache / Nix shells.
//!
//! ## Linux fsync chain (verified)
//!
//! 1. `std::fs::File::sync_data()` on Linux compiles to
//!    `libc::fdatasync(fd)`. Verified at
//!    `library/std/src/sys/fs/unix.rs:1262` (rustc 1.89). No
//!    fallback to `fsync` on Linux.
//! 2. redb 4.1.0 calls `File::sync_data()` exclusively on its
//!    commit path. Verified at
//!    `tree_store/page_store/file_backend/optimized.rs:88` and
//!    `cached_file.rs:181/402`. No `sync_all`, `sync_file_range`,
//!    `msync`, `O_DIRECT|O_DSYNC`, or `pwritev2(RWF_DSYNC)`.
//! 3. redb's `optimized.rs` backend is selected on Linux (the
//!    `#[cfg(any(windows, unix, target_os = "wasi"))]` gate at
//!    `file_backend/mod.rs:1`).
//!
//! Therefore, on Linux, intercepting `fdatasync` is what makes
//! the test work. Intercepting `fsync` is belt-and-braces (cheap,
//! harmless, may catch redb's `Drop`-time `sync_all` on some FS
//! configs) but is NOT load-bearing here.
//!
//! ## Why Linux-only
//!
//! macOS `File::sync_data()` calls `fcntl(fd, F_FULLFSYNC, 0)`,
//! which is variadic and architecturally awkward to intercept via
//! `dlsym` (you'd have to know the cmd → arg-shape mapping for
//! every cmd you forward, or use `va_arg` and hope the calling
//! convention is stable). macOS is not the production target;
//! this test is gated `#[cfg(target_os = "linux")]`. If the
//! workspace ever ships on macOS-as-server, intercept
//! `fcntl(F_FULLFSYNC)` here.
//!
//! ## What this test does NOT prove
//!
//! - Raft-driven retry-after-EIO behavior (Phase 3 territory).
//! - Recovery-time bounds (ROADMAP:822).
//! - That `commit_seq` does not advance on a failed commit. The
//!   `commit_seq` semantics on commit failure are tested by
//!   ordinary commit-error tests in `redb_backend.rs` and are
//!   orthogonal to fsync injection.
//! - Panic during commit future (deferred — tokio cancellation
//!   injection is its own item).
//! - SIGKILL variant (deferred to ROADMAP:820).
//!
//! ## Failure attribution
//!
//! Unlike 825 (where SIGABRT signal is the success indicator), the
//! child here exits cleanly (code `0`) on contract-met. The
//! child's *internal* assertion is what proves contract-met; if
//! the child's `commit_batch` returns `Ok` under injected EIO,
//! the child panics → exit code `101`.
//!
//! The shim's constructor emits `"eio_inject: armed\n"` to stderr
//! when injection is active. Parent surfaces this on failure: a
//! missing canary line means the `LD_PRELOAD` was sandbox-stripped
//! or the path was wrong, NOT that the contract was violated.
//!
//! ## CI wiring
//!
//! Tests are `#[ignore]`-gated by default — they spawn child
//! processes and require `cc` on PATH. CI runs them via
//! `cargo nextest run --run-ignored all` in `.github/workflows/ci.yml`
//! (the same step covers ROADMAP:825's `crash_recovery_panic`).
//!
//! ## Miri
//!
//! Miri is NOT run on this test. It spawns subprocesses and uses
//! `LD_PRELOAD`; Miri's interpreter cannot model that.

#![cfg(not(madsim))]
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use std::ffi::OsStr;
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use bytes::Bytes;
use mango_storage::{
    Backend, BackendConfig, BackendError, BucketId, ReadSnapshot, RedbBackend, WriteBatch as _,
};
use tempfile::TempDir;

const KV: BucketId = BucketId::new(1);
const ENV_SCENARIO: &str = "MANGO_TEST_CRASH_RECOVERY_EIO_SCENARIO";
const ENV_PATH: &str = "MANGO_TEST_CRASH_RECOVERY_EIO_PATH";
const ENV_INJECT: &str = "MANGO_TEST_INJECT_FSYNC_EIO";
const ENV_LD_PRELOAD: &str = "LD_PRELOAD";
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

/// Linux EIO. Hard-coded because the test is `#[cfg(target_os =
/// "linux")]`-gated and Linux EIO is `5` per `<errno.h>`. We do
/// not pull in `libc` just for this constant.
const LINUX_EIO: i32 = 5;

/// Canary line the LD_PRELOAD shim's constructor emits to stderr
/// when injection is armed. Parent surfaces this on failure.
const SHIM_ARMED_CANARY: &str = "eio_inject: armed";

fn key_at(i: u32) -> String {
    format!("k{i:03}")
}

fn val_at(i: u32) -> String {
    format!("v{i:03}")
}

fn child_role() -> Option<(String, PathBuf)> {
    let scenario = std::env::var(ENV_SCENARIO).ok()?;
    let path: PathBuf = std::env::var(ENV_PATH).ok()?.into();
    Some((scenario, path))
}

/// Compile the LD_PRELOAD shim once at parent test start.
///
/// Honors `${CC:-cc}`. `-ldl` goes AFTER the source file because
/// GNU `ld` defaults to `--as-needed` on modern Ubuntu and would
/// otherwise drop the library before the symbol-needing source is
/// linked.
///
/// The shim's path includes the parent PID so concurrent runs
/// don't collide. We do NOT use `tempfile::TempDir` because the
/// parent passes the path to a child via env; TempDir cleanup on
/// parent panic could yank the .so during child load.
fn build_shim() -> PathBuf {
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/eio_inject.c");
    let out_dir = std::env::temp_dir().join(format!("mango-eio-shim-{}", std::process::id()));
    std::fs::create_dir_all(&out_dir).expect("create shim out dir");
    let lib = out_dir.join("libeio_inject.so");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let status = Command::new(&cc)
        .args(["-shared", "-fPIC", "-o"])
        .arg(&lib)
        .arg(src)
        .arg("-ldl")
        .status()
        .expect("invoke cc to build LD_PRELOAD shim");
    assert!(status.success(), "cc {cc} failed to build {src}");
    lib
}

/// Spawn the libtest binary as a child re-exec. Strips
/// `MANGO_TEST_INJECT_FSYNC_EIO` and `LD_PRELOAD` from the
/// inherited env by default — only the injection-scenario children
/// re-add them. Without this, env leakage from prior tests or a CI
/// orchestrator could poison a baseline child.
///
/// `--include-ignored` is required because the test fns are
/// `#[ignore]`-gated; without it the child re-exec would filter
/// them out and exit with code 0. `--exact` keeps the libtest
/// filter to a single test fn (no fork-bomb via substring match).
fn spawn_child(
    test_name: &str,
    scenario: &str,
    db_path: &Path,
    extra_env: &[(&str, &OsStr)],
) -> std::process::Output {
    let mut cmd =
        Command::new(std::env::current_exe().expect("current_exe should resolve in test"));
    cmd.arg(test_name)
        .arg("--exact")
        .arg("--include-ignored")
        .env(ENV_SCENARIO, scenario)
        .env(ENV_PATH, db_path)
        .env_remove(ENV_INJECT)
        .env_remove(ENV_LD_PRELOAD)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn child test binary")
}

/// Assert the child exited cleanly (code 0). On any other outcome,
/// dump captured stderr/stdout for attribution. If the canary line
/// is absent in stderr when injection was meant to be armed, the
/// failure message names that explicitly so the engineer doesn't
/// chase a bug that's actually a sandbox-stripped LD_PRELOAD.
fn assert_clean_exit(out: &std::process::Output, ctx: &str, expect_armed_canary: bool) {
    let code = out.status.code();
    let signal = out.status.signal();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let canary_seen = stderr.contains(SHIM_ARMED_CANARY);

    if code != Some(0) {
        let canary_diag = if expect_armed_canary && !canary_seen {
            "\nDIAGNOSTIC: 'eio_inject: armed' canary MISSING from stderr — \
             the LD_PRELOAD shim did not load (sandbox-stripped? wrong path?). \
             The child's commit therefore did NOT see injected EIO."
        } else {
            ""
        };
        panic!(
            "child '{ctx}' exited non-zero: code={:?} signal={:?}{canary_diag}\n\
             ---- child stderr ----\n{stderr}\n\
             ---- child stdout ----\n{stdout}\n",
            code, signal,
        );
    }

    if expect_armed_canary && !canary_seen {
        panic!(
            "child '{ctx}' exited 0 but canary 'eio_inject: armed' is MISSING from stderr. \
             The shim did not load with injection active — the test result is therefore \
             not attributable to the EIO contract.\n\
             ---- child stderr ----\n{stderr}\n\
             ---- child stdout ----\n{stdout}\n",
        );
    }
}

/// Hand-rolled match used in every child to assert that a commit
/// under injected EIO returned the right error. Splits Ok vs
/// Err(other) so a panic message names the actual variant rather
/// than the empty `assertion failed: matches!(...)` from
/// `assert!(matches!(...))`.
fn assert_commit_failed_with_eio(result: &Result<mango_storage::CommitStamp, BackendError>) {
    match result {
        Err(BackendError::Io(e)) => {
            // The shim sets errno=EIO before returning -1; redb
            // wraps it as `io::Error::from_raw_os_error(EIO)` via
            // `StorageError::Io`. raw_os_error MUST round-trip.
            assert_eq!(
                e.raw_os_error(),
                Some(LINUX_EIO),
                "BUG: expected raw_os_error EIO={LINUX_EIO}; got io::Error: {e:?}",
            );
        }
        Err(BackendError::Corruption(_)) => {
            // `redb::StorageError::PreviousIo` path. Acceptable —
            // both Io and Corruption are explicit failure-reports
            // and satisfy the ROADMAP:826 contract.
        }
        Ok(stamp) => panic!(
            "BUG: commit succeeded under injected EIO: stamp={stamp:?} \
             — the LD_PRELOAD shim was probably not loaded (no canary in stderr?), \
             or redb stopped calling fdatasync on commit"
        ),
        Err(other) => panic!(
            "BUG: expected Err(Io|Corruption); got {other:?} — the contract \
             is 'report failure', but the failure must be the right kind"
        ),
    }
}

/// Force-flush stderr in the child before `exit(0)`. The shim
/// writes the canary via `fputs`, which on glibc goes through
/// stdio buffering; without a flush, a process exit can race the
/// flush and the parent sees an empty stderr.
fn flush_stderr() {
    let _ = std::io::stderr().flush();
}

// --- T1 -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns child process; requires cc + Linux. Gated behind --ignored. See module doc."]
async fn commit_under_eio_reports_failure_and_leaves_no_torn_state() {
    if let Some((scenario, path)) = child_role() {
        let fut = async move {
            assert_eq!(scenario, "t1-eio-first");
            let backend =
                RedbBackend::open(BackendConfig::new(path.clone(), false)).expect("child open");
            backend.register_bucket("kv", KV).expect("register kv");
            let mut batch = backend.begin_batch().expect("begin_batch");
            batch.put(KV, b"k_eio", b"v_eio").expect("put");
            let result = backend.commit_batch(batch, true).await;
            assert_commit_failed_with_eio(&result);
            flush_stderr();
            std::process::exit(0);
        };
        tokio::time::timeout(CHILD_TIMEOUT, fut)
            .await
            .expect("child timed out before exit — re-exec wedge?");
        unreachable!("child fut must exit(0) or time out");
    }

    // Parent role.
    let dir = TempDir::new().expect("tempdir");
    let shim = build_shim();
    let shim_os = shim.as_os_str();
    let one = OsStr::new("1");
    let extra_env: &[(&str, &OsStr)] = &[(ENV_LD_PRELOAD, shim_os), (ENV_INJECT, one)];

    let out = spawn_child(
        "commit_under_eio_reports_failure_and_leaves_no_torn_state",
        "t1-eio-first",
        dir.path(),
        extra_env,
    );
    assert_clean_exit(&out, "t1-eio-first", true);

    // Reopen WITHOUT injection. `read_only=false`: a "child silently
    // failed to create the DB" failure surfaces as redb's open
    // returning an error (no file to open), not as a silent
    // re-create.
    let backend = RedbBackend::open(BackendConfig::new(dir.path().to_path_buf(), false))
        .expect("BUG: redb failed to recover from EIO-failed commit");

    // Registry probe via snapshot — DO NOT re-register the bucket.
    // Re-registering would mask a registry-loss bug by silently
    // re-creating the binding.
    let snap = backend.snapshot().expect("snapshot");
    match snap.get(KV, b"k_eio") {
        Ok(None) => {} // contract met: no torn state
        Ok(Some(v)) => {
            panic!("k_eio is present after EIO-failed commit — torn state on disk: {v:?}",)
        }
        Err(BackendError::UnknownBucket(_)) => {
            // Acceptable in principle (bucket creation rolled back
            // alongside the failed commit), but worth surfacing
            // explicitly so a future redb behavior-change is loud.
            // Fall through — no k_eio means contract met.
        }
        Err(e) => panic!("snapshot probe failed: {e}"),
    }
}

// --- T2 -------------------------------------------------------------

const T2_BASELINE: u32 = 10;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns child process; requires cc + Linux. Gated behind --ignored. See module doc."]
async fn eio_after_successful_commits_preserves_them() {
    if let Some((scenario, path)) = child_role() {
        let fut = async move {
            match scenario.as_str() {
                "t2-baseline" => {
                    let backend = RedbBackend::open(BackendConfig::new(path.clone(), false))
                        .expect("child A open");
                    backend.register_bucket("kv", KV).expect("register kv");
                    for i in 0..T2_BASELINE {
                        let mut batch = backend.begin_batch().expect("begin_batch");
                        batch
                            .put(KV, key_at(i).as_bytes(), val_at(i).as_bytes())
                            .expect("put");
                        backend
                            .commit_batch(batch, true)
                            .await
                            .expect("commit_batch");
                    }
                    flush_stderr();
                    std::process::exit(0);
                }
                "t2-eio-after" => {
                    let backend = RedbBackend::open(BackendConfig::new(path.clone(), false))
                        .expect("child B open");
                    // Bucket already registered by child A on this
                    // path — re-registering is idempotent in
                    // RedbBackend, but we don't need to.
                    backend
                        .register_bucket("kv", KV)
                        .expect("register kv (idempotent)");
                    let mut batch = backend.begin_batch().expect("begin_batch");
                    batch.put(KV, b"k_eio", b"v_eio").expect("put");
                    let result = backend.commit_batch(batch, true).await;
                    assert_commit_failed_with_eio(&result);
                    flush_stderr();
                    std::process::exit(0);
                }
                other => panic!("unknown scenario: {other}"),
            }
        };
        tokio::time::timeout(CHILD_TIMEOUT, fut)
            .await
            .expect("child timed out before exit — re-exec wedge?");
        unreachable!("child fut must exit(0) or time out");
    }

    // Parent role.
    let dir = TempDir::new().expect("tempdir");
    let shim = build_shim();

    // Child A: 10 successful commits, no injection.
    let out_a = spawn_child(
        "eio_after_successful_commits_preserves_them",
        "t2-baseline",
        dir.path(),
        &[],
    );
    assert_clean_exit(&out_a, "t2-baseline", false);

    // Child B: attempt 1 commit under EIO injection.
    let shim_os = shim.as_os_str();
    let one = OsStr::new("1");
    let out_b = spawn_child(
        "eio_after_successful_commits_preserves_them",
        "t2-eio-after",
        dir.path(),
        &[(ENV_LD_PRELOAD, shim_os), (ENV_INJECT, one)],
    );
    assert_clean_exit(&out_b, "t2-eio-after", true);

    // Reopen without injection.
    let backend = RedbBackend::open(BackendConfig::new(dir.path().to_path_buf(), false))
        .expect("BUG: reopen after EIO commit failed");
    let snap = backend.snapshot().expect("snapshot");

    // (1) The 10 baseline commits survived.
    for i in 0..T2_BASELINE {
        assert_eq!(
            snap.get(KV, key_at(i).as_bytes()).expect("snap.get"),
            Some(Bytes::copy_from_slice(val_at(i).as_bytes())),
            "baseline key {} missing after EIO commit",
            key_at(i),
        );
    }
    // (2) The EIO commit left no on-disk trace.
    assert_eq!(
        snap.get(KV, b"k_eio").expect("snap.get"),
        None,
        "EIO commit's k_eio is visible — torn state on disk",
    );
}

// --- T3 -------------------------------------------------------------
//
// `RedbBackend::commit_group` flattens all batches into ONE redb
// `WriteTransaction` (`crates/mango-storage/src/redb/mod.rs:469`).
// T3 verifies that all-or-none atomicity is preserved across that
// single transaction's fsync failure — NOT multi-stage 2PC. If
// the impl ever changes to multi-stage commit, this test will
// need to grow.

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns child process; requires cc + Linux. Gated behind --ignored. See module doc."]
async fn eio_during_commit_group_is_atomic() {
    if let Some((scenario, path)) = child_role() {
        let fut = async move {
            assert_eq!(scenario, "t3-eio-group");
            let backend =
                RedbBackend::open(BackendConfig::new(path.clone(), false)).expect("child open");
            backend.register_bucket("kv", KV).expect("register kv");

            let mut b1 = backend.begin_batch().expect("begin_batch b1");
            b1.put(KV, b"k1", b"v1").expect("put k1");
            let mut b2 = backend.begin_batch().expect("begin_batch b2");
            b2.put(KV, b"k2", b"v2").expect("put k2");
            let mut b3 = backend.begin_batch().expect("begin_batch b3");
            b3.put(KV, b"k3", b"v3").expect("put k3");

            let result = backend.commit_group(vec![b1, b2, b3]).await;
            assert_commit_failed_with_eio(&result);
            flush_stderr();
            std::process::exit(0);
        };
        tokio::time::timeout(CHILD_TIMEOUT, fut)
            .await
            .expect("child timed out before exit — re-exec wedge?");
        unreachable!("child fut must exit(0) or time out");
    }

    // Parent role.
    let dir = TempDir::new().expect("tempdir");
    let shim = build_shim();
    let shim_os = shim.as_os_str();
    let one = OsStr::new("1");

    let out = spawn_child(
        "eio_during_commit_group_is_atomic",
        "t3-eio-group",
        dir.path(),
        &[(ENV_LD_PRELOAD, shim_os), (ENV_INJECT, one)],
    );
    assert_clean_exit(&out, "t3-eio-group", true);

    let backend = RedbBackend::open(BackendConfig::new(dir.path().to_path_buf(), false))
        .expect("BUG: reopen after EIO commit_group failed");
    let snap = backend.snapshot().expect("snapshot");

    // All-or-none: none of the three keys should be visible.
    for k in [&b"k1"[..], b"k2", b"k3"] {
        match snap.get(KV, k) {
            Ok(None) => {}
            Ok(Some(v)) => panic!("commit_group atomicity violated: {k:?} is visible: {v:?}",),
            Err(BackendError::UnknownBucket(_)) => {} // bucket-creation rolled back; acceptable
            Err(e) => panic!("snapshot probe failed: {e}"),
        }
    }
}
