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
    Backend, BackendConfig, BackendError, BucketId, ReadSnapshot, RedbBackend, RedbBatch,
    WriteBatch,
};
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, TestCaseError, TestRunner};
use serde::{Deserialize, Serialize};
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

/// Serde adapter for serializing `Vec<u8>` fields as base64 strings
/// instead of `serde_json`'s default JSON-array-of-bytes shape. Used
/// by `#[serde(with = "base64_helper")]` on the byte-vector fields of
/// [`DiffOp`] and [`GroupOp`] so seed files in
/// `tests/differential_vs_bbolt/seeds/*.json` stay grep-friendly and
/// ~4× smaller than the default encoding (plan §9 commit 9 step 6).
mod base64_helper {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&BASE64.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        BASE64.decode(s).map_err(serde::de::Error::custom)
    }
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

/// An op in the differential language. Commit 7 shipped Put / Delete /
/// `DeleteRange` / Commit / Rollback. Commit 8 adds `CloseReopen`
/// (process-restart durability axis); `CommitGroup` / `Defragment` /
/// error-triggering ops land in subsequent commits.
///
/// `#[derive(Debug, Clone)]` — cheap to clone for proptest
/// shrinking; the harness does not hold ops across threads, so
/// `Send`-ness is not required. `Serialize` / `Deserialize` round-trip
/// to/from `tests/differential_vs_bbolt/seeds/*.json` via the seed-
/// replay driver (plan §9 commit 9 step 6); byte-vector fields are
/// adapted through `base64_helper` so seed files stay grep-friendly.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum DiffOp {
    /// Insert-or-overwrite a non-empty (key, value) in `bucket`.
    Put {
        bucket: u8,
        #[serde(with = "base64_helper")]
        key: Vec<u8>,
        #[serde(with = "base64_helper")]
        value: Vec<u8>,
    },
    /// Delete a single key. No-op on both engines when absent.
    Delete {
        bucket: u8,
        #[serde(with = "base64_helper")]
        key: Vec<u8>,
    },
    /// Delete every key in `[start, end)`. Strategies generate
    /// `start <= end` — the `start > end` axis is an error-triggering
    /// op and lands in commit 8.
    DeleteRange {
        bucket: u8,
        #[serde(with = "base64_helper")]
        start: Vec<u8>,
        #[serde(with = "base64_helper")]
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
    /// Drop both engine handles and reopen against the same on-disk
    /// state. Tests durability across a "process restart". Any pending
    /// batch is discarded on the Rust side (no fsync, no commit) and
    /// the oracle's pending txn is dropped by closing its child
    /// process — symmetrical and lossy by design. Followed by a
    /// snapshot diff: post-reopen state must be byte-identical
    /// between engines.
    CloseReopen,
    /// Attempt to put with an empty key. Both engines must reject
    /// at stage time with their respective "empty key" error variants
    /// (`backend: empty key` on redb, `app: Put: key required` on
    /// bbolt) which `normalize_err` collapses to the shared
    /// `"empty key"`. No batch state changes on either side — the
    /// staging-time rejection means the pending txn is unaffected.
    PutNilKey {
        bucket: u8,
        #[serde(with = "base64_helper")]
        value: Vec<u8>,
    },
    /// Defragment / compact both engines. redb runs its in-place
    /// compaction; bbolt opens a fresh DB, copies via `bolt.Compact`,
    /// and atomic-renames over the original. Both reject if a txn is
    /// active — the harness rolls back the pending batch first to
    /// avoid that error path (it would mask real divergences).
    /// Followed by a snapshot diff: post-defrag state must remain
    /// byte-identical between engines.
    Defragment,
    /// Commit multiple batches atomically as a single group. Mirrors
    /// `Backend::commit_group` on the Rust side and the oracle's
    /// `commit_group` (whose `req.Batches` is `[][]groupOp`). All
    /// inner mutations succeed-or-fail as one — bbolt's Update
    /// closure rolls back on any error, redb's `commit_group` commits
    /// all staged batches in a single write txn. Pre-condition:
    /// no active txn (both engines reject otherwise), so the harness
    /// rolls back any pending batch first.
    CommitGroup {
        /// Outer Vec is batches; inner is the ops in that batch.
        /// Empty inner Vecs and an empty outer Vec are intentionally
        /// generated to exercise the no-op edge cases on both engines.
        batches: Vec<Vec<GroupOp>>,
        /// Threaded through to bbolt's `db.NoSync` flip and
        /// redb's commit-time fsync. Macroaligned with `Commit`
        /// to keep the durability axis consistent.
        fsync: bool,
    },
}

/// One mutation inside a [`DiffOp::CommitGroup`] batch. Mirrors the
/// Go oracle's `groupOp` struct field-for-field (see
/// `benches/oracles/bbolt/main.go::groupOp`). Reads are intentionally
/// excluded — a read inside a write group would need its own txn
/// (forbidden by the single-writer invariant) and is never emitted
/// by the harness. Serde derives match [`DiffOp`] for the seed-
/// replay driver; byte fields use `base64_helper` for compactness.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum GroupOp {
    Put {
        bucket: u8,
        #[serde(with = "base64_helper")]
        key: Vec<u8>,
        #[serde(with = "base64_helper")]
        value: Vec<u8>,
    },
    Delete {
        bucket: u8,
        #[serde(with = "base64_helper")]
        key: Vec<u8>,
    },
    DeleteRange {
        bucket: u8,
        #[serde(with = "base64_helper")]
        start: Vec<u8>,
        #[serde(with = "base64_helper")]
        end: Vec<u8>,
    },
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
/// 3. `bbolt_dir` — remove the bbolt db file.
/// 4. `redb_dir` — remove the redb db file.
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
    bbolt_dir: Option<TempDir>,
    redb_dir: Option<TempDir>,
    /// Path to the prebuilt Go oracle binary, kept so
    /// `close_and_reopen` can respawn the subprocess. The binary is
    /// resolved once per test via `skip_without_oracle` and
    /// thread-safe to share by path.
    oracle_binary_path: PathBuf,
    /// fsync bit threaded into every commit and into the new
    /// `GoOracle` constructed by `close_and_reopen`. Captured at
    /// `Case::new` time so the close-reopen cycle is durability-
    /// neutral against the original spawn.
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
            bbolt_dir: Some(bbolt_dir),
            redb_dir: Some(redb_dir),
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

    /// Drop both engine handles, then respawn against the same on-disk
    /// state. Tests that durable writes survive a "process restart"
    /// (plan §3 axis B6 / §9 commit 8 step 5).
    ///
    /// Drop-then-reopen sequencing is load-bearing: `GoOracle::spawn`
    /// acquires bbolt's flock and `RedbBackend::open` calls
    /// `Database::create` (single-writer guard); both fail or hang if
    /// the previous owner is still alive. `Option::take()` drops the
    /// old handle *before* constructing the new one, eliminating the
    /// overlap that `mem::replace` would create.
    ///
    /// The `TempDir`s in `bbolt_dir` / `redb_dir` are intentionally
    /// not touched — the on-disk files must persist across the cycle
    /// for the durability assertion to mean anything. Buckets are
    /// re-registered on both sides because bbolt's `CreateBucket` and
    /// redb's `register_bucket` are idempotent ("already registered"
    /// is a no-op success), so this is cheap and keeps the post-reopen
    /// state byte-identical to a fresh `Case::new`.
    fn close_and_reopen(&mut self) -> Result<(), String> {
        // Drop oracle first, then redb — same order as struct-field
        // drop order, so no flock/lock-overlap is possible.
        drop(self.oracle.take());
        drop(self.redb.take());

        let bbolt_dir = self
            .bbolt_dir
            .as_ref()
            .expect("bbolt_dir slot non-empty")
            .path();
        let redb_dir = self
            .redb_dir
            .as_ref()
            .expect("redb_dir slot non-empty")
            .path();
        let db_path = bbolt_dir.join("oracle.db");

        let mut oracle = GoOracle::spawn(&self.oracle_binary_path, &db_path, self.fsync)
            .map_err(|e| format!("oracle respawn: {e}"))?;
        let redb = RedbBackend::open(BackendConfig::new(redb_dir.to_path_buf(), false))
            .map_err(|e| format!("redb reopen: {e}"))?;

        for (idx, name) in BUCKET_NAMES.iter().enumerate() {
            let id = BucketId::new((idx + 1) as u16);
            let resp = oracle
                .call(&json!({"op":"bucket","name":name}))
                .map_err(|e| format!("oracle bucket {name} (reopen): {e}"))?;
            require_ok(&resp, &format!("bucket {name} (reopen)")).map_err(|e| e.to_string())?;
            redb.register_bucket(name, id)
                .map_err(|e| format!("redb register_bucket {name} (reopen): {e}"))?;
        }

        self.oracle = Some(oracle);
        self.redb = Some(redb);
        Ok(())
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
        DiffOp::PutNilKey { bucket, value } => {
            ensure_txn(case, state)?;
            let bucket_id = bucket_id_of(*bucket);
            let bucket_name = bucket_name_of(*bucket);
            let redb_res = state
                .pending
                .as_mut()
                .expect("ensure_txn left pending unset")
                .put(bucket_id, b"", value);
            let resp = case
                .oracle_mut()
                .call(&json!({
                    "op":"put","bucket":bucket_name,
                    "key":b64(b""),"value":b64(value),
                }))
                .map_err(|e| format!("oracle put_nil_key: {e}"))?;
            let oracle_err = oracle_error(&resp);
            match (redb_res, oracle_err) {
                (Err(e), Some(oe)) => {
                    let redb_norm = normalize_err(&e.to_string());
                    let oracle_norm = normalize_err(&oe);
                    if redb_norm != oracle_norm {
                        return Err(format!(
                            "symmetric put_nil_key error but normalized strings diverge: \
                             redb={redb_norm:?} (raw={e}), oracle={oracle_norm:?} (raw={oe})"
                        ));
                    }
                    Ok(())
                }
                (Ok(()), None) => {
                    Err("divergence on put_nil_key: both engines accepted an empty key".to_owned())
                }
                (Ok(()), Some(oe)) => Err(format!(
                    "divergence on put_nil_key: redb accepted, oracle rejected ({oe})"
                )),
                (Err(e), None) => Err(format!(
                    "divergence on put_nil_key: redb rejected ({e}), oracle accepted"
                )),
            }
        }
        DiffOp::CloseReopen => {
            // Discard the pending batch on the Rust side — close
            // throws away any uncommitted state on the oracle child
            // (its in-memory bbolt txn dies with the process), so
            // mirroring on the Rust side keeps the two state machines
            // in lockstep. No fsync path, cannot fail.
            state.pending = None;
            case.close_and_reopen()?;
            let (redb, oracle) = case.split_redb_and_oracle();
            snapshot_and_diff(redb, oracle)?;
            Ok(())
        }
        DiffOp::CommitGroup { batches, fsync } => {
            // commit_group requires no active txn on both engines.
            // Roll back the pending batch symmetrically before the
            // call so the operation under test is the multi-batch
            // commit, not a txn-active error path.
            if state.pending.take().is_some() {
                let resp = case
                    .oracle_mut()
                    .call(&json!({"op":"rollback"}))
                    .map_err(|e| format!("oracle pre-commit_group rollback: {e}"))?;
                require_ok(&resp, "pre-commit_group rollback").map_err(|e| e.to_string())?;
            }

            // Build the JSON wire shape for the oracle's
            // `[][]groupOp` field. Done before any redb staging so
            // a JSON build error doesn't leave us with an
            // open-ended state on either side.
            let json_batches: Vec<Vec<Value>> = batches
                .iter()
                .map(|inner| {
                    inner
                        .iter()
                        .map(|op| match op {
                            GroupOp::Put { bucket, key, value } => json!({
                                "op":"put",
                                "bucket": bucket_name_of(*bucket),
                                "key": b64(key),
                                "value": b64(value),
                            }),
                            GroupOp::Delete { bucket, key } => json!({
                                "op":"delete",
                                "bucket": bucket_name_of(*bucket),
                                "key": b64(key),
                            }),
                            GroupOp::DeleteRange { bucket, start, end } => json!({
                                "op":"delete_range",
                                "bucket": bucket_name_of(*bucket),
                                "start": b64(start),
                                "end": b64(end),
                            }),
                        })
                        .collect()
                })
                .collect();

            // Stage each batch on redb. We collect into a Vec rather
            // than a streaming iterator because Backend::commit_group
            // takes ownership.
            let mut redb_batches = Vec::with_capacity(batches.len());
            let mut staging_err: Option<BackendError> = None;
            'staging: for inner in batches {
                let mut b = match case.redb().begin_batch() {
                    Ok(b) => b,
                    Err(e) => {
                        staging_err = Some(e);
                        break 'staging;
                    }
                };
                for op in inner {
                    let bucket_id = match op {
                        GroupOp::Put { bucket, .. }
                        | GroupOp::Delete { bucket, .. }
                        | GroupOp::DeleteRange { bucket, .. } => bucket_id_of(*bucket),
                    };
                    let res = match op {
                        GroupOp::Put { key, value, .. } => b.put(bucket_id, key, value),
                        GroupOp::Delete { key, .. } => b.delete(bucket_id, key),
                        GroupOp::DeleteRange { start, end, .. } => {
                            b.delete_range(bucket_id, start, end)
                        }
                    };
                    if let Err(e) = res {
                        staging_err = Some(e);
                        break 'staging;
                    }
                }
                redb_batches.push(b);
            }

            let redb_res = if let Some(e) = staging_err {
                Err(e)
            } else {
                rt.block_on(case.redb().commit_group(redb_batches))
                    .map(|_| ())
            };
            let resp = case
                .oracle_mut()
                .call(&json!({
                    "op":"commit_group",
                    "fsync": *fsync,
                    "batches": json_batches,
                }))
                .map_err(|e| format!("oracle commit_group: {e}"))?;
            let oracle_err = oracle_error(&resp);
            match (redb_res, oracle_err) {
                (Ok(()), None) => {}
                (Err(e), Some(oe)) => {
                    let redb_norm = normalize_err(&e.to_string());
                    let oracle_norm = normalize_err(&oe);
                    if redb_norm != oracle_norm {
                        return Err(format!(
                            "symmetric commit_group error but normalized strings diverge: \
                             redb={redb_norm:?} (raw={e}), oracle={oracle_norm:?} (raw={oe})"
                        ));
                    }
                }
                (Ok(()), Some(oe)) => {
                    return Err(format!(
                        "divergence on commit_group: redb ok, oracle err={oe}"
                    ));
                }
                (Err(e), None) => {
                    return Err(format!(
                        "divergence on commit_group: redb err={e}, oracle ok"
                    ));
                }
            }
            let (redb, oracle) = case.split_redb_and_oracle();
            snapshot_and_diff(redb, oracle)?;
            Ok(())
        }
        DiffOp::Defragment => {
            // Both engines reject defrag/compact under an active txn.
            // Roll the pending batch back symmetrically before the
            // call so the operation under test is the actual
            // defragmentation, not the txn-active error path. Drop
            // the redb staged ops (no fsync, infallible) and tell
            // the oracle to rollback (no-op on the oracle if its
            // child has no active txn).
            if state.pending.take().is_some() {
                let resp = case
                    .oracle_mut()
                    .call(&json!({"op":"rollback"}))
                    .map_err(|e| format!("oracle pre-defrag rollback: {e}"))?;
                require_ok(&resp, "pre-defrag rollback").map_err(|e| e.to_string())?;
            }
            let redb_res = rt.block_on(case.redb().defragment());
            let resp = case
                .oracle_mut()
                .call(&json!({"op":"compact"}))
                .map_err(|e| format!("oracle compact: {e}"))?;
            let oracle_err = oracle_error(&resp);
            match (redb_res, oracle_err) {
                (Ok(()), None) => {}
                (Err(e), Some(oe)) => {
                    let redb_norm = normalize_err(&e.to_string());
                    let oracle_norm = normalize_err(&oe);
                    if redb_norm != oracle_norm {
                        return Err(format!(
                            "symmetric defrag error but normalized strings diverge: \
                             redb={redb_norm:?} (raw={e}), oracle={oracle_norm:?} (raw={oe})"
                        ));
                    }
                }
                (Ok(()), Some(oe)) => {
                    return Err(format!("divergence on defrag: redb ok, oracle err={oe}"));
                }
                (Err(e), None) => {
                    return Err(format!("divergence on defrag: redb err={e}, oracle ok"));
                }
            }
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

fn close_reopen_strat() -> Just<DiffOp> {
    Just(DiffOp::CloseReopen)
}

/// Generates a `PutNilKey` op — empty key, non-empty value, random
/// bucket. Pinned to value-`1..=16` (the same key alphabet) so the
/// rejection path is the only error axis under test.
fn put_nil_key_strat() -> impl Strategy<Value = DiffOp> {
    (bucket_idx(), value_bytes()).prop_map(|(bucket, value)| DiffOp::PutNilKey { bucket, value })
}

fn defragment_strat() -> Just<DiffOp> {
    Just(DiffOp::Defragment)
}

/// One inner op inside a [`DiffOp::CommitGroup`] batch. Reuses the
/// same key/value/bucket alphabets as the top-level Put/Delete/
/// `DeleteRange` strategies, so the multi-batch path exercises the
/// same byte distributions as the single-batch path.
fn group_op_strat() -> impl Strategy<Value = GroupOp> {
    prop_oneof![
        70 => (bucket_idx(), key_bytes(), value_bytes())
            .prop_map(|(bucket, key, value)| GroupOp::Put { bucket, key, value }),
        20 => (bucket_idx(), key_bytes())
            .prop_map(|(bucket, key)| GroupOp::Delete { bucket, key }),
        10 => (bucket_idx(), key_bytes(), key_bytes())
            .prop_map(|(bucket, a, b)| {
                let (start, end) = if a <= b { (a, b) } else { (b, a) };
                GroupOp::DeleteRange { bucket, start, end }
            }),
    ]
}

/// Generates a `CommitGroup` op. Outer Vec length `0..=3` covers
/// the empty-group edge case and small groupings; inner Vec length
/// `0..=4` covers the empty-batch case (a legal no-op on both
/// engines per the bbolt source) and small batches. Per-case op
/// budget capped well below redb/bbolt's group-size limits to keep
/// runtime predictable.
fn commit_group_strat() -> impl Strategy<Value = DiffOp> {
    (
        proptest::collection::vec(proptest::collection::vec(group_op_strat(), 0..=4), 0..=3),
        any::<bool>(),
    )
        .prop_map(|(batches, fsync)| DiffOp::CommitGroup { batches, fsync })
}

/// Per-op strategy. All advanced ops (`CommitGroup`, `Defragment`,
/// `CloseReopen`, `PutNilKey`) now wired in.
///
/// Put 44 / Delete 19 / `DeleteRange` 5 / Commit 20 / Rollback 5 /
/// `CloseReopen` 2 / `PutNilKey` 2 / `Defragment` 1 / `CommitGroup`
/// 2 = total 100. `CommitGroup` weight kept small (2 %) because each
/// fire stages multiple inner ops at once — its bug-finding power
/// per *fire* is high but per *inner op* is comparable to the single-
/// batch `Commit` path. Two percent over a length-1..=40 sequence
/// yields ~0.8 fires per case on average — enough to exercise the
/// multi-batch atomicity path frequently across the 256-case sweep
/// without dominating runtime.
fn op_strat() -> impl Strategy<Value = DiffOp> {
    prop_oneof![
        44 => put_strat(),
        19 => delete_strat(),
        5  => delete_range_strat(),
        20 => commit_strat(),
        5  => rollback_strat(),
        2  => close_reopen_strat(),
        2  => put_nil_key_strat(),
        1  => defragment_strat(),
        2  => commit_group_strat(),
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

/// Round-trip every [`DiffOp`] variant through `serde_json` and back.
/// Locks in the seed-file wire format (plan §9 commit 9 step 6) — a
/// future structural change to `DiffOp` that breaks deserialization
/// of older seed files would silently turn `replay_committed_seeds`
/// into a no-op; this test fails loud first.
///
/// Bytes are explicitly chosen to exercise the base64 path: NUL,
/// newline, and high-bit bytes that JSON would otherwise mangle in
/// the default `Vec<u8>` encoding.
#[test]
fn diff_op_serde_round_trip_every_variant() {
    let cases: Vec<DiffOp> = vec![
        DiffOp::Put {
            bucket: 0,
            key: b"\x00\nk".to_vec(),
            value: b"\xffv".to_vec(),
        },
        DiffOp::Delete {
            bucket: 1,
            key: b"k".to_vec(),
        },
        DiffOp::DeleteRange {
            bucket: 2,
            start: Vec::new(),
            end: vec![0xff],
        },
        DiffOp::Commit { fsync: true },
        DiffOp::Rollback,
        DiffOp::CloseReopen,
        DiffOp::PutNilKey {
            bucket: 0,
            value: b"v".to_vec(),
        },
        DiffOp::Defragment,
        DiffOp::CommitGroup {
            batches: vec![
                vec![
                    GroupOp::Put {
                        bucket: 0,
                        key: b"g".to_vec(),
                        value: b"v".to_vec(),
                    },
                    GroupOp::Delete {
                        bucket: 1,
                        key: b"d".to_vec(),
                    },
                ],
                vec![GroupOp::DeleteRange {
                    bucket: 2,
                    start: b"a".to_vec(),
                    end: Vec::new(),
                }],
            ],
            fsync: false,
        },
    ];
    for op in &cases {
        let s = serde_json::to_string(op).expect("serialize");
        let back: DiffOp = serde_json::from_str(&s).expect("deserialize");
        // Re-serialize and compare strings: structural equality without
        // adding a `PartialEq` derive on `DiffOp` (which would require
        // the same on `GroupOp` and start a chain of implementation
        // burdens for a test-only convenience).
        let s2 = serde_json::to_string(&back).expect("re-serialize");
        assert_eq!(s, s2, "round-trip diverged for {op:?}");
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

/// Deterministic regression test for every advanced op variant
/// (`CommitGroup`, `CloseReopen`, `Defragment`, `PutNilKey`).
/// Independent of proptest — guarantees these paths are exercised
/// every test run even if a proptest config change zeros their
/// weights. Each op is followed by enough state-mutating context
/// to make a divergence observable (writes before/after, a final
/// commit + snapshot diff).
#[test]
fn smoke_advanced_ops_no_divergence() {
    let Some(binary) = skip_without_oracle("smoke_advanced_ops_no_divergence") else {
        return;
    };
    let ops = vec![
        // Seed: two committed keys so Defragment/CloseReopen have
        // real state to preserve.
        DiffOp::Put {
            bucket: 0,
            key: b"k1".to_vec(),
            value: b"v1".to_vec(),
        },
        DiffOp::Put {
            bucket: 1,
            key: b"k2".to_vec(),
            value: b"v2".to_vec(),
        },
        DiffOp::Commit { fsync: false },
        // PutNilKey: both engines must reject with normalized
        // "empty key". A successful put on either side is divergence.
        DiffOp::PutNilKey {
            bucket: 0,
            value: b"v".to_vec(),
        },
        DiffOp::Commit { fsync: false },
        // CommitGroup with a non-empty multi-batch group.
        DiffOp::CommitGroup {
            batches: vec![
                vec![
                    GroupOp::Put {
                        bucket: 0,
                        key: b"g1".to_vec(),
                        value: b"a".to_vec(),
                    },
                    GroupOp::Delete {
                        bucket: 0,
                        key: b"k1".to_vec(),
                    },
                ],
                vec![GroupOp::Put {
                    bucket: 2,
                    key: b"g2".to_vec(),
                    value: b"b".to_vec(),
                }],
            ],
            fsync: false,
        },
        // CommitGroup edge case: empty outer Vec is a legal no-op.
        DiffOp::CommitGroup {
            batches: vec![],
            fsync: false,
        },
        // Defragment: post-state must remain byte-identical.
        DiffOp::Defragment,
        // CloseReopen: durability across a "process restart". The
        // committed state from above must survive intact.
        DiffOp::CloseReopen,
        // Final write + commit so the snapshot-diff has a fresh
        // mutation to compare across the full op sequence.
        DiffOp::Put {
            bucket: 1,
            key: b"k3".to_vec(),
            value: b"v3".to_vec(),
        },
        DiffOp::Commit { fsync: false },
    ];
    run_case(&binary, &ops).expect("smoke advanced ops diverged");
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
