//! Latency capture (`HdrHistogram` V2-deflate base64) and the
//! result-file JSON shape consumed by the gate (S1, B1).
//!
//! The pinned histogram parameters and the on-wire encoding are
//! frozen as part of the bench protocol — see
//! `.planning/parity-bench-harness.plan.md` §"Histogram parameters
//! — pinned (N2)" and §"On-wire histogram". Both the Rust harness
//! (this module) and the Go bbolt oracle (`benches/oracles/bbolt/
//! bench.go`) MUST agree on these constants byte-for-byte; a future
//! change to any of them is a wire-format break and requires a
//! `format_version` bump.
//!
//! ## Histogram constants
//!
//! - `LOWEST_DISCERNIBLE_NS = 1_000` (1 µs floor — sub-µs precision
//!   is illusory for storage reads above the syscall RTT).
//! - `HIGHEST_TRACKABLE_NS = 60_000_000_000` (60 s — anything beyond
//!   is a stall and lands in the overflow bucket via
//!   `saturating_record`).
//! - `SIGNIFICANT_FIGURES = 3` (≈ 0.1 % bucket precision).
//!
//! ## JSON shape (`format_version = 1`)
//!
//! Mirrors the plan's frozen fields. The gate refuses any result
//! file whose `format_version != 1`.

use std::io::Cursor;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use hdrhistogram::serialization::{Deserializer, V2DeflateSerializer};
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};

use crate::stats::Verdict;

/// Floor of the histogram's value range. 1 µs.
pub const LOWEST_DISCERNIBLE_NS: u64 = 1_000;

/// Ceiling of the histogram's value range. 60 s.
pub const HIGHEST_TRACKABLE_NS: u64 = 60_000_000_000;

/// Significant figures (mantissa precision) — 3 → ~0.1 % buckets.
pub const SIGNIFICANT_FIGURES: u8 = 3;

/// Errors raised by the latency-histogram capture layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HistogramError {
    /// The histogram could not be constructed with the pinned
    /// parameters (logically unreachable; the constants are
    /// validated via `const_assert`-style checks at construction).
    #[error("histogram creation failed: {0}")]
    Creation(#[from] hdrhistogram::errors::CreationError),

    /// The V2-deflate writer failed.
    #[error("histogram serialization failed: {0}")]
    Serialize(#[from] hdrhistogram::serialization::V2DeflateSerializeError),

    /// The deserialized payload was not a valid V2 / V2-compressed
    /// histogram, or the inner parameters disagreed with the pin.
    #[error("histogram deserialization failed: {0}")]
    Deserialize(#[from] hdrhistogram::serialization::DeserializeError),

    /// The base64 channel returned non-ASCII or invalid base64.
    #[error("base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),

    /// Adding two histograms with mismatched bounds.
    #[error("histogram add failed: {0:?}")]
    Add(hdrhistogram::errors::AdditionError),
}

/// A latency histogram pinned to the bench protocol parameters.
///
/// Wraps `hdrhistogram::Histogram<u64>` with a constructor that
/// hard-codes the pinned bounds and a record path that
/// saturating-clamps overflow rather than panicking.
#[derive(Debug, Clone)]
pub struct LatencyHistogram {
    inner: Histogram<u64>,
}

impl LatencyHistogram {
    /// Construct an empty histogram with the pinned parameters.
    pub fn new() -> Result<Self, HistogramError> {
        let inner = Histogram::<u64>::new_with_bounds(
            LOWEST_DISCERNIBLE_NS,
            HIGHEST_TRACKABLE_NS,
            SIGNIFICANT_FIGURES,
        )?;
        Ok(Self { inner })
    }

    /// Record one latency sample (nanoseconds). Values above
    /// [`HIGHEST_TRACKABLE_NS`] are clamped into the top bucket
    /// (saturating) — `record` (which would error on overflow) is
    /// not used so a single >60 s outlier cannot abort the run.
    pub fn record(&mut self, value_ns: u64) {
        self.inner.saturating_record(value_ns);
    }

    /// Number of recorded samples.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.inner.len()
    }

    /// Value at the given quantile (e.g. `0.99` for p99).
    #[must_use]
    pub fn value_at_quantile(&self, q: f64) -> u64 {
        self.inner.value_at_quantile(q)
    }

    /// Minimum recorded value, or 0 if empty.
    #[must_use]
    pub fn min_ns(&self) -> u64 {
        self.inner.min()
    }

    /// Maximum recorded value, or 0 if empty.
    #[must_use]
    pub fn max_ns(&self) -> u64 {
        self.inner.max()
    }

    /// Arithmetic mean of recorded values, or 0.0 if empty.
    #[must_use]
    pub fn mean_ns(&self) -> f64 {
        self.inner.mean()
    }

    /// Add another histogram into this one. Used to merge per-run
    /// histograms before computing aggregate percentiles.
    pub fn add(&mut self, other: &Self) -> Result<(), HistogramError> {
        self.inner.add(&other.inner).map_err(HistogramError::Add)?;
        Ok(())
    }

    /// Encode as base64-of-V2-deflate. This is the on-wire format
    /// shared with the Go oracle.
    pub fn to_base64_v2_deflate(&self) -> Result<String, HistogramError> {
        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        let mut serializer = V2DeflateSerializer::new();
        hdrhistogram::serialization::Serializer::serialize(&mut serializer, &self.inner, &mut buf)?;
        Ok(BASE64_STANDARD.encode(buf))
    }

    /// Decode from the same wire format. The deserializer accepts
    /// either V2 or V2-compressed (cookie-driven), so a future
    /// migration to uncompressed-V2 is non-breaking on the read
    /// path.
    pub fn from_base64_v2_deflate(s: &str) -> Result<Self, HistogramError> {
        let raw = BASE64_STANDARD.decode(s.as_bytes())?;
        let mut deserializer = Deserializer::new();
        let mut cursor = Cursor::new(raw);
        let inner: Histogram<u64> = deserializer.deserialize(&mut cursor)?;
        Ok(Self { inner })
    }
}

/// Compute throughput in operations per second from a (count,
/// elapsed-ns) pair.
///
/// Edge cases:
/// - `elapsed_ns == 0` → 0.0 (caller treats as "no measurement"; a
///   structurally impossible run should be filtered upstream).
/// - `ops == 0` → 0.0.
/// - Otherwise: `ops * 1e9 / elapsed_ns` in f64.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "ops and elapsed_ns are bounded well below 2^53 in any realistic bench run"
)]
pub fn throughput_ops_per_sec(ops: u64, elapsed_ns: u64) -> f64 {
    if ops == 0 || elapsed_ns == 0 {
        return 0.0;
    }
    let ops_f = ops as f64;
    let ns_f = elapsed_ns as f64;
    ops_f * 1e9 / ns_f
}

/// JSON: per-metric record. See plan §"Per-metric verdicts in the
/// JSON".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub struct MetricRecord {
    /// Stable metric name, e.g. `"write_throughput_unbatched"`,
    /// `"read_latency_p99_hot"`. The gate matches by this string.
    pub metric: String,

    /// Engine producing this record: `"mango"` or `"bbolt"`.
    pub engine: String,

    /// Per-run raw values (length = number of runs, typically 20).
    /// For throughput metrics: ops/s. For latency metrics: ns at
    /// the chosen percentile.
    pub engine_runs: Vec<f64>,

    /// Mean across `engine_runs`.
    pub mean: f64,

    /// Median across `engine_runs`.
    pub median: f64,

    /// Sample standard deviation (n − 1 denominator) across
    /// `engine_runs`. 0.0 if `engine_runs.len() < 2`.
    pub stddev: f64,

    /// Bootstrap 95 % CI lower bound on the mango/bbolt ratio.
    /// `None` until the gate computes it (a single-engine record
    /// has no ratio).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratio_lower_95: Option<f64>,

    /// Bootstrap 95 % CI upper bound on the mango/bbolt ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratio_upper_95: Option<f64>,

    /// Verdict (`"win"` / `"loss"` / `"tie"` / `"incomplete"`).
    /// `None` on a single-engine record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,

    /// Per-metric fairness flag (S3): `Some("asymmetric")` excludes
    /// the metric from the gate's win/loss aggregation per plan §B4
    /// "Mango loses on a metric that S3 marked `asymmetric`."
    /// `Some("symmetric_copy")` is the affirmative version of the
    /// flag; either is accepted, the gate's only behaviour is to
    /// **skip** when the value is exactly `"asymmetric"`. `None` on
    /// metrics without a fairness annotation (everything except
    /// `range_throughput` today).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fairness: Option<String>,
}

/// JSON: per-run record (one per run index). Carries the
/// interleaving order (N4) and the cold-cache verdict (S2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub struct RunRecord {
    /// Run index `[0, runs)`.
    pub run_index: u32,

    /// `"mango_first"` or `"bbolt_first"` — see plan §"Run
    /// interleaving (N4)".
    pub run_order: String,

    /// Cold-cache verdict for this run on this engine: `"pass"`,
    /// `"fail"`, `"incomplete"`. Encoded as a string for forward-
    /// compatibility with the gate.
    pub cold_cache_verdict: String,

    /// Base64 V2-deflate encoded latency histogram for the
    /// hot-cache read loop on this run, or `None` if this run did
    /// not include a hot-cache read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hot_hist_b64: Option<String>,

    /// Base64 V2-deflate encoded latency histogram for the
    /// cold-cache read loop on this run, or `None` if cold-cache
    /// was skipped (incomplete platform).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cold_hist_b64: Option<String>,

    /// Base64 V2-deflate encoded latency histogram for the
    /// zipfian read loop on this run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zipfian_hist_b64: Option<String>,
}

/// JSON: top-level result file. `format_version == 1` is enforced
/// by the gate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub struct ResultFile {
    /// Schema version. Must be 1 — older schemas cannot be silently
    /// re-graded under new gate logic.
    pub format_version: u32,

    /// Engine identifier: `"mango"` or `"bbolt"`.
    pub engine: String,

    /// SHA-256 (lowercase hex) of the workload toml bytes
    /// verbatim. Two runs with different hashes cannot be gated
    /// against each other.
    pub workload_sha256: String,

    /// Workload schema version (the toml's `version` field).
    pub workload_version: u32,

    /// Path (relative to the result JSON's directory) of the
    /// `signature.txt` file emitted by `benches/runner/run.sh`.
    pub signature_path: String,

    /// UTC start time, ISO 8601 (e.g. `"2026-05-03T18:30:11Z"`).
    pub started_at: String,

    /// Per-run records. Length = `metrics[*].engine_runs.len()`.
    pub runs: Vec<RunRecord>,

    /// Per-metric aggregates. Single-engine result files carry no
    /// ratio / verdict; the gate fills those in when comparing two
    /// files.
    pub metrics: Vec<MetricRecord>,
}

impl ResultFile {
    /// `format_version` constant exposed for callers that want to
    /// stamp it without hard-coding the literal.
    pub const FORMAT_VERSION: u32 = 1;
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::panic,
        clippy::float_cmp
    )]

    use super::*;

    #[test]
    fn empty_histogram_has_zero_count() {
        let h = LatencyHistogram::new().unwrap();
        assert_eq!(h.count(), 0);
    }

    #[test]
    fn records_land_in_expected_percentile_buckets() {
        let mut h = LatencyHistogram::new().unwrap();
        // 1000 samples uniformly across 1ms..101ms (1000 ns increments).
        for i in 0..1000_u64 {
            let v = 1_000_000 + (i * 100_000); // 1ms .. 100.9ms
            h.record(v);
        }
        assert_eq!(h.count(), 1000);
        let p50 = h.value_at_quantile(0.50);
        let p99 = h.value_at_quantile(0.99);
        assert!(
            (49_000_000..=52_000_000).contains(&p50),
            "p50 {p50} ns out of expected ~50ms band"
        );
        assert!(
            (98_000_000..=101_000_000).contains(&p99),
            "p99 {p99} ns out of expected ~100ms band"
        );
    }

    #[test]
    fn record_above_ceiling_saturates_without_panic() {
        let mut h = LatencyHistogram::new().unwrap();
        // 1 hour — well above the 60 s ceiling.
        h.record(3_600_000_000_000);
        assert_eq!(h.count(), 1);
        // `saturating_record` clamps to the ceiling's containing
        // bucket. With 3 sigfigs at the 60 s ceiling, bucket
        // resolution is ~ 0.1 % × 60 s ≈ 60 ms; the equivalent
        // value can therefore land slightly above
        // `HIGHEST_TRACKABLE_NS`. We bound the slop at the
        // histogram's own quoted precision (1 % of the ceiling)
        // which is the tightest defensible check; the contract
        // here is "no panic, no 1-hour outlier in the top bucket
        // — just clamped to ~ ceiling".
        let p100 = h.value_at_quantile(1.0);
        let max_allowed = HIGHEST_TRACKABLE_NS.saturating_add(HIGHEST_TRACKABLE_NS / 100);
        assert!(
            p100 <= max_allowed,
            "p100 {p100} exceeded ceiling+1% tolerance ({max_allowed})"
        );
    }

    #[test]
    fn base64_v2_deflate_roundtrips() {
        let mut h = LatencyHistogram::new().unwrap();
        // Bimodal: most fast, a long tail.
        for _ in 0..1000 {
            h.record(5_000); // 5µs
        }
        for _ in 0..10 {
            h.record(50_000_000); // 50ms tail
        }
        let p50 = h.value_at_quantile(0.50);
        let p99 = h.value_at_quantile(0.99);
        let p999 = h.value_at_quantile(0.999);

        let b64 = h.to_base64_v2_deflate().unwrap();
        let recovered = LatencyHistogram::from_base64_v2_deflate(&b64).unwrap();

        assert_eq!(recovered.count(), h.count());
        assert_eq!(recovered.value_at_quantile(0.50), p50);
        assert_eq!(recovered.value_at_quantile(0.99), p99);
        assert_eq!(recovered.value_at_quantile(0.999), p999);
    }

    #[test]
    fn base64_decode_rejects_garbage() {
        let err = LatencyHistogram::from_base64_v2_deflate("not base64!@#$").unwrap_err();
        assert!(matches!(err, HistogramError::Base64(_)), "got {err:?}");
    }

    #[test]
    fn base64_decode_rejects_truncated_payload() {
        // Valid base64 but not a histogram.
        let err = LatencyHistogram::from_base64_v2_deflate("AAAA").unwrap_err();
        assert!(matches!(err, HistogramError::Deserialize(_)), "got {err:?}");
    }

    #[test]
    fn add_merges_two_histograms() {
        let mut a = LatencyHistogram::new().unwrap();
        let mut b = LatencyHistogram::new().unwrap();
        for _ in 0..100 {
            a.record(10_000);
        }
        for _ in 0..100 {
            b.record(20_000);
        }
        a.add(&b).unwrap();
        assert_eq!(a.count(), 200);
        // p50 of merged distribution is the lower mode (50 % of
        // mass at 10 µs).
        let p50 = a.value_at_quantile(0.50);
        assert!((9_000..=11_000).contains(&p50), "merged p50 = {p50}");
    }

    #[test]
    fn throughput_handles_normal_case() {
        // 1 000 ops in 1 ms → 1 000 000 ops/s.
        let v = throughput_ops_per_sec(1_000, 1_000_000);
        assert!((v - 1_000_000.0).abs() < 1e-3);
    }

    #[test]
    fn throughput_handles_one_op_in_one_ns() {
        // The "edge value" the plan calls out (commit 5 test note).
        let v = throughput_ops_per_sec(1, 1);
        assert!((v - 1e9).abs() < 1.0);
    }

    #[test]
    fn throughput_zero_ops_returns_zero_not_nan() {
        assert_eq!(throughput_ops_per_sec(0, 1_000_000), 0.0);
    }

    #[test]
    fn throughput_zero_elapsed_returns_zero_not_inf() {
        let v = throughput_ops_per_sec(1_000, 0);
        assert_eq!(v, 0.0);
        assert!(v.is_finite());
    }

    #[test]
    fn result_file_format_version_constant_is_one() {
        assert_eq!(ResultFile::FORMAT_VERSION, 1);
    }

    #[test]
    fn result_file_json_shape_is_stable() {
        let result = ResultFile {
            format_version: ResultFile::FORMAT_VERSION,
            engine: "mango".to_owned(),
            workload_sha256: "abc123".to_owned(),
            workload_version: 1,
            signature_path: "signature.txt".to_owned(),
            started_at: "2026-05-03T18:30:11Z".to_owned(),
            runs: Vec::new(),
            metrics: vec![MetricRecord {
                metric: "write_throughput_unbatched".to_owned(),
                engine: "mango".to_owned(),
                engine_runs: vec![12345.6, 12300.4],
                mean: 12323.0,
                median: 12323.0,
                stddev: 22.1,
                ratio_lower_95: None,
                ratio_upper_95: None,
                verdict: None,
                fairness: None,
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        // Required top-level keys.
        assert!(json.contains("\"format_version\":1"), "{json}");
        assert!(json.contains("\"engine\":\"mango\""), "{json}");
        assert!(json.contains("\"workload_sha256\":\"abc123\""), "{json}");
        assert!(
            json.contains("\"workload_version\":1"),
            "missing workload_version: {json}"
        );
        assert!(
            json.contains("\"metric\":\"write_throughput_unbatched\""),
            "{json}"
        );
        // Optional fields elided when None — keeps single-engine
        // files free of placeholder verdicts.
        assert!(!json.contains("\"verdict\""), "verdict leaked: {json}");
        assert!(!json.contains("\"ratio_lower_95\""), "ratio leaked: {json}");
    }

    #[test]
    fn result_file_json_round_trips() {
        let original = ResultFile {
            format_version: 1,
            engine: "bbolt".to_owned(),
            workload_sha256: "deadbeef".to_owned(),
            workload_version: 1,
            signature_path: "signature.txt".to_owned(),
            started_at: "2026-05-03T18:30:11Z".to_owned(),
            runs: vec![RunRecord {
                run_index: 0,
                run_order: "mango_first".to_owned(),
                cold_cache_verdict: "incomplete".to_owned(),
                hot_hist_b64: Some("ZmFrZQ==".to_owned()),
                cold_hist_b64: None,
                zipfian_hist_b64: None,
            }],
            metrics: Vec::new(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let recovered: ResultFile = serde_json::from_str(&json).unwrap();
        assert_eq!(recovered, original);
    }
}
