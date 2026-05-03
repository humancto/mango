//! Subprocess driver for the bbolt oracle in `--mode=bench`
//! (ROADMAP:829, plan §"bbolt oracle protocol — B1" and commit 9
//! in `.planning/parity-bench-harness.plan.md`).
//!
//! ## Wire protocol
//!
//! One JSON object per line, request/reply, single in-flight: the
//! caller MUST read each response before sending the next request.
//! The Go side enforces this via `bufio.Scanner` on stdin and one
//! synchronous `dispatchBench` call per line.
//!
//! Op set (mirrors `benches/oracles/bbolt/bench.go::dispatchBench`):
//!
//! - `bench_open`         — handshake; opens the .db file
//! - `bench_close`        — graceful shutdown signal (Go side returns
//!   from its main loop after replying)
//! - `bench_load`         — batch put
//! - `bench_get_seq`      — point reads, per-op latency histogram
//! - `bench_get_zipfian`  — same shape as `get_seq` (caller pre-shapes
//!   the key list per the distribution)
//! - `bench_range`        — bounded scan with force-copy + xor-fold
//!   checksum (proves bbolt's mmap reads are not elided —
//!   fairness invariant for S3)
//! - `bench_size`         — `os.Stat(path).Size()` after `db.Sync()`
//!
//! ## Why this lives in the harness crate (not the storage crate)
//!
//! The diff-mode driver (`crates/mango-storage/tests/
//! differential_vs_bbolt.rs::GoOracle`) talks the same JSON-over-pipes
//! protocol but a totally different op set. The two share no fields
//! and would only get tangled if hoisted into a common module. They
//! also have different lifetime profiles: diff is one-shot per
//! property test, bench is one long-lived session per run.
//!
//! ## Stderr capture
//!
//! Stderr is drained into a 1 MiB ring buffer by a non-joining
//! background thread, identical pattern to the diff-mode driver
//! (see `differential_vs_bbolt.rs` for the rationale chain — pipe
//! back-pressure beats user-space size cap as the failure mode).
//! [`BboltOracle::stderr_snapshot`] returns a copy for divergence
//! reports.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::measure::LatencyHistogram;

/// Environment-variable override for the oracle binary path.
/// Tests skip cleanly when neither this nor the relative-path
/// fallback resolve.
pub const ORACLE_ENV: &str = "MANGO_BBOLT_ORACLE";

/// Workspace-relative oracle binary path (from the harness crate
/// manifest dir). The harness lives at `benches/storage/`; the
/// oracle binary at `benches/oracles/bbolt/bbolt-oracle`, hence one
/// `..` hop.
pub const ORACLE_REL: &str = "../oracles/bbolt/bbolt-oracle";

/// 16 MiB stdout-read buffer. Matches the Go oracle's
/// `bufio.Scanner` cap so any single response (notably `hist_b64`
/// for very-long bench runs) cannot exceed both ends symmetrically.
const STDOUT_BUF_CAP: usize = 16 << 20;

/// 1 MiB stderr ring buffer. Holds the tail of any reasonable Go
/// panic + stack trace; the drainer's `pop_front` eviction keeps
/// memory bounded under bursty stderr.
const STDERR_RING_CAP: usize = 1 << 20;

/// Bench-mode wire request. Field set is one-to-one with the Go
/// side's `benchRequest` (see `benches/oracles/bbolt/bench.go`).
/// Every field except `op` is `Option`-skipped on the wire.
#[derive(Debug, Clone, Serialize)]
struct BenchRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    op: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fsync: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pairs: Option<&'a [[String; 2]]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    batch_size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keys: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theta: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<usize>,
}

/// Bench-mode wire response. Field-for-field with the Go
/// `benchResponse`. Every numeric field is `omitempty` on the wire,
/// hence `Option<_>` here.
#[derive(Debug, Clone, Default, Deserialize)]
struct BenchResponse {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    elapsed_ns: Option<i64>,
    #[serde(default)]
    ops: Option<i64>,
    #[serde(default)]
    rows: Option<i64>,
    #[serde(default)]
    hist_b64: Option<String>,
    #[serde(default)]
    checksum: Option<u64>,
    #[serde(default)]
    bytes: Option<i64>,
}

/// Outcome of a `bench_load` call.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LoadOutcome {
    /// Wall-clock for the whole load batch, nanoseconds.
    pub elapsed_ns: u64,
    /// Number of pairs the oracle reported it wrote. Should equal
    /// the input length.
    pub ops: u64,
}

/// Outcome of a `bench_get_seq` / `bench_get_zipfian` call. The
/// histogram is decoded from the V2-deflate base64 the Go side
/// emits.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GetOutcome {
    /// Wall-clock for the whole get batch.
    pub elapsed_ns: u64,
    /// Number of reads the oracle performed.
    pub ops: u64,
    /// Per-op latency histogram (decoded from `hist_b64`).
    pub histogram: LatencyHistogram,
}

/// Outcome of a `bench_range` call. The `checksum` field MUST be
/// non-zero on a non-empty range — see plan §S3 N8 (proves Go's
/// escape analysis didn't elide the per-row copy).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RangeOutcome {
    /// Wall-clock for the scan.
    pub elapsed_ns: u64,
    /// Number of rows visited (after the `limit` cap if any).
    pub rows: u64,
    /// xor-fold of `(k[0]) | (v[0] << 8)` over each visited row.
    pub checksum: u64,
}

/// Outcome of a `bench_size` call.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SizeOutcome {
    /// `os.Stat(path).Size()` after `db.Sync()`.
    pub bytes: u64,
}

/// Handle to a running bbolt oracle subprocess in `--mode=bench`.
///
/// Owns the child's stdin/stdout pipes. Each public op method
/// writes one JSON request line and reads exactly one JSON
/// response line; protocol is strictly request/reply (one
/// in-flight). The drop impl does a best-effort `bench_close`
/// followed by a 500 ms graceful-exit window, then SIGKILL — the
/// child cannot be left stranded if a test panics.
pub struct BboltOracle {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    /// Monotonically increasing id, echoed by the oracle. Used for
    /// reply-skew detection in `call`.
    next_id: u64,
    /// Shared ring buffer of the child's stderr. Cloned into the
    /// drainer thread; the thread is detached and exits naturally
    /// when the child closes its stderr on `kill` / `wait`.
    stderr_buf: Arc<Mutex<VecDeque<u8>>>,
}

/// Locate the oracle binary without panicking. Order:
/// 1. `MANGO_BBOLT_ORACLE` env var — if set but bad, returns `None`
///    (caller decides whether to surface that loudly).
/// 2. Workspace-relative fallback `../oracles/bbolt/bbolt-oracle`.
/// 3. Otherwise `None`.
#[must_use]
pub fn oracle_binary_opt() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(ORACLE_ENV) {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
        return None;
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest_dir.join(ORACLE_REL);
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

impl BboltOracle {
    /// Spawn the oracle as `--mode=bench`. Does NOT call
    /// `bench_open`; the caller invokes [`Self::open`] explicitly so
    /// the spawn step is decoupled from db-path selection.
    pub fn spawn(binary: &Path) -> io::Result<Self> {
        let mut child = Command::new(binary)
            .args(["--mode=bench"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("child stdin pipe missing"))?,
        );
        let stdout = BufReader::with_capacity(
            STDOUT_BUF_CAP,
            child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("child stdout pipe missing"))?,
        );
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("child stderr pipe missing"))?;
        let stderr_buf: Arc<Mutex<VecDeque<u8>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_RING_CAP)));
        spawn_stderr_drainer(stderr, Arc::clone(&stderr_buf));

        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 0,
            stderr_buf,
        })
    }

    /// Snapshot of the captured-stderr ring buffer. Used by callers
    /// to enrich divergence / failure dumps.
    #[must_use]
    pub fn stderr_snapshot(&self) -> Vec<u8> {
        let g = self.stderr_buf.lock();
        g.iter().copied().collect()
    }

    /// Send `bench_open` for `db_path`. The Go side pre-creates
    /// the `bench` bucket so the timed path doesn't include bucket
    /// creation.
    pub fn open(&mut self, db_path: &Path, fsync: bool) -> io::Result<()> {
        let path = db_path
            .to_str()
            .ok_or_else(|| io::Error::other("db_path not UTF-8"))?;
        let resp = self.call(&BenchRequest {
            id: None,
            op: "bench_open",
            path: Some(path),
            fsync: Some(fsync),
            pairs: None,
            batch_size: None,
            keys: None,
            theta: None,
            start: None,
            end: None,
            limit: None,
        })?;
        require_ok(&resp, "bench_open")
    }

    /// Send `bench_close`. The Go side returns from its dispatch
    /// loop after replying, so subsequent calls on this handle
    /// will fail. Idempotent within the drop impl.
    pub fn close(&mut self) -> io::Result<()> {
        let resp = self.call(&BenchRequest {
            id: None,
            op: "bench_close",
            path: None,
            fsync: None,
            pairs: None,
            batch_size: None,
            keys: None,
            theta: None,
            start: None,
            end: None,
            limit: None,
        })?;
        require_ok(&resp, "bench_close")
    }

    /// Send `bench_load`: batch put `pairs` with `commit_group(N)`-
    /// equivalent batching at `batch_size`.
    pub fn load(
        &mut self,
        pairs: &[(Vec<u8>, Vec<u8>)],
        batch_size: usize,
    ) -> io::Result<LoadOutcome> {
        let encoded: Vec<[String; 2]> = pairs
            .iter()
            .map(|(k, v)| [BASE64_STANDARD.encode(k), BASE64_STANDARD.encode(v)])
            .collect();
        let resp = self.call(&BenchRequest {
            id: None,
            op: "bench_load",
            path: None,
            fsync: None,
            pairs: Some(&encoded),
            batch_size: Some(batch_size),
            keys: None,
            theta: None,
            start: None,
            end: None,
            limit: None,
        })?;
        require_ok(&resp, "bench_load")?;
        Ok(LoadOutcome {
            elapsed_ns: nonneg_u64(resp.elapsed_ns, "bench_load.elapsed_ns")?,
            ops: nonneg_u64(resp.ops, "bench_load.ops")?,
        })
    }

    /// Send `bench_get_seq`: read each key in `keys` order. Captures
    /// per-op latency in an `HdrHistogram` on the Go side and ships
    /// it back as V2-deflate base64.
    pub fn get_seq(&mut self, keys: &[Vec<u8>]) -> io::Result<GetOutcome> {
        self.get(keys, "bench_get_seq", None)
    }

    /// Send `bench_get_zipfian`: same response shape as `get_seq`.
    /// `theta` is recorded for provenance; the keys are pre-shaped
    /// by the harness before being passed in.
    pub fn get_zipfian(&mut self, keys: &[Vec<u8>], theta: f64) -> io::Result<GetOutcome> {
        self.get(keys, "bench_get_zipfian", Some(theta))
    }

    fn get(&mut self, keys: &[Vec<u8>], op: &str, theta: Option<f64>) -> io::Result<GetOutcome> {
        let encoded: Vec<String> = keys.iter().map(|k| BASE64_STANDARD.encode(k)).collect();
        let resp = self.call(&BenchRequest {
            id: None,
            op,
            path: None,
            fsync: None,
            pairs: None,
            batch_size: None,
            keys: Some(&encoded),
            theta,
            start: None,
            end: None,
            limit: None,
        })?;
        require_ok(&resp, op)?;
        let hist_b64 = resp
            .hist_b64
            .ok_or_else(|| io::Error::other(format!("{op}: response missing hist_b64")))?;
        let histogram = LatencyHistogram::from_base64_v2_deflate(&hist_b64)
            .map_err(|e| io::Error::other(format!("{op}: hist_b64 decode: {e}")))?;
        Ok(GetOutcome {
            elapsed_ns: nonneg_u64(resp.elapsed_ns, "elapsed_ns")?,
            ops: nonneg_u64(resp.ops, "ops")?,
            histogram,
        })
    }

    /// Send `bench_range`: half-open scan `[start, end)`, capped at
    /// `limit` rows (0 = no cap). The returned `checksum` is the
    /// xor-fold of the first byte of each row's key+value — the
    /// caller MUST assert it's non-zero on non-empty ranges (S3 N8
    /// fairness invariant).
    pub fn range(&mut self, start: &[u8], end: &[u8], limit: usize) -> io::Result<RangeOutcome> {
        let s = BASE64_STANDARD.encode(start);
        let e = BASE64_STANDARD.encode(end);
        let resp = self.call(&BenchRequest {
            id: None,
            op: "bench_range",
            path: None,
            fsync: None,
            pairs: None,
            batch_size: None,
            keys: None,
            theta: None,
            start: Some(&s),
            end: Some(&e),
            limit: Some(limit),
        })?;
        require_ok(&resp, "bench_range")?;
        Ok(RangeOutcome {
            elapsed_ns: nonneg_u64(resp.elapsed_ns, "bench_range.elapsed_ns")?,
            rows: nonneg_u64(resp.rows, "bench_range.rows")?,
            checksum: resp.checksum.unwrap_or(0),
        })
    }

    /// Send `bench_size`: post-`Sync` on-disk `os.Stat` size.
    pub fn size(&mut self) -> io::Result<SizeOutcome> {
        let resp = self.call(&BenchRequest {
            id: None,
            op: "bench_size",
            path: None,
            fsync: None,
            pairs: None,
            batch_size: None,
            keys: None,
            theta: None,
            start: None,
            end: None,
            limit: None,
        })?;
        require_ok(&resp, "bench_size")?;
        Ok(SizeOutcome {
            bytes: nonneg_u64(resp.bytes, "bench_size.bytes")?,
        })
    }

    /// Write one request line, read one response line. Auto-injects
    /// `id`. Errors propagate as `io::Error::other` so the call
    /// sites stay terse.
    fn call(&mut self, req: &BenchRequest<'_>) -> io::Result<BenchResponse> {
        self.next_id = self.next_id.wrapping_add(1);
        let with_id = BenchRequest {
            id: Some(self.next_id),
            ..req.clone()
        };
        let line = serde_json::to_string(&with_id).map_err(io::Error::other)?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        let mut buf = String::new();
        let n = self.stdout.read_line(&mut buf)?;
        if n == 0 {
            return Err(io::Error::other(format!(
                "oracle closed stdout unexpectedly during op={}",
                req.op,
            )));
        }
        serde_json::from_str(buf.trim_end()).map_err(io::Error::other)
    }
}

impl Drop for BboltOracle {
    fn drop(&mut self) {
        // Best-effort graceful close: send `bench_close`, ignore
        // every failure. Drop MUST NOT panic — a panic during
        // unwind aborts the process.
        let _ = self.stdin.write_all(br#"{"op":"bench_close"}"#);
        let _ = self.stdin.write_all(b"\n");
        let _ = self.stdin.flush();

        // Poll for graceful exit up to 500 ms; SIGKILL after that.
        // The bench-close path is O(1) on the Go side (one
        // `db.Close()` and the dispatcher returns).
        #[allow(
            clippy::arithmetic_side_effects,
            reason = "Instant + Duration is checked at runtime by std and \
                      cannot meaningfully overflow on a 500ms add from now"
        )]
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

fn require_ok(resp: &BenchResponse, context: &str) -> io::Result<()> {
    if resp.ok {
        return Ok(());
    }
    let err = resp.error.as_deref().unwrap_or("<no error field>");
    Err(io::Error::other(format!(
        "{context}: ok=false, error={err}"
    )))
}

fn nonneg_u64(field: Option<i64>, name: &str) -> io::Result<u64> {
    let v = field.ok_or_else(|| io::Error::other(format!("response missing {name}")))?;
    u64::try_from(v).map_err(|_| io::Error::other(format!("{name}: negative value {v}")))
}

/// Detached drainer thread. Reads stderr in 4 KiB chunks and pushes
/// into the shared ring buffer with `pop_front` eviction once over
/// the cap. The thread exits when the child closes its stderr.
fn spawn_stderr_drainer(mut stderr: ChildStderr, buf: Arc<Mutex<VecDeque<u8>>>) {
    std::thread::Builder::new()
        .name("bbolt-bench-stderr".into())
        .spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        let slice = chunk.get(..n).unwrap_or(&[]);
                        let mut g = buf.lock();
                        g.extend(slice.iter().copied());
                        while g.len() > STDERR_RING_CAP {
                            g.pop_front();
                        }
                    }
                }
            }
        })
        .unwrap_or_else(|e| {
            // Drainer-thread spawn is non-recoverable: without it the
            // child's stderr backs up, eventually deadlocking the
            // protocol. Aborting here is correct behaviour.
            //
            // `expect` would fire a clippy lint workspace-wide; we
            // achieve the same effect by panicking explicitly.
            #[allow(
                clippy::panic,
                reason = "drainer thread spawn failure has no recovery path"
            )]
            {
                panic!("spawn bbolt-bench-stderr drainer thread: {e}")
            }
        });
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use super::*;

    /// Same skip pattern as `crates/mango-storage/tests/
    /// differential_vs_bbolt.rs::skip_without_oracle`. The default
    /// `cargo test` job does not build the Go oracle; tests
    /// requiring it print `SKIP:` and pass.
    fn skip_without_oracle(test_name: &str) -> Option<PathBuf> {
        if let Some(p) = oracle_binary_opt() {
            return Some(p);
        }
        // Workspace clippy denies `print_stderr`, so use io::Write
        // directly — same bypass the run.rs binary uses.
        let mut err = std::io::stderr();
        let _ = writeln!(
            err,
            "{test_name}: SKIP — bbolt oracle binary not built. \
             Run `cd benches/oracles/bbolt && ./build.sh` (or set \
             {ORACLE_ENV}) to enable."
        );
        None
    }

    /// Spawn → handshake (`bench_open`) → graceful close. The
    /// minimum lifecycle assertion required by the plan's test
    /// strategy table (`bbolt_runner.rs unit tests`).
    #[test]
    fn spawn_open_close_lifecycle() {
        let Some(binary) = skip_without_oracle("spawn_open_close_lifecycle") else {
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("bench.db");

        let mut oracle = BboltOracle::spawn(&binary).expect("spawn");
        oracle.open(&db, false).expect("open");
        oracle.close().expect("close");
    }

    /// Non-existent path → `bench_open` returns ok=false and we
    /// surface an `io::Error`. Verifies the protocol error path
    /// rather than the Rust-side spawn path.
    #[test]
    fn open_nonexistent_directory_errors() {
        let Some(binary) = skip_without_oracle("open_nonexistent_directory_errors") else {
            return;
        };
        let mut oracle = BboltOracle::spawn(&binary).expect("spawn");
        // bbolt creates the .db file but won't create missing
        // parent dirs; pointing at a definitely-missing parent
        // exercises the bolt.Open error path.
        let bad = Path::new("/this/path/does/not/exist/bench.db");
        let err = oracle.open(bad, false).expect_err("open should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("bench_open") && msg.contains("ok=false"),
            "unexpected error message: {msg}"
        );
    }

    /// Round-trip a load + sequential get and verify the histogram
    /// decodes and carries samples — the wire-format contract with
    /// the Go side. Catches V2-deflate skew between
    /// `hdrhistogram-go` and the Rust `hdrhistogram` crate at the
    /// integration boundary (the dedicated cross-language fixture
    /// test in `tests/hdrhist_xlang.rs` will be added separately;
    /// this is the minimum smoke check that the runner can decode
    /// what the Go side emits).
    #[test]
    fn load_then_get_seq_round_trip() {
        let Some(binary) = skip_without_oracle("load_then_get_seq_round_trip") else {
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("bench.db");
        let mut oracle = BboltOracle::spawn(&binary).expect("spawn");
        oracle.open(&db, false).expect("open");

        let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..50_u8)
            .map(|i| (vec![i, i.wrapping_add(1)], vec![b'v', i]))
            .collect();
        let load = oracle.load(&pairs, 16).expect("load");
        assert_eq!(load.ops, 50, "load should report 50 ops");

        let keys: Vec<Vec<u8>> = pairs.iter().map(|(k, _)| k.clone()).collect();
        let got = oracle.get_seq(&keys).expect("get_seq");
        assert_eq!(got.ops, 50, "get_seq should report 50 ops");
        assert_eq!(
            got.histogram.count(),
            50,
            "histogram should carry 50 samples"
        );
        assert!(got.histogram.max_ns() > 0, "max latency must be positive");
        oracle.close().expect("close");
    }

    /// `bench_range` must return a non-zero checksum on a non-empty
    /// range — proves the Go-side force-copy in `benchOpRange` is
    /// alive (S3 fairness invariant). If escape analysis ever
    /// elides the copy, this test fires.
    #[test]
    fn range_checksum_is_non_zero_on_non_empty_scan() {
        let Some(binary) = skip_without_oracle("range_checksum_is_non_zero_on_non_empty_scan")
        else {
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("bench.db");
        let mut oracle = BboltOracle::spawn(&binary).expect("spawn");
        oracle.open(&db, false).expect("open");

        // Both key[0] and value[0] must be non-zero so the xor-fold
        // cannot land on zero by coincidence.
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = (1..=20_u8)
            .map(|i| (vec![i], vec![0xff_u8.wrapping_sub(i), b'v']))
            .collect();
        oracle.load(&pairs, 8).expect("load");

        let scan = oracle.range(&[1_u8], &[30_u8], 0).expect("range");
        assert_eq!(scan.rows, 20, "range should visit all 20 rows");
        assert_ne!(
            scan.checksum, 0,
            "non-empty range checksum was zero — fairness invariant broken \
             (Go escape analysis may have elided the per-row copy)"
        );
        oracle.close().expect("close");
    }

    /// `bench_size` after a load reports a positive byte count
    /// post-`Sync`.
    #[test]
    fn size_reports_positive_after_load() {
        let Some(binary) = skip_without_oracle("size_reports_positive_after_load") else {
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("bench.db");
        let mut oracle = BboltOracle::spawn(&binary).expect("spawn");
        oracle.open(&db, false).expect("open");
        let pairs = vec![(b"k".to_vec(), vec![b'v'; 1024])];
        oracle.load(&pairs, 1).expect("load");
        let sz = oracle.size().expect("size");
        assert!(sz.bytes > 0, "size after load was zero");
        oracle.close().expect("close");
    }

    /// Sanity check the constants haven't drifted. Catches a
    /// rebase that changes `ORACLE_ENV` or `ORACLE_REL` without
    /// touching the diff-mode driver in `crates/mango-storage/
    /// tests/differential_vs_bbolt.rs`, where the same names are
    /// duplicated for the sibling protocol. Drift here is a wire
    /// break.
    #[test]
    fn protocol_constants_frozen() {
        assert_eq!(ORACLE_ENV, "MANGO_BBOLT_ORACLE");
        assert_eq!(ORACLE_REL, "../oracles/bbolt/bbolt-oracle");
    }
}
