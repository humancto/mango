//! Crash-recovery test (ROADMAP:825).
//!
//! Proves [`mango_storage::RedbBackend`] survives an in-process
//! [`std::process::abort`] (SIGABRT — no destructors, no atexit, no
//! buffered-stdio flush, no `Drop` for `redb::Database`) and that on
//! reopen:
//!
//! - All previously-committed data is present (T1).
//! - An in-flight uncommitted [`mango_storage::WriteBatch`] left no
//!   on-disk trace (T2).
//!
//! These two assertions are exactly what ROADMAP:825 requires:
//! "no torn state and no committed data lost."
//!
//! Notes for future readers:
//!
//! 1. This test pins the **process-abort survival contract**, not the
//!    full fsync contract. EIO-on-fsync injection (and the
//!    correspondingly-deferred T3 "panic during commit future" and T4
//!    "`commit_group` atomicity across abort during fsync") lives in
//!    ROADMAP:826, which owns syscall-level fault injection.
//!
//! 2. [`mango_storage::Backend::commit_batch`]'s `force_fsync`
//!    parameter is currently a no-op in `RedbBackend`
//!    (`crates/mango-storage/src/redb/mod.rs:442`). The engine fsyncs
//!    every commit anyway. The tests here exercise redb's intrinsic
//!    per-commit fsync, NOT the `force_fsync=true` branch (which has
//!    no extra effect today).
//!
//! 3. `process::abort()` skips destructors entirely — that is the
//!    whole reason we use abort instead of panic. `redb::Database`'s
//!    `Drop` does not run; recovery on reopen is what we are testing.
//!
//! 4. **Stamp continuity across abort is intentionally NOT asserted
//!    here.** `RedbBackend.commit_seq` is an in-memory `AtomicU64`
//!    that resets to `0` on every `open` (see `redb/mod.rs:110`). The
//!    [`mango_storage::CommitStamp`] trait doc does not promise
//!    across-reopen monotonicity — it is "Impl-defined". Asserting
//!    `probe.seq > pre_abort_highest_seq` would therefore test a
//!    non-contract property and fail for a non-bug. If across-reopen
//!    stamp monotonicity becomes a contract claim later (e.g. for
//!    Raft fsync-batching with a Raft-driven persistent stamp), add
//!    the assertion then.
//!
//! Mechanism: child-process re-exec. Each `#[tokio::test]` checks for
//! `MANGO_TEST_CRASH_RECOVERY_SCENARIO`. If set, the test runs as the
//! child (under a 60-second `tokio::time::timeout` watchdog) and
//! `process::abort`s. If unset, it runs as the parent: re-execs the
//! test binary at [`std::env::current_exe`] with the scenario tag and
//! DB path, captures piped stderr, asserts on reopen state.
//!
//! `--exact` + `--include-ignored` are required on the child re-exec:
//! `--include-ignored` because the test fns are `#[ignore]`-gated;
//! `--exact` to keep the libtest filter to a single test fn.
//! Substring fallback would risk a fork-bomb (the re-exec'ing test
//! matching itself plus others).
//!
//! Tests are `#[ignore]` by default — they spawn child processes,
//! which is slow and may be sandbox-blocked. CI runs them via
//! `--run-ignored all` (or `cargo test -- --ignored`).

#![cfg(not(madsim))]
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use bytes::Bytes;
use mango_storage::{
    Backend, BackendConfig, BackendError, BucketId, ReadSnapshot, RedbBackend, WriteBatch as _,
};
use tempfile::TempDir;

const KV: BucketId = BucketId::new(1);
const ENV_SCENARIO: &str = "MANGO_TEST_CRASH_RECOVERY_SCENARIO";
const ENV_PATH: &str = "MANGO_TEST_CRASH_RECOVERY_PATH";
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

// SIGABRT signal number on every Unix the workspace targets. We
// assert positively on this rather than only `code().is_none()` so a
// child killed by a different signal (e.g. SIGSEGV from an unrelated
// bug) fails the test attribution clearly instead of being mistaken
// for a successful abort.
const SIGABRT: i32 = 6;

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

fn spawn_child(test_name: &str, scenario: &str, db_path: &Path) -> std::process::Output {
    // `--include-ignored` is required because the test fns are
    // `#[ignore]`-gated; without it the child re-exec would filter
    // them out and exit with code 0. `--exact` keeps the libtest
    // filter to a single test fn (no substring fallback → no
    // fork-bomb).
    Command::new(std::env::current_exe().expect("current_exe should resolve in test"))
        .arg(test_name)
        .arg("--exact")
        .arg("--include-ignored")
        .env(ENV_SCENARIO, scenario)
        .env(ENV_PATH, db_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn child test binary")
}

fn assert_aborted(out: &std::process::Output, ctx: &str) {
    let signal = out.status.signal();
    if signal != Some(SIGABRT) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        panic!(
            "child '{ctx}' did not abort cleanly: code={:?} signal={:?} (expected SIGABRT={SIGABRT}).\n\
             ---- child stderr ----\n{stderr}\n\
             ---- child stdout ----\n{stdout}\n",
            out.status.code(),
            signal,
        );
    }
}

// --- T1 -------------------------------------------------------------

const T1_BATCHES: u32 = 50;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns child process; gated behind --ignored. See module doc."]
async fn abort_after_committed_batches_preserves_data() {
    if let Some((scenario, path)) = child_role() {
        let fut = async move {
            assert_eq!(scenario, "abort-after-50");
            // `read_only=false`: the child created the DB and is
            // continuing to write to it.
            let backend =
                RedbBackend::open(BackendConfig::new(path.clone(), false)).expect("child open");
            backend.register_bucket("kv", KV).expect("register kv");
            for i in 0..T1_BATCHES {
                let mut batch = backend.begin_batch().expect("begin_batch");
                batch
                    .put(KV, key_at(i).as_bytes(), val_at(i).as_bytes())
                    .expect("put");
                let _ = backend
                    .commit_batch(batch, true)
                    .await
                    .expect("commit_batch");
            }
            std::process::abort();
        };
        tokio::time::timeout(CHILD_TIMEOUT, fut)
            .await
            .expect("child timed out before abort — re-exec wedge?");
        // Unreachable: `fut` ends in `process::abort()`, and
        // `tokio::time::timeout` either yields the inner future's
        // value (impossible here) or panics on expiration.
        unreachable!("child fut must abort or time out");
    }

    // Parent role.
    let dir = TempDir::new().expect("tempdir");
    let out = spawn_child(
        "abort_after_committed_batches_preserves_data",
        "abort-after-50",
        dir.path(),
    );
    assert_aborted(&out, "abort-after-50");

    // `read_only=false`: a "child silently failed to create the DB"
    // failure surfaces as redb's open returning an error (no file to
    // open), not as a silent re-create, because we never ask redb to
    // create.
    let backend = RedbBackend::open(BackendConfig::new(dir.path().to_path_buf(), false))
        .expect("BUG: redb failed to recover from process::abort — this is the bug we're testing");

    // Registry probe — DO NOT re-register the bucket. Re-registering
    // would mask a registry-loss bug by silently re-creating the
    // binding. Snapshot-probe surfaces registry loss as
    // `BackendError::UnknownBucket`.
    let snap = backend.snapshot().expect("snapshot");
    match snap.get(KV, key_at(0).as_bytes()) {
        Ok(Some(_)) => {}
        Ok(None) => panic!("k000 missing — committed data lost across abort"),
        Err(BackendError::UnknownBucket(_)) => panic!("registry lost across abort"),
        Err(e) => panic!("snapshot probe failed: {e}"),
    }

    for i in 0..T1_BATCHES {
        assert_eq!(
            snap.get(KV, key_at(i).as_bytes()).expect("snap.get"),
            Some(Bytes::copy_from_slice(val_at(i).as_bytes())),
            "key {} missing after abort",
            key_at(i),
        );
    }
}

// --- T2 -------------------------------------------------------------

const T2_BATCHES: u32 = 10;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns child process; gated behind --ignored. See module doc."]
async fn abort_in_batch_construction_keeps_prior_commits_intact() {
    if let Some((scenario, path)) = child_role() {
        let fut = async move {
            assert_eq!(scenario, "abort-mid-batch");
            let backend =
                RedbBackend::open(BackendConfig::new(path.clone(), false)).expect("child open");
            backend.register_bucket("kv", KV).expect("register kv");
            for i in 0..T2_BATCHES {
                let mut batch = backend.begin_batch().expect("begin_batch");
                batch
                    .put(KV, key_at(i).as_bytes(), val_at(i).as_bytes())
                    .expect("put");
                let _ = backend
                    .commit_batch(batch, true)
                    .await
                    .expect("commit_batch");
            }
            // The danger zone: an in-flight, uncommitted batch.
            let mut batch = backend.begin_batch().expect("begin_batch (uncommitted)");
            batch
                .put(KV, b"k_uncommitted", b"v_uncommitted")
                .expect("put (uncommitted)");
            std::process::abort();
        };
        tokio::time::timeout(CHILD_TIMEOUT, fut)
            .await
            .expect("child timed out before abort — re-exec wedge?");
        unreachable!("child fut must abort or time out");
    }

    // Parent role.
    let dir = TempDir::new().expect("tempdir");
    let out = spawn_child(
        "abort_in_batch_construction_keeps_prior_commits_intact",
        "abort-mid-batch",
        dir.path(),
    );
    assert_aborted(&out, "abort-mid-batch");

    let backend = RedbBackend::open(BackendConfig::new(dir.path().to_path_buf(), false))
        .expect("BUG: redb failed to recover from process::abort — this is the bug we're testing");

    let snap = backend.snapshot().expect("snapshot");
    match snap.get(KV, key_at(0).as_bytes()) {
        Ok(Some(_)) => {}
        Ok(None) => panic!("k000 missing — committed data lost across abort"),
        Err(BackendError::UnknownBucket(_)) => panic!("registry lost across abort"),
        Err(e) => panic!("snapshot probe failed: {e}"),
    }

    // (1) the 10 baseline commits survived
    for i in 0..T2_BATCHES {
        assert_eq!(
            snap.get(KV, key_at(i).as_bytes()).expect("snap.get"),
            Some(Bytes::copy_from_slice(val_at(i).as_bytes())),
            "committed key {} missing after abort",
            key_at(i),
        );
    }
    // (2) the uncommitted put left no on-disk trace
    assert_eq!(
        snap.get(KV, b"k_uncommitted").expect("snap.get"),
        None,
        "uncommitted batch leaked across abort"
    );
}
