//! Crash-recovery test for [`mango_storage::RaftEngineLogStore`].
//!
//! This exercises the durability story the whole Raft-log surface
//! depends on: a `sync=true` write must survive a SIGKILL of the
//! writer process. A unit test can't cover this — the engine has to
//! actually be running in a separate process that the OS tears down
//! without a chance to run `Drop` or flush buffers.
//!
//! Pattern: the test binary re-executes itself with
//! `--ignored --exact <worker-name> --nocapture`. The child opens a
//! store under a path the parent passes via env var, writes 10
//! entries + `hard_state` with `sync=true`, prints a marker to stdout,
//! then sleeps forever. The parent reads the marker, sends SIGKILL,
//! reopens the same path, and asserts the fsynced data survived.
//!
//! `#[cfg(unix)]` gates the whole file — SIGKILL semantics and
//! `from_raw_fd` / signal numbers are Unix-only. On other platforms
//! the file compiles to nothing.
//!
//! Under `--cfg madsim` this file is excluded — madsim does not
//! emulate `fork` / `kill` semantics.

#![cfg(all(unix, not(madsim)))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::items_after_statements,
    clippy::print_stdout
)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use mango_storage::{HardState, RaftEngineConfig, RaftEngineLogStore, RaftLogStore};

const CHILD_DIR_ENV: &str = "MANGO_RAFT_CRASH_CHILD_DIR";
const WRITES_DONE_MARKER: &str = "WRITES-DONE";

/// Worker function that runs in the spawned child process. Writes
/// data with `sync=true` then blocks forever so the parent can
/// SIGKILL it mid-process (the Raft log state was already fsynced
/// before the sleep).
///
/// Marked `#[ignore]` so `cargo test` won't run it directly; the
/// parent test invokes it via `--ignored --exact`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "spawned by parent sigkill_after_append_preserves_fsynced_entries; do not run directly"]
async fn crash_child_worker() {
    let dir = std::env::var(CHILD_DIR_ENV).expect("child must receive target dir via env var");
    let store = RaftEngineLogStore::open(RaftEngineConfig::new(dir.into())).expect("child open");

    // 10 entries written with sync=true (the default for append in
    // this impl — see `engine.write(&mut batch, /*sync=*/ true)`).
    let batch: Vec<_> = (1_u64..=10)
        .map(|i| {
            mango_storage::RaftEntry::new(
                i,
                1,
                mango_storage::RaftEntryType::Normal,
                bytes::Bytes::copy_from_slice(&i.to_le_bytes()),
                bytes::Bytes::new(),
            )
        })
        .collect();
    store.append(&batch).await.expect("child append");

    // Hard state, also sync=true.
    let hs = HardState::new(5, 7, 10);
    store.save_hard_state(&hs).await.expect("child save_hs");

    // Signal to the parent — MUST be printed AFTER all fsynced
    // writes have returned. Flush explicitly so the line leaves the
    // child's stdio buffer before we sleep.
    println!("{WRITES_DONE_MARKER}");
    std::io::stdout().flush().expect("flush");

    // Park forever. The parent sends SIGKILL; `close()` is NEVER
    // called here. That's the whole point of the test — we need the
    // OS to tear down the engine without running `Drop`, simulating
    // a real crash.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Parent-side test: spawn the worker, wait for its "done" marker,
/// SIGKILL it, then reopen the same directory and assert every
/// `sync=true` write survived.
#[test]
fn sigkill_after_append_preserves_fsynced_entries() {
    // tempfile::TempDir is fine here — the *parent* owns the
    // directory; the child only writes into it, no lifetime issue.
    let tmp = tempfile::TempDir::new().expect("tempdir");

    // Re-exec ourselves filtered to the worker test.
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(&exe)
        .args(["--ignored", "--exact", "--nocapture", "crash_child_worker"])
        .env(CHILD_DIR_ENV, tmp.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn child");

    // Wait for the "writes done" marker on stdout.
    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);
    let mut saw_marker = false;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).expect("read child stdout");
        if n == 0 {
            break; // EOF — child exited before printing marker.
        }
        if line.trim() == WRITES_DONE_MARKER {
            saw_marker = true;
            break;
        }
    }
    assert!(saw_marker, "child did not print {WRITES_DONE_MARKER}");

    // SIGKILL. `std::process::Child::kill` is documented to perform
    // `kill(2)` with `SIGKILL` on Unix (stdlib docs). No cleanup, no
    // `Drop`, no graceful close — which is the whole point of the
    // test.
    child.kill().expect("kill child");
    let _ = child.wait().expect("wait child");

    // Reopen the same path.
    let store = RaftEngineLogStore::open(RaftEngineConfig::new(tmp.path().to_path_buf()))
        .expect("parent reopen");

    // sync=true on every write means no data loss. The engine may
    // permit a trailing partial batch on `TolerateTailCorruption`
    // recovery, but since our last fsynced write completed before
    // the marker print, last_index MUST be at least 10.
    assert!(
        store.last_index().expect("last_index") >= 10,
        "lost fsynced entries: last_index={}",
        store.last_index().unwrap()
    );
    assert_eq!(store.hard_state().expect("hs"), HardState::new(5, 7, 10));

    store.close().expect("parent close");
}
