//! In-process driver for the `RedbBackend` (ROADMAP:829, plan
//! §"Workload spec" and commit 10 in
//! `.planning/parity-bench-harness.plan.md`). The symmetric
//! counterpart to [`crate::bbolt_runner::BboltOracle`]: every public
//! method has the same name, takes the same arguments, and returns
//! the same outcome type, so the orchestrator's paired-comparison
//! loop calls into the two engines through a shape-identical surface.
//!
//! ## Why an in-process runner (not another subprocess)
//!
//! The bbolt side has to be a subprocess because the engine is
//! Go. The mango side could be either, but in-process wins:
//!
//! - One fewer `serde_json`-over-pipes hop on the timed path; pipe
//!   RTT is non-zero relative to a 1 µs lookup.
//! - The `RedbBackend`'s async commit API is driven through a private
//!   `tokio` current-thread runtime; there is no IPC ceremony.
//! - Latency capture happens in the same address space as the
//!   measurement primitive — a `LatencyHistogram` records `Instant`
//!   deltas directly, no V2-deflate round-trip.
//!
//! ## Op set (mirrors [`crate::bbolt_runner`])
//!
//! - [`MangoRunner::open`] → opens a `RedbBackend`, registers the
//!   bench bucket
//! - [`MangoRunner::close`] → idempotent backend close
//! - [`MangoRunner::load`] → batch put with `commit_batch` per chunk
//! - [`MangoRunner::get_seq`] → per-op-timed point reads in caller order
//! - [`MangoRunner::get_zipfian`] → same as `get_seq`; `theta` is
//!   provenance only (the harness pre-shapes the key list)
//! - [`MangoRunner::range`] → bounded scan with the same xor-fold
//!   checksum as bbolt (S3 N8 fairness invariant)
//! - [`MangoRunner::size`] → `RedbBackend::size_on_disk`
//!
//! ## Fairness vs bbolt
//!
//! - **fsync.** bbolt is opened with `db.NoSync = !fsync`; redb is
//!   always `Durability::Immediate`. The harness passes `fsync=true`
//!   to bbolt for the parity setting; this runner always fsyncs. See
//!   plan §N5.
//! - **Per-row copy in `range`.** redb's `Range` iterator already
//!   yields `Bytes::copy_from_slice(...)` for both key and value
//!   (snapshot.rs:115 / snapshot.rs:120 — verified). The bbolt side
//!   force-copies via `append([]byte{}, ...)` and xor-folds the
//!   first byte of each k+v to keep the copy live against escape
//!   analysis. The mango side mirrors the same fold so the per-row
//!   work is symmetric down to the byte read. See plan §S3 N8.

use std::path::PathBuf;
use std::time::Instant;

use bytes::Bytes;
use mango_storage::{
    Backend, BackendConfig, BackendError, BucketId, ReadSnapshot, RedbBackend, WriteBatch,
};
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};

use crate::measure::LatencyHistogram;

/// The bench bucket name. Same string the bbolt oracle uses
/// (`benches/oracles/bbolt/bench.go::dispatchBench` opens a bucket
/// literally named `"bench"`), so both engines are exercised on
/// identically-named keyspace.
pub const BENCH_BUCKET_NAME: &str = "bench";

/// Bucket id for [`BENCH_BUCKET_NAME`]. The numeric value is
/// arbitrary — mango's `BucketId` is opaque — but it is hard-coded
/// to a hex constant so a leak into a log or failure dump is
/// recognizable. `0xb007` reads as "boot" and is the same kind of
/// well-known marker the registry uses elsewhere.
pub const BENCH_BUCKET_ID: BucketId = BucketId::new(0xb007);

/// Outcome of a [`MangoRunner::load`] call. Field shape mirrors
/// [`crate::bbolt_runner::LoadOutcome`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LoadOutcome {
    /// Wall-clock for the whole load batch, nanoseconds.
    pub elapsed_ns: u64,
    /// Number of pairs the runner wrote — should equal
    /// `pairs.len()`.
    pub ops: u64,
}

/// Outcome of a [`MangoRunner::get_seq`] / [`MangoRunner::get_zipfian`]
/// call. Mirror of [`crate::bbolt_runner::GetOutcome`]; the
/// histogram is captured directly in this address space (no
/// V2-deflate round-trip).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GetOutcome {
    /// Wall-clock for the whole get loop.
    pub elapsed_ns: u64,
    /// Number of reads performed.
    pub ops: u64,
    /// Per-op latency histogram — pinned to the same parameters as
    /// the Go side via [`LatencyHistogram::new`].
    pub histogram: LatencyHistogram,
}

/// Outcome of a [`MangoRunner::range`] call. The `checksum` MUST be
/// non-zero on a non-empty scan — proves redb's per-row
/// `Bytes::copy_from_slice` is being read (S3 N8 fairness invariant
/// against the bbolt side).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RangeOutcome {
    /// Wall-clock for the scan.
    pub elapsed_ns: u64,
    /// Rows visited, after the `limit` cap if any.
    pub rows: u64,
    /// xor-fold of `(k[0]) | (v[0] << 8)` over each row. Exact
    /// same expression as the bbolt side's `bench_range` checksum.
    pub checksum: u64,
}

/// Outcome of a [`MangoRunner::size`] call.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SizeOutcome {
    /// Bytes reported by [`Backend::size_on_disk`]. Advisory per the
    /// trait contract — may lag an in-flight write txn — but the
    /// bench loop only calls it post-load.
    pub bytes: u64,
}

/// Errors produced by the mango runner. Wraps the storage
/// `BackendError` plus a small set of harness-specific failure
/// modes so the orchestrator can route on cause without depending
/// on string matching.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunnerError {
    /// The underlying [`Backend`] returned an error.
    #[error("backend: {0}")]
    Backend(#[from] BackendError),

    /// Building the private tokio runtime failed.
    #[error("tokio runtime: {0}")]
    Runtime(#[source] std::io::Error),

    /// Histogram construction or recording failed (logically
    /// unreachable; the pinned constants are validated by the
    /// histogram crate at construction).
    #[error("histogram: {0}")]
    Histogram(#[from] crate::measure::HistogramError),

    /// A caller-supplied parameter was outside the runner's accepted
    /// range — for example, `batch_size == 0` would cause the
    /// chunk-iterator to panic, so we reject it explicitly with the
    /// offending value reported.
    #[error("invalid argument: {0}")]
    InvalidArg(&'static str),

    /// Elapsed-nanosecond conversion `u128 -> u64` overflowed. Only
    /// reachable if a single batch took ~ 584 years — kept for
    /// completeness because the workspace bans `as u64` truncation
    /// and we'd rather fail loudly than silently wrap.
    #[error("elapsed nanoseconds did not fit in u64")]
    ElapsedOverflow,
}

/// Handle to a running mango bench session.
///
/// Owns the [`RedbBackend`], the resolved on-disk path (kept for
/// post-mortem dumps and `size_on_disk` cross-checks; the trait's
/// `size_on_disk` already covers the live read), and a private
/// current-thread `tokio` runtime that drives the async commit
/// methods. The runtime is current-thread because the bench loop
/// is single-threaded by construction — see plan §"Per-run isolation
/// (N4)".
pub struct MangoRunner {
    backend: RedbBackend,
    bucket: BucketId,
    rt: Runtime,
    data_dir: PathBuf,
}

impl MangoRunner {
    /// Open `data_dir` as a redb-backed mango bench session. Creates
    /// the directory if missing. `cache_size_bytes` is threaded into
    /// [`BackendConfig::with_cache_size`] when `Some`; pass `None`
    /// to use redb's default. The bench bucket is registered before
    /// return so the timed path does not include registration.
    pub fn open(data_dir: PathBuf, cache_size_bytes: Option<usize>) -> Result<Self, RunnerError> {
        let mut cfg = BackendConfig::new(data_dir.clone(), false);
        if let Some(bytes) = cache_size_bytes {
            cfg = cfg.with_cache_size(bytes);
        }
        let backend = RedbBackend::open(cfg)?;
        backend.register_bucket(BENCH_BUCKET_NAME, BENCH_BUCKET_ID)?;
        // current_thread runtime: spawn_blocking is available
        // without `rt-multi-thread`, and the bench harness never
        // spawns a second task. Same shape as the differential test
        // (`crates/mango-storage/tests/differential_vs_bbolt.rs`).
        let rt = RuntimeBuilder::new_current_thread()
            .build()
            .map_err(RunnerError::Runtime)?;
        Ok(Self {
            backend,
            bucket: BENCH_BUCKET_ID,
            rt,
            data_dir,
        })
    }

    /// On-disk data directory passed to [`Self::open`]. Useful for
    /// post-mortem artifact dumps.
    #[must_use]
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Idempotent close. After the first call, all read/write
    /// methods on the underlying backend return
    /// [`BackendError::Closed`]; the runner does NOT re-check on
    /// every op — the orchestrator owns the lifecycle.
    pub fn close(&self) -> Result<(), RunnerError> {
        self.backend.close()?;
        Ok(())
    }

    /// Batch put `pairs` with `batch_size` ops per `commit_batch`.
    /// Each commit fsyncs (`force_fsync = true`) — the parity setting
    /// against bbolt's `db.NoSync = false`. Wall-clock covers the
    /// whole load including the sync stream.
    pub fn load(
        &self,
        pairs: &[(Vec<u8>, Vec<u8>)],
        batch_size: usize,
    ) -> Result<LoadOutcome, RunnerError> {
        if batch_size == 0 {
            return Err(RunnerError::InvalidArg("batch_size must be > 0"));
        }
        let start = Instant::now();
        let mut ops: u64 = 0;
        for chunk in pairs.chunks(batch_size) {
            let mut batch = self.backend.begin_batch()?;
            for (k, v) in chunk {
                batch.put(self.bucket, k, v)?;
            }
            // The returned `CommitStamp` is intentionally unused —
            // the bench harness measures wall-clock and op counts,
            // not commit-cursor ordering. Bound the value with
            // `let _ = ...` to satisfy `#[must_use]` without
            // suppressing the lint workspace-wide.
            let _ = self.rt.block_on(self.backend.commit_batch(batch, true))?;
            ops = ops.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        }
        let elapsed = start.elapsed().as_nanos();
        let elapsed_ns = u64::try_from(elapsed).map_err(|_| RunnerError::ElapsedOverflow)?;
        Ok(LoadOutcome { elapsed_ns, ops })
    }

    /// Sequential point-read each key in `keys`. Captures one
    /// histogram sample per op. Mirrors
    /// [`crate::bbolt_runner::BboltOracle::get_seq`].
    pub fn get_seq(&self, keys: &[Vec<u8>]) -> Result<GetOutcome, RunnerError> {
        self.get(keys)
    }

    /// Same shape as [`Self::get_seq`]. `theta` is recorded for
    /// provenance only — the harness has already pre-shaped the key
    /// list per the zipfian distribution.
    pub fn get_zipfian(&self, keys: &[Vec<u8>], _theta: f64) -> Result<GetOutcome, RunnerError> {
        self.get(keys)
    }

    fn get(&self, keys: &[Vec<u8>]) -> Result<GetOutcome, RunnerError> {
        let snap = self.backend.snapshot()?;
        let mut hist = LatencyHistogram::new()?;
        let mut ops: u64 = 0;
        let loop_start = Instant::now();
        for key in keys {
            let op_start = Instant::now();
            let _ = snap.get(self.bucket, key)?;
            let dt = op_start.elapsed().as_nanos();
            // Saturating cast into u64 for the histogram. The
            // pinned ceiling is 60 s; anything past that is already
            // an overflow on the histogram side, so clamping at u64
            // before recording is correct.
            let dt_u64 = u64::try_from(dt).unwrap_or(u64::MAX);
            hist.record(dt_u64);
            ops = ops.saturating_add(1);
        }
        let elapsed = loop_start.elapsed().as_nanos();
        let elapsed_ns = u64::try_from(elapsed).map_err(|_| RunnerError::ElapsedOverflow)?;
        Ok(GetOutcome {
            elapsed_ns,
            ops,
            histogram: hist,
        })
    }

    /// Half-open scan `[start, end)`, capped at `limit` rows
    /// (`0` = no cap). The returned `checksum` xor-folds the first
    /// byte of each row's key+value (`(k[0]) | (v[0] << 8)`) — the
    /// SAME expression the bbolt oracle uses, so the byte-level work
    /// is symmetric. Asserting the checksum is non-zero on non-empty
    /// scans pins the fairness invariant from the harness side.
    pub fn range(
        &self,
        start: &[u8],
        end: &[u8],
        limit: usize,
    ) -> Result<RangeOutcome, RunnerError> {
        let snap = self.backend.snapshot()?;
        let scan_start = Instant::now();
        let mut iter = snap.range(self.bucket, start, end)?;
        let mut rows: u64 = 0;
        let mut checksum: u64 = 0;
        for item in iter.by_ref() {
            let (k, v): (Bytes, Bytes) = item?;
            // Xor-fold the first byte of key and the first byte of
            // value, the same shape as `benchOpRange` in bench.go.
            // `Bytes::first` returns `Option<&u8>`; an empty key or
            // value contributes 0 from that slot — matches the Go
            // side, where `kCopy[0]` would panic on empty (the bench
            // workload is contractually non-empty).
            let kb = u64::from(k.first().copied().unwrap_or(0));
            let vb = u64::from(v.first().copied().unwrap_or(0));
            // `vb << 8` is the same fold the Go side performs; both
            // operands are bounded to u8 so the shift cannot
            // overflow u64.
            #[allow(
                clippy::arithmetic_side_effects,
                reason = "vb is u8 widened to u64; vb << 8 cannot overflow"
            )]
            let fold = kb | (vb << 8);
            checksum ^= fold;
            rows = rows.saturating_add(1);
            if limit != 0 && rows >= u64::try_from(limit).unwrap_or(u64::MAX) {
                break;
            }
        }
        let elapsed = scan_start.elapsed().as_nanos();
        let elapsed_ns = u64::try_from(elapsed).map_err(|_| RunnerError::ElapsedOverflow)?;
        Ok(RangeOutcome {
            elapsed_ns,
            rows,
            checksum,
        })
    }

    /// On-disk size in bytes (advisory). Mirrors
    /// [`crate::bbolt_runner::BboltOracle::size`]; both engines
    /// resolve it from the post-commit file size.
    pub fn size(&self) -> Result<SizeOutcome, RunnerError> {
        let bytes = self.backend.size_on_disk()?;
        Ok(SizeOutcome { bytes })
    }
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

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// Open → close → reopen lifecycle assertion. Mirrors
    /// `bbolt_runner::tests::spawn_open_close_lifecycle`.
    #[test]
    fn open_close_lifecycle() {
        let dir = tmpdir();
        let runner = MangoRunner::open(dir.path().to_path_buf(), None).expect("open");
        runner.close().expect("close");
        // Idempotent close.
        runner.close().expect("close again");
    }

    /// `with_cache_size` plumbs through to the redb backend without
    /// erroring — the value is engine-private; we cannot read it
    /// back from the public surface. This is a smoke test that the
    /// builder chain is wired.
    #[test]
    fn open_accepts_explicit_cache_size() {
        let dir = tmpdir();
        let runner =
            MangoRunner::open(dir.path().to_path_buf(), Some(16 << 20)).expect("open w/ cache");
        runner.close().expect("close");
    }

    /// Load + sequential get. Asserts the histogram records one
    /// sample per get and that `max_ns > 0`. Mirrors
    /// `bbolt_runner::tests::load_then_get_seq_round_trip` so a
    /// reviewer can diff the two test bodies and see they exercise
    /// the same shape on each side.
    #[test]
    fn load_then_get_seq_round_trip() {
        let dir = tmpdir();
        let runner = MangoRunner::open(dir.path().to_path_buf(), None).expect("open");
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..50_u8)
            .map(|i| (vec![i, i.wrapping_add(1)], vec![b'v', i]))
            .collect();
        let load = runner.load(&pairs, 16).expect("load");
        assert_eq!(load.ops, 50, "load should report 50 ops");

        let keys: Vec<Vec<u8>> = pairs.iter().map(|(k, _)| k.clone()).collect();
        let got = runner.get_seq(&keys).expect("get_seq");
        assert_eq!(got.ops, 50);
        assert_eq!(got.histogram.count(), 50, "histogram carries 50 samples");
        assert!(got.histogram.max_ns() > 0);
        runner.close().expect("close");
    }

    /// Range checksum over a non-empty scan must be non-zero. Pins
    /// the fairness invariant (S3 N8) on the mango side: the
    /// `Bytes::copy_from_slice` results must actually be read into
    /// the fold or the comparison against bbolt is meaningless.
    #[test]
    fn range_checksum_is_non_zero_on_non_empty_scan() {
        let dir = tmpdir();
        let runner = MangoRunner::open(dir.path().to_path_buf(), None).expect("open");
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = (1..=20_u8)
            .map(|i| (vec![i], vec![0xff_u8.wrapping_sub(i), b'v']))
            .collect();
        runner.load(&pairs, 8).expect("load");

        let scan = runner.range(&[1_u8], &[30_u8], 0).expect("range");
        assert_eq!(scan.rows, 20, "range should visit all 20 rows");
        assert_ne!(
            scan.checksum, 0,
            "non-empty range checksum was zero — fairness invariant broken \
             (per-row read elided?)"
        );
        runner.close().expect("close");
    }

    /// Zero-checksum-on-zero-input contract: an empty range MUST
    /// return checksum=0 and rows=0 (fold over the empty set is the
    /// identity). Distinct from the non-empty test so a regression
    /// where the fold accumulator is incorrectly seeded is caught.
    #[test]
    fn range_on_empty_bucket_returns_zero_checksum_zero_rows() {
        let dir = tmpdir();
        let runner = MangoRunner::open(dir.path().to_path_buf(), None).expect("open");
        let scan = runner.range(&[0_u8], &[255_u8], 0).expect("range");
        assert_eq!(scan.rows, 0);
        assert_eq!(scan.checksum, 0);
        runner.close().expect("close");
    }

    /// `range` honors the `limit` cap: scanning N rows with limit=5
    /// stops after 5 rows.
    #[test]
    fn range_respects_limit_cap() {
        let dir = tmpdir();
        let runner = MangoRunner::open(dir.path().to_path_buf(), None).expect("open");
        let pairs: Vec<(Vec<u8>, Vec<u8>)> =
            (1..=20_u8).map(|i| (vec![i], vec![i, b'v'])).collect();
        runner.load(&pairs, 8).expect("load");
        let scan = runner.range(&[1_u8], &[30_u8], 5).expect("range");
        assert_eq!(scan.rows, 5);
    }

    /// Size after a load is positive. Sanity check on the
    /// `size_on_disk` plumb-through.
    #[test]
    fn size_reports_positive_after_load() {
        let dir = tmpdir();
        let runner = MangoRunner::open(dir.path().to_path_buf(), None).expect("open");
        runner
            .load(&[(b"k".to_vec(), vec![b'v'; 1024])], 1)
            .expect("load");
        let sz = runner.size().expect("size");
        assert!(sz.bytes > 0, "size after load was zero");
        runner.close().expect("close");
    }

    /// `batch_size = 0` is rejected up-front rather than panicking
    /// inside `slice::chunks(0)`.
    #[test]
    fn load_rejects_zero_batch_size() {
        let dir = tmpdir();
        let runner = MangoRunner::open(dir.path().to_path_buf(), None).expect("open");
        let pairs = vec![(b"k".to_vec(), b"v".to_vec())];
        let err = runner.load(&pairs, 0).expect_err("zero batch size");
        assert!(
            matches!(err, RunnerError::InvalidArg(_)),
            "got {err:?}, expected InvalidArg"
        );
    }

    /// `get_zipfian` shares the implementation with `get_seq`; this
    /// test pins that the public method is wired and the histogram
    /// records samples — the harness will call this method with
    /// shaped keys, but on the runner side the surface is the same.
    #[test]
    fn get_zipfian_records_per_op_samples() {
        let dir = tmpdir();
        let runner = MangoRunner::open(dir.path().to_path_buf(), None).expect("open");
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..10_u8).map(|i| (vec![i], vec![b'v', i])).collect();
        runner.load(&pairs, 4).expect("load");

        // theta value is provenance-only — we just assert the call
        // works and the histogram records ops_count samples.
        let keys: Vec<Vec<u8>> = pairs.iter().map(|(k, _)| k.clone()).collect();
        let got = runner.get_zipfian(&keys, 0.99).expect("get_zipfian");
        assert_eq!(got.ops, 10);
        assert_eq!(got.histogram.count(), 10);
    }
}
