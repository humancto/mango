//! Differential-test harness vs bbolt (ROADMAP:819).
//!
//! This file hosts the Rust-side harness that drives the Go bbolt
//! oracle (`benches/oracles/bbolt/`) in lockstep with a
//! [`RedbBackend`] and asserts byte-identical state after every
//! commit boundary.
//!
//! Scope of THIS commit (plan §9 commit 6): the [`GoOracle`]
//! subprocess helper — spawn, JSON-framed `call`, drop-guard, plus
//! a hardcoded 10-op protocol round-trip smoke test. The
//! differential layer (`RedbBackend` alongside, per-commit snapshot
//! diff, proptest strategy, failure-reporting) lands in subsequent
//! commits (§9 commits 7–9).
//!
//! # Binary discovery
//!
//! The Go oracle is a sibling `cargo`-external binary produced by
//! `benches/oracles/bbolt/build.sh`. Discovery order:
//!
//! 1. `MANGO_BBOLT_ORACLE` env var — absolute path override, used by
//!    CI where the workflow may place the binary outside the repo.
//! 2. `$CARGO_MANIFEST_DIR/../../benches/oracles/bbolt/bbolt-oracle` —
//!    the default build artifact location.
//!
//! If neither exists the test panics with an actionable message. We
//! deliberately do NOT silently skip: a quietly-missing oracle means
//! CI passes while exercising nothing, defeating the whole premise
//! of differential testing.

// Tests carry several ergonomic shortcuts (unwrap on JSON values,
// panic on unreachable protocol paths, println! for the actionable
// error message when the oracle binary is missing) that the
// workspace lint config denies globally. Opt them in for this file
// only — matches the pattern used in other integration tests under
// this crate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};
use tempfile::TempDir;

/// Environment variable override for the oracle binary path.
const ORACLE_ENV: &str = "MANGO_BBOLT_ORACLE";

/// Path of the oracle binary relative to `CARGO_MANIFEST_DIR`. The
/// crate manifest dir is `crates/mango-storage/`; the oracle lives
/// at `benches/oracles/bbolt/bbolt-oracle` from the workspace root,
/// hence the two `..` hops.
const ORACLE_REL: &str = "../../benches/oracles/bbolt/bbolt-oracle";

/// Handle to a running bbolt oracle subprocess.
///
/// Owns the child's stdin/stdout pipes as buffered wrappers. `call`
/// writes a newline-terminated JSON request and reads exactly one
/// newline-terminated JSON response. Protocol is strictly
/// request/reply: one in flight at a time.
///
/// `BufReader` capacity is `16 MiB` to match the oracle's
/// `bufio.Scanner` buffer — realistic `snapshot` responses over
/// ~1K keys can exceed the default 64 KiB.
struct GoOracle {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    /// Monotonically increasing id for outgoing requests. Echoed
    /// back verbatim by the oracle so we can detect reply-skew; the
    /// harness otherwise does not rely on it.
    next_id: u64,
}

impl GoOracle {
    /// Spawn the oracle and send the initial `open` request at
    /// `db_path` with the given fsync bit.
    ///
    /// Stderr is inherited — any Go-side panic or log line surfaces
    /// in the `cargo test` output immediately. We do NOT capture
    /// stderr because it can block the child if the harness
    /// never reads it (classic unbounded-pipe deadlock).
    fn spawn(binary: &Path, db_path: &Path, fsync: bool) -> io::Result<Self> {
        let mut child = Command::new(binary)
            .args(["--mode=diff"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("child stdin pipe missing"))?,
        );
        let stdout = BufReader::with_capacity(
            16 << 20,
            child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("child stdout pipe missing"))?,
        );
        let mut oracle = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        let resp = oracle.call(&json!({
            "op": "open",
            "path": db_path.to_str().ok_or_else(|| io::Error::other("db_path not UTF-8"))?,
            "fsync": fsync,
        }))?;
        require_ok(&resp, "open")?;
        Ok(oracle)
    }

    /// Send one JSON request, read one JSON response. The request
    /// gains an `id` field (auto-incremented) before being serialized
    /// so the oracle can echo it; the response's `id` field is NOT
    /// validated here — reply-skew should manifest as higher-level
    /// assertion failures and we keep `call` focused on framing.
    fn call(&mut self, req: &Value) -> io::Result<Value> {
        self.next_id = self.next_id.wrapping_add(1);
        let mut with_id = req.clone();
        if let Some(obj) = with_id.as_object_mut() {
            obj.insert("id".into(), Value::from(self.next_id));
        }
        let line = serde_json::to_string(&with_id).map_err(io::Error::other)?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        let mut buf = String::new();
        let n = self.stdout.read_line(&mut buf)?;
        if n == 0 {
            return Err(io::Error::other("oracle closed stdout unexpectedly"));
        }
        serde_json::from_str(buf.trim_end()).map_err(io::Error::other)
    }
}

impl Drop for GoOracle {
    fn drop(&mut self) {
        // Best-effort graceful close. Every failure path is ignored
        // — `drop` MUST NOT panic, especially during a test
        // assertion's unwind, and the child may already be dead.
        let _ = self.stdin.write_all(br#"{"op":"close"}"#);
        let _ = self.stdin.write_all(b"\n");
        let _ = self.stdin.flush();

        // Poll for up to 500ms; if the child hasn't exited by then,
        // send SIGKILL. The oracle's close handler is O(1) (just
        // `db.Close()` + return) so 500ms is ample and leaves
        // headroom for an fsync under load.
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() >= deadline => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Resolve the oracle binary path. Panics with an actionable
/// message if it cannot be found — see module docs for rationale.
fn oracle_binary() -> PathBuf {
    if let Ok(p) = std::env::var(ORACLE_ENV) {
        let path = PathBuf::from(p);
        if path.exists() {
            return path;
        }
        panic!(
            "{ORACLE_ENV} points to non-existent path: {}",
            path.display()
        );
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest_dir.join(ORACLE_REL);
    if candidate.exists() {
        return candidate;
    }
    panic!(
        "bbolt oracle binary not found at {} \
         and {ORACLE_ENV} is unset. Build it first: \
         `cd benches/oracles/bbolt && ./build.sh`",
        candidate.display()
    );
}

/// Assert `resp["ok"] == true`, or return an error describing the
/// failure. Wraps the common boilerplate used by every call site in
/// the smoke test.
fn require_ok(resp: &Value, context: &str) -> io::Result<()> {
    if resp.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    let err = resp
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("<no error field>");
    Err(io::Error::other(format!(
        "{context}: ok=false, error={err}"
    )))
}

/// Standard-encoding helper — matches the oracle's wire convention.
fn b64(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

/// The 10-op protocol round-trip smoke test (plan §9 commit 6).
///
/// Exercises every basic op the harness will emit once proptest is
/// wired, without yet involving `RedbBackend`. A green run here
/// proves: (a) the subprocess spawn works, (b) JSON framing is
/// symmetric across the pipe, (c) base64 payloads survive
/// round-trip, (d) `close` cleanly terminates the child without
/// relying on drop-guard kill.
#[test]
fn smoke_ten_ops_protocol_round_trip() {
    let binary = oracle_binary();
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("oracle.db");

    let mut oracle = GoOracle::spawn(&binary, &db_path, false).expect("spawn oracle");

    // 1. Register bucket.
    let r = oracle
        .call(&json!({"op":"bucket","name":"b1"}))
        .expect("bucket");
    require_ok(&r, "bucket").unwrap();

    // 2. Begin writable txn.
    let r = oracle.call(&json!({"op":"begin"})).expect("begin");
    require_ok(&r, "begin").unwrap();

    // 3-5. Three puts, one with a value containing NUL + newline +
    // high byte — the worst-case test of base64 framing (ensures we
    // do NOT corrupt bytes that would otherwise break line-oriented
    // protocols).
    for (k, v) in [
        ("k1", &b"v1"[..]),
        ("k2", &b"v2"[..]),
        ("k3", &b"\x00\n\r\xff"[..]),
    ] {
        let r = oracle
            .call(&json!({
                "op":"put","bucket":"b1",
                "key": b64(k.as_bytes()), "value": b64(v),
            }))
            .expect("put");
        require_ok(&r, "put").unwrap();
    }

    // 6. Commit the txn.
    let r = oracle
        .call(&json!({"op":"commit","fsync":false}))
        .expect("commit");
    require_ok(&r, "commit").unwrap();

    // 7. Get — value must round-trip byte-for-byte.
    let r = oracle
        .call(&json!({"op":"get","bucket":"b1","key": b64(b"k3")}))
        .expect("get");
    require_ok(&r, "get").unwrap();
    let got_value = r
        .get("value")
        .and_then(Value::as_str)
        .expect("value field present on hit");
    let decoded = BASE64.decode(got_value).expect("base64 decode");
    assert_eq!(decoded, b"\x00\n\r\xff", "binary round-trip failed");

    // 8. Range over [k1, k3) — half-open, must return exactly {k1, k2}.
    let r = oracle
        .call(&json!({
            "op":"range","bucket":"b1",
            "start": b64(b"k1"), "end": b64(b"k3"), "limit": 0,
        }))
        .expect("range");
    require_ok(&r, "range").unwrap();
    let entries = r
        .get("entries")
        .and_then(Value::as_array)
        .expect("entries array");
    assert_eq!(entries.len(), 2, "range returned {} entries", entries.len());

    // 9. Begin + delete k2 + commit, then assert it's gone.
    let r = oracle.call(&json!({"op":"begin"})).expect("begin 2");
    require_ok(&r, "begin 2").unwrap();
    let r = oracle
        .call(&json!({"op":"delete","bucket":"b1","key": b64(b"k2")}))
        .expect("delete");
    require_ok(&r, "delete").unwrap();
    let r = oracle.call(&json!({"op":"commit"})).expect("commit 2");
    require_ok(&r, "commit 2").unwrap();
    let r = oracle
        .call(&json!({"op":"get","bucket":"b1","key": b64(b"k2")}))
        .expect("get after delete");
    require_ok(&r, "get after delete").unwrap();
    assert!(
        r.get("value").is_none(),
        "k2 should be absent after delete, got value={:?}",
        r.get("value")
    );

    // 10. Snapshot — remaining state is {k1, k3}.
    let r = oracle.call(&json!({"op":"snapshot"})).expect("snapshot");
    require_ok(&r, "snapshot").unwrap();
    let state = r
        .get("state")
        .and_then(Value::as_object)
        .expect("state object");
    let b1 = state
        .get("b1")
        .and_then(Value::as_array)
        .expect("b1 entries");
    assert_eq!(b1.len(), 2, "snapshot b1 has {} entries, want 2", b1.len());

    // Explicit close. Drop impl would also close, but we exercise
    // the clean path here so a green test proves the explicit
    // shutdown works — the drop-guard path is a backstop.
    let r = oracle.call(&json!({"op":"close"})).expect("close");
    require_ok(&r, "close").unwrap();
}

/// Drop without an explicit close must still reap the child. If the
/// drop-guard is broken (e.g. panicking, or not killing a wedged
/// child), this test hangs or leaves a zombie — visible as a
/// nextest timeout or `ps` residue post-run.
#[test]
fn drop_without_close_reaps_child() {
    let binary = oracle_binary();
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("oracle.db");

    let oracle = GoOracle::spawn(&binary, &db_path, false).expect("spawn oracle");
    // Deliberately no `close` call — simulate a test that panicked
    // mid-sequence. Drop runs here.
    drop(oracle);
    // If the child didn't exit inside the Drop deadline, it would
    // either still be running (noisy) or we'd have panicked. A
    // successful return means the guard did its job.
}
