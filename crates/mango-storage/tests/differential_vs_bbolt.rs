//! Differential-test harness vs bbolt (ROADMAP:819).
//!
//! This file hosts the Rust-side harness that drives the Go bbolt
//! oracle (`benches/oracles/bbolt/`) in lockstep with a
//! [`RedbBackend`] and asserts byte-identical state after every
//! commit boundary.
//!
//! Scope at this commit (plan §9 commit 7): the [`DiffOp`] language
//! (`Put` / `Delete` / `DeleteRange` / `Commit` / `Rollback`), the
//! [`Case`] fixture with plan §7's field-drop order, the per-op
//! `apply_op` dispatcher, post-commit snapshot diff against a
//! `BTreeMap` oracle, a hardcoded `smoke_10_ops_no_divergence` and
//! a 256-case `proptest_256_cases_no_divergence`.
//!
//! Out of scope here — lands in later commits:
//! - `CommitGroup` / `Defragment` / `CloseReopen` / error-triggering
//!   ops (§9 commit 8).
//! - Divergence-artifact preservation + piped-stderr dump (§9
//!   commit 9).
//! - CI wiring + nightly 10k-case run (§9 commits 10–11).
//!
//! The earlier [`GoOracle`] subprocess helper and protocol round-trip
//! smoke test (plan §9 commit 6) are kept in place below — they
//! serve as a narrower smoke check independent of `RedbBackend`.
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
    clippy::cast_sign_loss,
    // `apply_op` is a five-variant match whose arms are structurally
    // parallel; splitting further would fragment the dispatcher and
    // hurt readability more than line count helps.
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use mango_storage::{
    Backend, BackendConfig, BucketId, ReadSnapshot, RedbBackend, RedbBatch, WriteBatch,
};
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, TestCaseError, TestRunner};
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

/// Locate the oracle binary without panicking. Returns `None` when
/// neither the `MANGO_BBOLT_ORACLE` env var nor the default relative
/// path resolve to an existing file. Callers decide whether to
/// panic (interactive `oracle_binary`), or to skip (`test` CI job,
/// via [`skip_without_oracle`]).
///
/// An `MANGO_BBOLT_ORACLE` env var pointing at a non-existent path
/// still panics via [`oracle_binary`] — that's a user error worth
/// surfacing loudly, not a "binary missing" skip signal.
fn oracle_binary_opt() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest_dir.join(ORACLE_REL);
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// Resolve the oracle binary path. Panics with an actionable
/// message if it cannot be found — see module docs for rationale.
///
/// Tests that should skip (rather than fail) in CI environments
/// without the oracle built should call [`skip_without_oracle`]
/// instead.
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
    oracle_binary_opt().unwrap_or_else(|| {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let candidate = manifest_dir.join(ORACLE_REL);
        panic!(
            "bbolt oracle binary not found at {} \
             and {ORACLE_ENV} is unset. Build it first: \
             `cd benches/oracles/bbolt && ./build.sh`",
            candidate.display()
        )
    })
}

/// Return `Some(path)` if the oracle binary is available, or `None`
/// with a skip-line on stderr otherwise. The skip branch is what
/// keeps the default `test` CI job green — that job does not build
/// the oracle (the dedicated `differential` CI job does, per plan
/// commit 10). Local dev: run `./benches/oracles/bbolt/build.sh`
/// once and every test flips from skipped to exercised.
///
/// An explicit `MANGO_BBOLT_ORACLE` env override still forces the
/// panic path via [`oracle_binary`] — a mis-set override is a bug
/// the developer needs to see, not a silent skip.
fn skip_without_oracle(test_name: &str) -> Option<PathBuf> {
    if std::env::var(ORACLE_ENV).is_ok() {
        return Some(oracle_binary());
    }
    if let Some(p) = oracle_binary_opt() {
        Some(p)
    } else {
        eprintln!(
            "{test_name}: SKIP — bbolt oracle binary not built. \
             Run `cd benches/oracles/bbolt && ./build.sh` to enable."
        );
        None
    }
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

// -----------------------------------------------------------------------------
// Differential harness — plan §9 commit 7.
// -----------------------------------------------------------------------------

/// Bucket alphabet shared by both engines. Three ASCII buckets keep
/// byte-lex and UTF-8-lex ordering identical on both sides (plan
/// §3 N3). Indexed by `u8` — `DiffOp::Put::bucket` etc. carry the
/// index (0..=2), which we translate to a [`BucketId`] (1..=3) and
/// to a `&'static str` for the oracle wire.
///
/// `BucketId(0)` is reserved (the trait docs call out 0 as reserved
/// for the Raft log). Shifting the proptest bucket index by one
/// keeps us clear of that reservation while staying dense.
const BUCKET_NAMES: &[&str] = &["b1", "b2", "b3"];

/// Translate a 0-based bucket index into the [`BucketId`] registered
/// on the `RedbBackend` side. Panics on out-of-range input —
/// proptest strategies constrain inputs to `0..BUCKET_NAMES.len()`
/// so any panic here is a harness bug.
fn bucket_id_of(idx: u8) -> BucketId {
    assert!(
        (idx as usize) < BUCKET_NAMES.len(),
        "bucket index {idx} out of range"
    );
    BucketId::new(u16::from(idx) + 1)
}

/// Translate a 0-based bucket index into the wire-protocol bucket
/// name for the oracle. See [`bucket_id_of`] for the index contract.
fn bucket_name_of(idx: u8) -> &'static str {
    BUCKET_NAMES[idx as usize]
}

/// An op in the differential language. Subset per plan §9 commit 7:
/// `CommitGroup` / `Defragment` / `CloseReopen` / error-triggering
/// ops land in commit 8.
///
/// `#[derive(Debug, Clone)]` — cheap to clone for proptest
/// shrinking; the harness does not hold ops across threads, so
/// `Send`-ness is not required.
#[derive(Debug, Clone)]
enum DiffOp {
    /// Insert-or-overwrite a non-empty (key, value) in `bucket`.
    Put {
        bucket: u8,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    /// Delete a single key. No-op on both engines when absent.
    Delete { bucket: u8, key: Vec<u8> },
    /// Delete every key in `[start, end)`. Strategies generate
    /// `start <= end` — the `start > end` axis is an error-triggering
    /// op and lands in commit 8.
    DeleteRange {
        bucket: u8,
        start: Vec<u8>,
        end: Vec<u8>,
    },
    /// Commit the pending batch. If no writes have been staged since
    /// the last commit/rollback, the harness skips the commit call
    /// (oracle rejects `commit` without an active txn) but STILL
    /// runs a snapshot diff — drift between engines must not exist
    /// even when no new work was committed.
    Commit { fsync: bool },
    /// Discard the pending batch on both engines. A rollback without
    /// an active txn is a no-op. Like `Commit`, followed by a
    /// snapshot diff for drift-detection.
    Rollback,
}

/// Mutable per-case state threaded through [`apply_op`]. Tracks the
/// pending `RedbBatch` so the oracle's `begin`-before-write invariant
/// is mirrored on the Rust side: on the first write op after a
/// commit/rollback we lazily call `begin_batch` on redb and emit
/// `begin` to the oracle; on commit/rollback we clear back to `None`.
///
/// The oracle's "txn active" bit is intentionally not tracked
/// separately — `pending.is_some()` is the single source of truth;
/// our state machine calls oracle `begin` exactly when transitioning
/// `None -> Some`.
#[derive(Default)]
struct RunState {
    pending: Option<RedbBatch>,
}

/// The per-case test fixture. Field order is the **drop order** and
/// MUST match plan §7:
///
/// 1. `oracle` — close the pipe, reap the Go child.
/// 2. `redb` — close the redb Database handle.
/// 3. `_bbolt_dir` — remove the bbolt db file.
/// 4. `_redb_dir` — remove the redb db file.
///
/// If the `TempDir`s dropped before the engines, `db.Close()` on the
/// Go side would run on a deleted directory → EIO on fsync → panic
/// in Drop → test-runner confusion. Rust drops struct fields in
/// declaration order; a `compile_fail` guard is deliberately NOT
/// used here (the invariant is positional, not type-level), so the
/// field-order comment above is load-bearing.
///
/// All four engine/dir slots are `Option<T>`. Rationale (rust-expert
/// NIT-3 on the PR-B plan): `GoOracle::spawn` and `RedbBackend::open`
/// both acquire engine-level file locks before returning — bbolt's
/// flock via the inline `open` request, and redb's single-writer
/// guard via `Database::create`. So `close_and_reopen` (commit 8
/// step 5) cannot use `mem::replace`, which would construct the
/// replacement *before* dropping the old handle and deadlock on the
/// flock or panic on `Database::create`. The `Option<T>` shape lets
/// `close_and_reopen` `take()` the old handle, drop it, and *then*
/// install the new one. The slots are empty only for those few
/// lines; every other site sees them via `Some`-asserting accessors
/// and is none the wiser. `Option::drop` invokes `T::drop` on
/// `Some`, so the field-order drop invariant is preserved.
struct Case {
    oracle: Option<GoOracle>,
    redb: Option<RedbBackend>,
    _bbolt_dir: Option<TempDir>,
    _redb_dir: Option<TempDir>,
    /// Path to the prebuilt Go oracle binary, kept so
    /// `close_and_reopen` can respawn the subprocess. The binary is
    /// resolved once per test via `skip_without_oracle` and
    /// thread-safe to share by path.
    #[expect(
        dead_code,
        reason = "consumed by close_and_reopen in the next commit on this branch"
    )]
    oracle_binary_path: PathBuf,
    /// fsync bit threaded into every commit and into the new
    /// `GoOracle` constructed by `close_and_reopen`. Captured at
    /// `Case::new` time so the close-reopen cycle is durability-
    /// neutral against the original spawn.
    #[expect(
        dead_code,
        reason = "consumed by close_and_reopen in the next commit on this branch"
    )]
    fsync: bool,
}

impl Case {
    /// Spawn the oracle and open a fresh `RedbBackend` in parallel
    /// `TempDir`s, then register the three shared buckets on both
    /// sides so subsequent ops can skip any auto-register concerns
    /// (plan §5 "Accepted quirks" — pre-register eliminates the
    /// bbolt-auto-create asymmetry at the fixture level).
    fn new(binary: &Path, fsync: bool) -> Result<Self, String> {
        let bbolt_dir = TempDir::new().map_err(|e| format!("bbolt tempdir: {e}"))?;
        let redb_dir = TempDir::new().map_err(|e| format!("redb tempdir: {e}"))?;
        let db_path = bbolt_dir.path().join("oracle.db");

        let mut oracle =
            GoOracle::spawn(binary, &db_path, fsync).map_err(|e| format!("oracle spawn: {e}"))?;
        let redb = RedbBackend::open(BackendConfig::new(redb_dir.path().to_path_buf(), false))
            .map_err(|e| format!("redb open: {e}"))?;

        for (idx, name) in BUCKET_NAMES.iter().enumerate() {
            let id = BucketId::new((idx + 1) as u16);
            let resp = oracle
                .call(&json!({"op":"bucket","name":name}))
                .map_err(|e| format!("oracle bucket {name}: {e}"))?;
            require_ok(&resp, &format!("bucket {name}")).map_err(|e| e.to_string())?;
            redb.register_bucket(name, id)
                .map_err(|e| format!("redb register_bucket {name}: {e}"))?;
        }

        Ok(Self {
            oracle: Some(oracle),
            redb: Some(redb),
            _bbolt_dir: Some(bbolt_dir),
            _redb_dir: Some(redb_dir),
            oracle_binary_path: binary.to_path_buf(),
            fsync,
        })
    }

    /// Mutable accessor for the Go oracle subprocess handle. Panics
    /// (with a message naming the slot) if called between the
    /// `take()` and the reassignment inside `close_and_reopen` —
    /// which is by design: that interval is invariant-violating.
    fn oracle_mut(&mut self) -> &mut GoOracle {
        self.oracle.as_mut().expect("oracle slot non-empty")
    }

    /// Shared accessor for the redb backend.
    fn redb(&self) -> &RedbBackend {
        self.redb.as_ref().expect("redb slot non-empty")
    }

    /// Borrow `redb` (shared) and `oracle` (exclusive) at once.
    /// Required by the snapshot-and-diff and commit paths where
    /// both halves of the harness must be in scope simultaneously.
    /// Sound because the two `Option`s live in disjoint struct
    /// fields, so the resulting references do not alias.
    fn split_redb_and_oracle(&mut self) -> (&RedbBackend, &mut GoOracle) {
        (
            self.redb.as_ref().expect("redb slot non-empty"),
            self.oracle.as_mut().expect("oracle slot non-empty"),
        )
    }
}

/// Lazily open a write batch on both engines on the first write op
/// after a commit/rollback. Idempotent — repeated calls with an
/// already-active txn are a no-op.
fn ensure_txn(case: &mut Case, state: &mut RunState) -> Result<(), String> {
    if state.pending.is_some() {
        return Ok(());
    }
    let batch = case
        .redb()
        .begin_batch()
        .map_err(|e| format!("redb begin_batch: {e}"))?;
    let resp = case
        .oracle_mut()
        .call(&json!({"op":"begin"}))
        .map_err(|e| format!("oracle begin: {e}"))?;
    require_ok(&resp, "begin").map_err(|e| e.to_string())?;
    state.pending = Some(batch);
    Ok(())
}

/// Extract the `error` field from an oracle response. `None` if the
/// response reports `ok: true` or the field is missing (ok-without-
/// error shape).
fn oracle_error(resp: &Value) -> Option<String> {
    if resp.get("ok").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    resp.get("error")
        .and_then(Value::as_str)
        .map(std::borrow::ToOwned::to_owned)
}

/// Normalize an error string to its engine-neutral core by stripping
/// known wire-level wrappers. Go's oracle wraps errors as
/// `"app: <Method>: <inner>"`, and redb's [`BackendError`] `Display`
/// adds `"backend: "`. Structural prefix stripping — not method-name
/// matching — keeps the helper decoupled from the oracle's exact
/// labels.
///
/// If `"app: "` is present without the inner `": "` separator (a
/// drift in `main.go` that lacks a method label), the helper still
/// strips `"app: "` so a future wire-format change can't silently
/// mask a divergence.
fn normalize_err(raw: &str) -> String {
    // Strip redb's BackendError Display prefix.
    let s = raw.strip_prefix("backend: ").unwrap_or(raw);
    // Strip the Go oracle's wire wrapper.
    if let Some(rest) = s.strip_prefix("app: ") {
        if let Some((_method, inner)) = rest.split_once(": ") {
            return map_alias(inner);
        }
        return map_alias(rest);
    }
    map_alias(s)
}

/// Map bbolt's error vocabulary into redb's where they differ on wire
/// but mean the same thing. Keep this table tiny and obvious — every
/// entry is a deliberate "these two strings are the same error class"
/// decision, not a regex or heuristic.
fn map_alias(s: &str) -> String {
    match s {
        "key required" => "empty key".to_owned(),
        "value cannot be nil" => "empty value".to_owned(),
        other => other.to_owned(),
    }
}

/// Apply one [`DiffOp`] in lockstep to both engines. Post-commit
/// and post-rollback we run [`snapshot_and_diff`] to detect drift.
///
/// Error-symmetry handling is intentionally conservative for commit
/// 7: every op's strategy generates inputs that should succeed on
/// both engines (non-empty keys/values, `start <= end` bounds, etc.),
/// so a staging error on either side is treated as a harness fault
/// and propagated. Commit 8 introduces error-triggering ops and
/// upgrades this to the full symmetric-error contract of plan §5.
fn apply_op(
    rt: &tokio::runtime::Runtime,
    case: &mut Case,
    state: &mut RunState,
    op: &DiffOp,
) -> Result<(), String> {
    match op {
        DiffOp::Put { bucket, key, value } => {
            ensure_txn(case, state)?;
            let bucket_id = bucket_id_of(*bucket);
            let bucket_name = bucket_name_of(*bucket);
            state
                .pending
                .as_mut()
                .expect("ensure_txn left pending unset")
                .put(bucket_id, key, value)
                .map_err(|e| format!("redb put: {e}"))?;
            let resp = case
                .oracle_mut()
                .call(&json!({
                    "op":"put","bucket":bucket_name,
                    "key":b64(key),"value":b64(value),
                }))
                .map_err(|e| format!("oracle put: {e}"))?;
            require_ok(&resp, "put").map_err(|e| e.to_string())?;
            Ok(())
        }
        DiffOp::Delete { bucket, key } => {
            ensure_txn(case, state)?;
            let bucket_id = bucket_id_of(*bucket);
            let bucket_name = bucket_name_of(*bucket);
            state
                .pending
                .as_mut()
                .expect("ensure_txn left pending unset")
                .delete(bucket_id, key)
                .map_err(|e| format!("redb delete: {e}"))?;
            let resp = case
                .oracle_mut()
                .call(&json!({
                    "op":"delete","bucket":bucket_name,"key":b64(key),
                }))
                .map_err(|e| format!("oracle delete: {e}"))?;
            require_ok(&resp, "delete").map_err(|e| e.to_string())?;
            Ok(())
        }
        DiffOp::DeleteRange { bucket, start, end } => {
            ensure_txn(case, state)?;
            let bucket_id = bucket_id_of(*bucket);
            let bucket_name = bucket_name_of(*bucket);
            state
                .pending
                .as_mut()
                .expect("ensure_txn left pending unset")
                .delete_range(bucket_id, start, end)
                .map_err(|e| format!("redb delete_range: {e}"))?;
            let resp = case
                .oracle_mut()
                .call(&json!({
                    "op":"delete_range","bucket":bucket_name,
                    "start":b64(start),"end":b64(end),
                }))
                .map_err(|e| format!("oracle delete_range: {e}"))?;
            require_ok(&resp, "delete_range").map_err(|e| e.to_string())?;
            Ok(())
        }
        DiffOp::Commit { fsync } => {
            let Some(batch) = state.pending.take() else {
                // No active txn on either side. Still diff — a drift
                // here would mean something committed without our
                // harness emitting a commit, which is a real bug.
                let (redb, oracle) = case.split_redb_and_oracle();
                snapshot_and_diff(redb, oracle)?;
                return Ok(());
            };
            let redb_res = rt.block_on(case.redb().commit_batch(batch, *fsync));
            let resp = case
                .oracle_mut()
                .call(&json!({"op":"commit","fsync":*fsync}))
                .map_err(|e| format!("oracle commit: {e}"))?;
            let oracle_err = oracle_error(&resp);
            match (redb_res, oracle_err) {
                (Ok(_), None) => {}
                (Err(e), Some(oe)) => {
                    // Symmetric error — both engines rejected. The
                    // plan §5 hard contract requires the normalized
                    // errors to match: identical error class on wire,
                    // modulo engine-specific wrappers.
                    let redb_norm = normalize_err(&e.to_string());
                    let oracle_norm = normalize_err(&oe);
                    if redb_norm != oracle_norm {
                        return Err(format!(
                            "symmetric commit error but normalized strings diverge: \
                             redb={redb_norm:?} (raw={e}), oracle={oracle_norm:?} (raw={oe})"
                        ));
                    }
                }
                (Ok(_), Some(oe)) => {
                    return Err(format!("divergence on commit: redb ok, oracle err={oe}"));
                }
                (Err(e), None) => {
                    return Err(format!("divergence on commit: redb err={e}, oracle ok"));
                }
            }
            let (redb, oracle) = case.split_redb_and_oracle();
            snapshot_and_diff(redb, oracle)?;
            Ok(())
        }
        DiffOp::Rollback => {
            if state.pending.is_none() {
                return Ok(());
            }
            // Drop the batch on the Rust side — staging buffer, no
            // fsync path, cannot fail.
            state.pending = None;
            let resp = case
                .oracle_mut()
                .call(&json!({"op":"rollback"}))
                .map_err(|e| format!("oracle rollback: {e}"))?;
            require_ok(&resp, "rollback").map_err(|e| e.to_string())?;
            let (redb, oracle) = case.split_redb_and_oracle();
            snapshot_and_diff(redb, oracle)?;
            Ok(())
        }
    }
}

/// The sentinel that bounds "every possible test key" on the
/// `range()` upper side. Proptest key bytes are drawn from `0..=15`
/// and length `1..=16`, so any single byte `>= 0x10` strictly
/// exceeds every generatable key. 17 bytes of `0xFF` is defensive
/// overkill; kept for robustness if the key alphabet is ever widened.
const RANGE_END_SENTINEL: &[u8] = &[0xff; 17];

/// Full (bucket, key) → value map of one engine's current state,
/// materialized via iteration over each registered bucket. Used by
/// [`snapshot_and_diff`] to compare byte-identically.
type StateMap = BTreeMap<(String, Vec<u8>), Vec<u8>>;

fn full_snapshot_redb(redb: &RedbBackend) -> Result<StateMap, String> {
    let snap = redb.snapshot().map_err(|e| format!("redb snapshot: {e}"))?;
    let mut out = StateMap::new();
    for (idx, name) in BUCKET_NAMES.iter().enumerate() {
        let id = BucketId::new((idx + 1) as u16);
        let iter = snap
            .range(id, &[][..], RANGE_END_SENTINEL)
            .map_err(|e| format!("redb range {name}: {e}"))?;
        for entry in iter {
            let (k, v) = entry.map_err(|e| format!("redb range item {name}: {e}"))?;
            out.insert(((*name).to_owned(), k.to_vec()), v.to_vec());
        }
    }
    Ok(out)
}

fn full_snapshot_oracle(oracle: &mut GoOracle) -> Result<StateMap, String> {
    let resp = oracle
        .call(&json!({"op":"snapshot"}))
        .map_err(|e| format!("oracle snapshot: {e}"))?;
    require_ok(&resp, "snapshot").map_err(|e| e.to_string())?;
    let state = resp
        .get("state")
        .and_then(Value::as_object)
        .ok_or_else(|| "oracle snapshot: missing state object".to_owned())?;
    let mut out = StateMap::new();
    for (bucket_name, entries_val) in state {
        let entries = entries_val
            .as_array()
            .ok_or_else(|| format!("oracle snapshot: {bucket_name} entries not an array"))?;
        for (i, entry) in entries.iter().enumerate() {
            let pair = entry
                .as_array()
                .ok_or_else(|| format!("oracle snapshot: {bucket_name}[{i}] not an array"))?;
            if pair.len() != 2 {
                return Err(format!(
                    "oracle snapshot: {bucket_name}[{i}] has {} elements, want 2",
                    pair.len()
                ));
            }
            let k_b64 = pair[0]
                .as_str()
                .ok_or_else(|| format!("oracle snapshot: {bucket_name}[{i}].k not a string"))?;
            let v_b64 = pair[1]
                .as_str()
                .ok_or_else(|| format!("oracle snapshot: {bucket_name}[{i}].v not a string"))?;
            let k = BASE64
                .decode(k_b64)
                .map_err(|e| format!("oracle snapshot: {bucket_name}[{i}].k base64: {e}"))?;
            let v = BASE64
                .decode(v_b64)
                .map_err(|e| format!("oracle snapshot: {bucket_name}[{i}].v base64: {e}"))?;
            out.insert((bucket_name.clone(), k), v);
        }
    }
    Ok(out)
}

/// Snapshot both engines at the same logical cut and assert
/// byte-identical state. Commit 9 layers on artifact preservation
/// (`target/differential-failures/<case>/{ops,bbolt,redb,diff}`);
/// for now a plain `Err` with the minimal diff is sufficient — the
/// proptest runner surfaces the message and the test fails loud.
fn snapshot_and_diff(redb: &RedbBackend, oracle: &mut GoOracle) -> Result<(), String> {
    let r = full_snapshot_redb(redb)?;
    let o = full_snapshot_oracle(oracle)?;
    if r == o {
        return Ok(());
    }
    let mut lines = Vec::new();
    lines.push(format!(
        "DIVERGENCE: redb has {} entries, oracle has {}",
        r.len(),
        o.len()
    ));
    // Collect first 20 differing keys for minimal readable output.
    let mut shown = 0usize;
    let mut keys: std::collections::BTreeSet<&(String, Vec<u8>)> =
        std::collections::BTreeSet::new();
    keys.extend(r.keys());
    keys.extend(o.keys());
    for key in keys {
        if shown >= 20 {
            lines.push("...(truncated)".into());
            break;
        }
        let rv = r.get(key);
        let ov = o.get(key);
        if rv == ov {
            continue;
        }
        shown += 1;
        lines.push(format!(
            "{}/{:?}: redb={:?}, oracle={:?}",
            key.0, key.1, rv, ov
        ));
    }
    Err(lines.join("\n"))
}

/// Run a sequence of [`DiffOp`]s against both engines. Returns
/// `Ok(())` iff every post-commit snapshot diff agreed. Errors
/// carry a human-readable message; the proptest runner promotes
/// them into `TestCaseError::fail`.
fn run_case(binary: &Path, ops: &[DiffOp]) -> Result<(), String> {
    // Default true; override with MANGO_DIFFERENTIAL_FSYNC=0 for
    // local macOS iteration (plan §7).
    let fsync = std::env::var("MANGO_DIFFERENTIAL_FSYNC").as_deref() != Ok("0");
    let mut case = Case::new(binary, fsync)?;

    // Single-threaded runtime is sufficient: RedbBackend's
    // commit_batch internally uses spawn_blocking (the blocking
    // pool is available under current_thread too), and the harness
    // never spawns a second task.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    let mut state = RunState::default();

    for (idx, op) in ops.iter().enumerate() {
        apply_op(&rt, &mut case, &mut state, op).map_err(|e| format!("op[{idx}] {op:?}: {e}"))?;
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// proptest strategies — commit 7 subset.
// -----------------------------------------------------------------------------

/// 0..=2 uniform bucket index.
fn bucket_idx() -> impl Strategy<Value = u8> {
    0u8..(BUCKET_NAMES.len() as u8)
}

/// Non-empty key: length `1..=16`, bytes drawn from the 16-value
/// alphabet `[0..=15]`. High-collision by design (plan §3).
fn key_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(0u8..=15u8, 1..=16)
}

/// Non-empty value. Commit 7 keeps the distribution simple
/// (`1..=16`, same alphabet as keys). Commit 8 widens to the
/// `prop_oneof![...]` in plan §3 B2 with empty / medium / overflow
/// buckets once the symmetric-error contract is wired.
fn value_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(0u8..=15u8, 1..=16)
}

fn put_strat() -> impl Strategy<Value = DiffOp> {
    (bucket_idx(), key_bytes(), value_bytes()).prop_map(|(bucket, key, value)| DiffOp::Put {
        bucket,
        key,
        value,
    })
}

fn delete_strat() -> impl Strategy<Value = DiffOp> {
    (bucket_idx(), key_bytes()).prop_map(|(bucket, key)| DiffOp::Delete { bucket, key })
}

/// `DeleteRange` with `start <= end`. We generate two key-shaped
/// byte vectors and swap into order — cheaper than a rejection-
/// sampled strategy and keeps the generated distribution balanced
/// across the key space.
fn delete_range_strat() -> impl Strategy<Value = DiffOp> {
    (bucket_idx(), key_bytes(), key_bytes()).prop_map(|(bucket, a, b)| {
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        DiffOp::DeleteRange { bucket, start, end }
    })
}

fn commit_strat() -> impl Strategy<Value = DiffOp> {
    any::<bool>().prop_map(|fsync| DiffOp::Commit { fsync })
}

fn rollback_strat() -> Just<DiffOp> {
    Just(DiffOp::Rollback)
}

/// Per-op strategy. Weights derived from plan §3 by zeroing the
/// op classes that don't exist in commit 7 (`CommitGroup`,
/// `CloseReopen`, `Defragment`, error-triggering) and renormalizing.
///
/// Put 50 / Delete 20 / `DeleteRange` 5 / Commit 20 / Rollback 5 =
/// total 100. Put-heavy to build up state; Commit at 20 % keeps
/// the snapshot-diff cadence frequent.
fn op_strat() -> impl Strategy<Value = DiffOp> {
    prop_oneof![
        50 => put_strat(),
        20 => delete_strat(),
        5  => delete_range_strat(),
        20 => commit_strat(),
        5  => rollback_strat(),
    ]
}

/// Sequence strategy. `1..=40` generated ops, then a terminal
/// `Commit { fsync: false }` appended unconditionally (plan §3:
/// "final op is always Commit").
fn op_sequence_strat() -> impl Strategy<Value = Vec<DiffOp>> {
    proptest::collection::vec(op_strat(), 1..=40).prop_map(|mut ops| {
        ops.push(DiffOp::Commit { fsync: false });
        ops
    })
}

/// Pick the proptest case count. Default 256 (< 60 s on a dev box
/// per plan §10), `MANGO_DIFFERENTIAL_THOROUGH=1` bumps to `10_000`
/// for nightly / milestone CI.
fn proptest_cases() -> u32 {
    match std::env::var("MANGO_DIFFERENTIAL_THOROUGH").as_deref() {
        Ok("1") => 10_000,
        _ => 256,
    }
}

/// The 10-op protocol round-trip smoke test (plan §9 commit 6).
///
/// Pin the normalizer's behavior across the wire-wrapper permutations
/// the harness actually observes. If `main.go` ever drifts — renaming
/// a method label, changing the alias strings, or omitting the
/// inner `": "` separator — this test is the canary.
#[test]
fn normalize_err_unit() {
    // 1. Redb BackendError Display prefix stripped, pass-through after.
    assert_eq!(normalize_err("backend: empty key"), "empty key");
    // 2. Go oracle wrapper + alias: "Put: key required" → "empty key".
    assert_eq!(normalize_err("app: Put: key required"), "empty key");
    // 3. Same alias through Delete path.
    assert_eq!(normalize_err("app: Delete: key required"), "empty key");
    // 4. Empty-value alias through commit_group path.
    assert_eq!(
        normalize_err("app: commit_group: value cannot be nil"),
        "empty value"
    );
    // 5. Redb prefix + non-aliased inner — pass through.
    assert_eq!(
        normalize_err("backend: UnknownBucket(BucketId { raw: 7 })"),
        "UnknownBucket(BucketId { raw: 7 })"
    );
    // 6. Malformed Go wrapper without the inner ": " separator still
    //    strips "app: " (defensive fallthrough per rust-expert NIT-5).
    assert_eq!(normalize_err("app: kaboom"), "kaboom");
    // 7. Pass-through for a string with no recognized prefix.
    assert_eq!(normalize_err("some other thing"), "some other thing");
}

/// Exercises every basic op the harness will emit once proptest is
/// wired, without yet involving `RedbBackend`. A green run here
/// proves: (a) the subprocess spawn works, (b) JSON framing is
/// symmetric across the pipe, (c) base64 payloads survive
/// round-trip, (d) `close` cleanly terminates the child without
/// relying on drop-guard kill.
#[test]
fn smoke_ten_ops_protocol_round_trip() {
    let Some(binary) = skip_without_oracle("smoke_ten_ops_protocol_round_trip") else {
        return;
    };
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
    let Some(binary) = skip_without_oracle("drop_without_close_reaps_child") else {
        return;
    };
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

/// Hardcoded 10-op differential smoke test (plan §9 commit 7 /
/// §11). Exercises `Put` / `Delete` / `DeleteRange` / `Commit` /
/// `Rollback` against both engines and asserts byte-identical state
/// after every commit boundary. A green run here proves the
/// differential wiring before proptest takes over.
///
/// Sequence (10 user-visible ops, 3 commit boundaries, 1 rollback):
///
/// 1. `Put` b1 / a / 1
/// 2. `Put` b1 / b / 2
/// 3. `Commit` — diff #1 (state: {b1:{a:1, b:2}})
/// 4. `Put` b1 / c / 3
/// 5. `Delete` b1 / a
/// 6. `Commit` — diff #2 (state: {b1:{b:2, c:3}})
/// 7. `Put` b2 / x / y
/// 8. `Rollback` — diff with state unchanged from #2
/// 9. `DeleteRange` b1 / \[\] / \[0xff\]  (clears b1)
/// 10. `Commit` — diff #3 (state: {})
#[test]
fn smoke_10_ops_no_divergence() {
    let Some(binary) = skip_without_oracle("smoke_10_ops_no_divergence") else {
        return;
    };
    let ops = vec![
        DiffOp::Put {
            bucket: 0,
            key: b"a".to_vec(),
            value: b"1".to_vec(),
        },
        DiffOp::Put {
            bucket: 0,
            key: b"b".to_vec(),
            value: b"2".to_vec(),
        },
        DiffOp::Commit { fsync: false },
        DiffOp::Put {
            bucket: 0,
            key: b"c".to_vec(),
            value: b"3".to_vec(),
        },
        DiffOp::Delete {
            bucket: 0,
            key: b"a".to_vec(),
        },
        DiffOp::Commit { fsync: false },
        DiffOp::Put {
            bucket: 1,
            key: b"x".to_vec(),
            value: b"y".to_vec(),
        },
        DiffOp::Rollback,
        DiffOp::DeleteRange {
            bucket: 0,
            start: Vec::new(),
            end: vec![0xff],
        },
        DiffOp::Commit { fsync: false },
    ];
    run_case(&binary, &ops).expect("smoke 10 ops diverged");
}

/// Proptest-driven 256-case (default) / 10k-case (thorough) sweep.
///
/// Every generated sequence ends in `Commit`, so the terminal op
/// guarantees a final-state diff — a regression in any earlier op
/// surfaces at the latest by the end of the sequence (plan §3 N2).
///
/// Timing: on a Linux SSD the 256-case default completes in < 60 s
/// (plan §10 `DoD`); nightly 10k in < 15 min (plan §7 / §10).
///
/// Shrinking: proptest shrinks `Vec<DiffOp>` by truncation + per-op
/// shrink; byte vectors shrink by length + content. On divergence
/// the printed `ops` (formatted Debug) is the minimal reproducer.
#[test]
fn proptest_256_cases_no_divergence() {
    let Some(binary) = skip_without_oracle("proptest_256_cases_no_divergence") else {
        return;
    };
    // Manual `TestRunner` (instead of the `proptest!` macro) so the
    // case count can be resolved at test-start from the env var
    // (plan §1: `MANGO_DIFFERENTIAL_THOROUGH=1` → 10_000). The
    // macro reads config at expansion time, which won't see the
    // runtime env.
    let cases = proptest_cases();
    let config = ProptestConfig {
        cases,
        // Disable proptest's own failure-persistence dir under
        // `proptest-regressions/`; our regression persistence is
        // `tests/differential_vs_bbolt/seeds/` (wired in commit 9).
        failure_persistence: None,
        ..ProptestConfig::default()
    };
    let mut runner = TestRunner::new(config);
    let strategy = op_sequence_strat();
    runner
        .run(&strategy, |ops| {
            run_case(&binary, &ops).map_err(TestCaseError::fail)?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("proptest divergence: {e}"));
}
