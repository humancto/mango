//! Verdict gate: turns two single-engine result JSONs (one mango,
//! one bbolt) into a `pass` / `fail` for ROADMAP:829 (L829).
//!
//! Phase 1 parity bench harness — see
//! `.planning/parity-bench-harness.plan.md`. The contract is:
//!
//! 1. **Co-resident signature.** Each result JSON references a
//!    `signature_path`; the gate reads that file and rejects on any
//!    I/O error or parse failure (N9 §1).
//! 2. **Linux Tier-1.** The signature must parse as `os == "linux"`
//!    AND `tier == 1` (N9 §2). macOS or non-Tier-1 runners cannot
//!    satisfy L829 — even if the JSON looks otherwise clean.
//! 3. **Co-resident with the JSON.** `signature.txt` must live in
//!    the same directory as its result JSON (N9 §3); a Linux
//!    signature copied next to a macOS JSON is rejected.
//! 4. **`format_version == 1`.** Per N10. A future schema bump that
//!    forgets to teach the gate how to read it MUST fail loudly,
//!    not silently re-grade old data.
//! 5. **`workload_sha256` matches across the two files.** Two runs
//!    on different workload tomls cannot be gated against each
//!    other — that is comparing apples to oranges.
//! 6. **Engine identifiers.** Exactly one file per engine, the
//!    other being the corresponding bbolt or mango oracle. Two
//!    `mango.json` files or two `bbolt.json` files is a structural
//!    error, not a tie.
//! 7. **Bootstrap CI win/loss aggregation.** For each paired
//!    metric, `stats::bootstrap_ci` computes the 95 % CI of the
//!    "higher means mango wins" ratio. The metric is `Win` /
//!    `Loss` / `Tie` per the floors in `stats`. Aggregate across
//!    all *non-skipped* metrics:
//!    - **Pass**: ≥ 1 `Win`, 0 `Loss`. (`Tie` is OK.)
//!    - **Fail**: any `Loss`, OR no `Win` (all-Tie still fails per
//!      §B4 "Mango ties on every metric. … gate requires ≥ 1 win").
//!    - **Incomplete metric anywhere → Fail.** (S2 §"Verdict
//!      states": a result with any `incomplete` metric *cannot
//!      satisfy L829*.)
//! 8. **Fairness (S3 §B4).** A metric whose `fairness == "asymmetric"`
//!    in *either* file is excluded from the aggregate — neither its
//!    win nor its loss counts. The verdict still appears in the
//!    report under `skipped` so the operator can see what was
//!    excluded and why. `"symmetric_copy"` is the affirmative
//!    annotation; only the literal string `"asymmetric"` triggers
//!    the skip.
//!
//! The gate does NOT mutate the input files. It returns a
//! [`GateReport`] which the binary at `bin/gate.rs` renders to
//! stdout and exits 0/1/2 from.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::measure::{MetricRecord, ResultFile};
use crate::stats::{self, BootstrapCi, HigherIs, StatsError, Verdict};

/// Default RNG seed for the bootstrap CI when the binary does not
/// override it. The plan §"Determinism" specifies
/// `H(workload.seed || metric_name)`; in absence of a seed override
/// the gate falls back to this constant so its output is still
/// reproducible. The binary surfaces a `--rng-seed` flag for
/// callers that want a workload-derived seed.
pub const DEFAULT_GATE_RNG_SEED: u64 = 0xb007_8298_29b0_07b0_u64;

/// Acceptable engine names.
pub const ENGINE_MANGO: &str = "mango";
pub const ENGINE_BBOLT: &str = "bbolt";

/// Sentinel value for the `fairness` field that triggers a metric
/// skip in the aggregate.
pub const FAIRNESS_ASYMMETRIC: &str = "asymmetric";
/// Affirmative fairness annotation: the metric's per-row work is
/// symmetric across engines (e.g. `range_throughput` after the S3
/// force-copy). Recorded for operator visibility; gate does not
/// branch on it.
pub const FAIRNESS_SYMMETRIC_COPY: &str = "symmetric_copy";

/// What kind of metric this is — drives the bootstrap's
/// "higher-is-better" direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetricKind {
    /// Operations per second. Higher is better for mango.
    Throughput,
    /// Latency at some percentile (ns). Lower is better for mango.
    Latency,
    /// On-disk size (bytes). Lower is better for mango.
    Size,
}

impl MetricKind {
    /// Map to the bootstrap's direction parameter.
    #[must_use]
    pub fn higher_is(self) -> HigherIs {
        match self {
            Self::Throughput => HigherIs::Better,
            // Latency / size are "lower is better"; the bootstrap
            // inverts internally so the returned ratio still reads
            // "higher means mango wins".
            Self::Latency | Self::Size => HigherIs::Worse,
        }
    }
}

/// Lookup the [`MetricKind`] from a metric's stable name.
///
/// Phase 1 metric set is closed: the gate refuses any name not in
/// this table (returned as [`GateError::UnknownMetric`]). A new
/// metric requires both a `format_version` bump and a code change
/// here — silently inferring kind from a substring like `"latency"`
/// is precisely the kind of footgun N10 (`format_version` policy)
/// is meant to prevent.
#[must_use]
pub fn infer_kind(metric_name: &str) -> Option<MetricKind> {
    match metric_name {
        "write_throughput_unbatched" | "write_throughput_batched" | "range_throughput" => {
            Some(MetricKind::Throughput)
        }
        "read_latency_p99_hot" | "read_latency_p99_cold" | "read_latency_p99_zipfian" => {
            Some(MetricKind::Latency)
        }
        "on_disk_size" => Some(MetricKind::Size),
        _ => None,
    }
}

/// Parsed `BENCH_HW v1: …` signature line. The gate only ever
/// reads `os` and `tier`; the other fields are kept for the
/// human-readable report.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SignatureInfo {
    /// `linux` (Tier-1 only) | `darwin` | `…`. Lowercase.
    pub os: String,
    /// Tier number (1, 2, 3). Tier 1 is the only acceptance class
    /// per the plan §"Phase 1 acceptance — Linux only".
    pub tier: u32,
    /// All `key=value` pairs verbatim, in declaration order. The
    /// report includes this for the operator; the gate itself
    /// only reads `os` and `tier`.
    pub raw: Vec<(String, String)>,
}

/// Errors returned by [`gate`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GateError {
    /// The result file's `format_version` was not `1`. Refuses
    /// silent re-grading per N10.
    #[error("{which}: unsupported format_version {actual}, gate accepts only format_version == 1")]
    FormatVersion { which: &'static str, actual: u32 },

    /// The two files reference different workloads. Their
    /// `workload_sha256` fields disagreed.
    #[error(
        "workload_sha256 mismatch: mango={mango_sha} bbolt={bbolt_sha} — \
         the two runs are not comparable"
    )]
    WorkloadHashMismatch {
        mango_sha: String,
        bbolt_sha: String,
    },

    /// The two files came from the wrong engine pair (e.g., two
    /// `mango.json` or a `mango.json` mislabelled as `bbolt`).
    #[error(
        "engine mismatch: expected one mango and one bbolt result, \
         got mango.engine={mango_engine}, bbolt.engine={bbolt_engine}"
    )]
    EngineMismatch {
        mango_engine: String,
        bbolt_engine: String,
    },

    /// Workload schema-version drift inside `format_version=1` —
    /// the major schema is fixed at 1 but the workload itself can
    /// version-bump (different keys etc.); compared files must
    /// share the same `workload_version` integer.
    #[error(
        "workload_version mismatch: mango={mango} bbolt={bbolt} — \
         the two runs are not comparable"
    )]
    WorkloadVersionMismatch { mango: u32, bbolt: u32 },

    /// A metric appeared in only one of the two files. The harness
    /// is expected to emit the same metric set on both engines.
    #[error("{which}.json carries metric {metric:?} that is missing from the other engine")]
    MissingMetricPair { which: &'static str, metric: String },

    /// A metric name the gate does not know. Forces explicit
    /// `MetricKind` registration when adding new metrics.
    #[error(
        "unknown metric {name:?} — add it to gate::infer_kind and \
         bump format_version if the schema changed"
    )]
    UnknownMetric { name: String },

    /// Bootstrap CI computation rejected the inputs (length
    /// mismatch, non-positive values, NaN, …).
    #[error("metric {metric:?} bootstrap failed: {source}")]
    Stats { metric: String, source: StatsError },

    /// I/O error reading the signature file (missing, permission
    /// denied, …).
    #[error("{which}: cannot read signature {path}: {source}")]
    SignatureIo {
        which: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Signature file did not contain the expected `BENCH_HW v1:`
    /// line, or was malformed.
    #[error("{which}: signature parse failed for {path}: {reason}")]
    SignatureParse {
        which: &'static str,
        path: PathBuf,
        reason: &'static str,
    },

    /// Signature parsed, but `os != "linux"` or `tier != 1`. L829
    /// only accepts Tier-1 Linux runs.
    #[error("{which}: signature is os={os}, tier={tier}; L829 requires os=linux and tier=1")]
    SignatureNotTier1 {
        which: &'static str,
        os: String,
        tier: u32,
    },

    /// `signature_path` resolves outside the result-JSON's directory.
    /// Prevents copying a Linux signature into a macOS JSON's dir
    /// and re-using it.
    #[error("{which}: signature {sig_path} is not co-resident with result JSON dir {json_dir}")]
    SignatureNotCoResident {
        which: &'static str,
        sig_path: PathBuf,
        json_dir: PathBuf,
    },
}

/// One metric's contribution to the gate aggregate.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct MergedMetric {
    pub metric: String,
    pub kind: MetricKind,
    pub mango_runs: Vec<f64>,
    pub bbolt_runs: Vec<f64>,
    pub ratio_mean: f64,
    pub ratio_lower_95: f64,
    pub ratio_upper_95: f64,
    pub verdict: Verdict,
    /// Either file's `fairness` flag. If either was
    /// `"asymmetric"`, this metric appears in `GateReport.skipped`
    /// instead of `GateReport.merged`.
    pub fairness: Option<String>,
}

/// Top-level gate verdict — the only thing the binary's exit code
/// branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GateVerdict {
    /// ≥ 1 win and 0 losses on the non-skipped metrics. L829 is
    /// satisfied.
    Pass,
    /// Any loss, or no win at all (incl. all-tie), or any
    /// `incomplete`.
    Fail,
}

/// Full gate report — both the verdict and the per-metric trace,
/// for human consumption.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GateReport {
    pub verdict: GateVerdict,
    /// Reason for `Fail` (empty on `Pass`). Lists e.g.
    /// `"loss on read_latency_p99_hot"`,
    /// `"incomplete on read_latency_p99_cold"`,
    /// `"no metric reached Win"`.
    pub fail_reasons: Vec<String>,
    /// Per-metric merged records (the ones that went into the
    /// aggregate). Sorted by metric name for deterministic output.
    pub merged: Vec<MergedMetric>,
    /// Metrics excluded from the aggregate, with the human-readable
    /// reason. Sorted by metric name.
    pub skipped: Vec<(String, &'static str)>,
    /// Parsed signatures from both files, for the report header.
    pub mango_signature: SignatureInfo,
    pub bbolt_signature: SignatureInfo,
    /// Echo of the workload sha256 + workload version that both
    /// files agreed on.
    pub workload_sha256: String,
    pub workload_version: u32,
}

/// Read `signature.txt` from `dir/rel`, parse the
/// `BENCH_HW v1: key=value …` line.
///
/// `dir` is the directory containing the result JSON; `rel` is the
/// `signature_path` field from the JSON, which the plan specifies
/// is **relative** to that directory. The gate enforces relativity
/// by rejecting any `rel` that resolves outside `dir` (N9 §3).
pub fn read_signature(
    dir: &Path,
    rel: &str,
    which: &'static str,
) -> Result<(PathBuf, SignatureInfo), GateError> {
    let sig_path = dir.join(rel);

    // N9 §3 — co-residency. We canonicalize both paths, then
    // require the canonical signature path's parent equals the
    // canonical JSON dir. `canonicalize` resolves `..`, symlinks,
    // and relative bits identically on both sides, so a
    // `signature_path = "../somewhere-else/signature.txt"` cannot
    // smuggle in a Linux signature next to a macOS JSON.
    let canon_dir = match dir.canonicalize() {
        Ok(p) => p,
        Err(source) => {
            return Err(GateError::SignatureIo {
                which,
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    let canon_sig = match sig_path.canonicalize() {
        Ok(p) => p,
        Err(source) => {
            return Err(GateError::SignatureIo {
                which,
                path: sig_path.clone(),
                source,
            });
        }
    };
    let sig_parent = canon_sig.parent().unwrap_or_else(|| Path::new(""));
    if sig_parent != canon_dir {
        return Err(GateError::SignatureNotCoResident {
            which,
            sig_path: canon_sig,
            json_dir: canon_dir,
        });
    }

    let raw = fs::read_to_string(&canon_sig).map_err(|source| GateError::SignatureIo {
        which,
        path: canon_sig.clone(),
        source,
    })?;
    let info = parse_signature_text(&raw, which, &canon_sig)?;
    Ok((canon_sig, info))
}

/// Parse the `BENCH_HW v1: os=linux tier=1 …` line from a
/// `signature.txt`. Lines without that prefix are ignored; if no
/// such line appears, returns `SignatureParse`.
fn parse_signature_text(
    text: &str,
    which: &'static str,
    path: &Path,
) -> Result<SignatureInfo, GateError> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("BENCH_HW v1:"))
        .ok_or_else(|| GateError::SignatureParse {
            which,
            path: path.to_path_buf(),
            reason: "no `BENCH_HW v1:` line found",
        })?;

    let body = line.trim_start_matches("BENCH_HW v1:").trim();
    if body.is_empty() {
        return Err(GateError::SignatureParse {
            which,
            path: path.to_path_buf(),
            reason: "`BENCH_HW v1:` line has no body",
        });
    }

    let mut raw: Vec<(String, String)> = Vec::new();
    for tok in body.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            raw.push((k.to_owned(), v.to_owned()));
        } else {
            return Err(GateError::SignatureParse {
                which,
                path: path.to_path_buf(),
                reason: "malformed key=value token",
            });
        }
    }

    let os = raw
        .iter()
        .find(|(k, _)| k == "os")
        .map(|(_, v)| v.clone())
        .ok_or(GateError::SignatureParse {
            which,
            path: path.to_path_buf(),
            reason: "missing `os=` field",
        })?;
    let tier_str = raw
        .iter()
        .find(|(k, _)| k == "tier")
        .map(|(_, v)| v.clone())
        .ok_or(GateError::SignatureParse {
            which,
            path: path.to_path_buf(),
            reason: "missing `tier=` field",
        })?;
    let tier: u32 = tier_str.parse().map_err(|_| GateError::SignatureParse {
        which,
        path: path.to_path_buf(),
        reason: "`tier=` value is not a u32",
    })?;

    Ok(SignatureInfo { os, tier, raw })
}

/// Validate the schema-level invariants common to both files
/// (`format_version`, engine identifiers, workload hash + version).
/// Returns on first violation. Pure — does not touch the
/// filesystem.
fn validate_headers(mango: &ResultFile, bbolt: &ResultFile) -> Result<(), GateError> {
    if mango.format_version != ResultFile::FORMAT_VERSION {
        return Err(GateError::FormatVersion {
            which: "mango",
            actual: mango.format_version,
        });
    }
    if bbolt.format_version != ResultFile::FORMAT_VERSION {
        return Err(GateError::FormatVersion {
            which: "bbolt",
            actual: bbolt.format_version,
        });
    }
    if mango.engine != ENGINE_MANGO || bbolt.engine != ENGINE_BBOLT {
        return Err(GateError::EngineMismatch {
            mango_engine: mango.engine.clone(),
            bbolt_engine: bbolt.engine.clone(),
        });
    }
    if mango.workload_sha256 != bbolt.workload_sha256 {
        return Err(GateError::WorkloadHashMismatch {
            mango_sha: mango.workload_sha256.clone(),
            bbolt_sha: bbolt.workload_sha256.clone(),
        });
    }
    if mango.workload_version != bbolt.workload_version {
        return Err(GateError::WorkloadVersionMismatch {
            mango: mango.workload_version,
            bbolt: bbolt.workload_version,
        });
    }
    Ok(())
}

/// Read both signatures, enforce N9 §1/§2/§3 (existence,
/// co-residency, Linux Tier-1).
fn validate_signatures(
    mango: &ResultFile,
    mango_dir: &Path,
    bbolt: &ResultFile,
    bbolt_dir: &Path,
) -> Result<(SignatureInfo, SignatureInfo), GateError> {
    let (_, mango_sig) = read_signature(mango_dir, &mango.signature_path, "mango")?;
    if mango_sig.os != "linux" || mango_sig.tier != 1 {
        return Err(GateError::SignatureNotTier1 {
            which: "mango",
            os: mango_sig.os.clone(),
            tier: mango_sig.tier,
        });
    }
    let (_, bbolt_sig) = read_signature(bbolt_dir, &bbolt.signature_path, "bbolt")?;
    if bbolt_sig.os != "linux" || bbolt_sig.tier != 1 {
        return Err(GateError::SignatureNotTier1 {
            which: "bbolt",
            os: bbolt_sig.os.clone(),
            tier: bbolt_sig.tier,
        });
    }
    Ok((mango_sig, bbolt_sig))
}

/// Borrowed metric index: name → `&MetricRecord`. Used for the
/// per-metric pairing pass.
type MetricIndex<'a> = BTreeMap<&'a str, &'a MetricRecord>;

/// Index both sides by metric name, returning an error if the two
/// metric sets are not symmetric (one side has a metric the other
/// lacks).
fn pair_metrics<'a>(
    mango: &'a ResultFile,
    bbolt: &'a ResultFile,
) -> Result<(MetricIndex<'a>, MetricIndex<'a>), GateError> {
    let mango_by_name: MetricIndex<'a> = mango
        .metrics
        .iter()
        .map(|m| (m.metric.as_str(), m))
        .collect();
    let bbolt_by_name: MetricIndex<'a> = bbolt
        .metrics
        .iter()
        .map(|m| (m.metric.as_str(), m))
        .collect();
    for name in mango_by_name.keys() {
        if !bbolt_by_name.contains_key(name) {
            return Err(GateError::MissingMetricPair {
                which: "mango",
                metric: (*name).to_owned(),
            });
        }
    }
    for name in bbolt_by_name.keys() {
        if !mango_by_name.contains_key(name) {
            return Err(GateError::MissingMetricPair {
                which: "bbolt",
                metric: (*name).to_owned(),
            });
        }
    }
    Ok((mango_by_name, bbolt_by_name))
}

/// Aggregate counters carried through the per-metric loop.
#[derive(Default)]
struct MetricAggregate {
    merged: Vec<MergedMetric>,
    skipped: Vec<(String, &'static str)>,
    fail_reasons: Vec<String>,
    wins: usize,
    losses: usize,
}

/// Compute the per-metric merge for one paired record. Returns
/// `Ok(None)` when the metric is skipped (asymmetric fairness);
/// otherwise `Ok(Some(MergedMetric))`. Fails on bootstrap-input
/// errors and on unknown metric names.
fn process_metric(
    name: &str,
    mango_metric: &MetricRecord,
    bbolt_metric: &MetricRecord,
    rng_seed: u64,
) -> Result<MetricStep, GateError> {
    let kind = infer_kind(name).ok_or_else(|| GateError::UnknownMetric {
        name: name.to_owned(),
    })?;

    // Fairness skip — either side flagged asymmetric.
    let mango_fairness = mango_metric.fairness.as_deref();
    let bbolt_fairness = bbolt_metric.fairness.as_deref();
    let fairness = mango_fairness.or(bbolt_fairness).map(str::to_owned);
    if mango_fairness == Some(FAIRNESS_ASYMMETRIC) || bbolt_fairness == Some(FAIRNESS_ASYMMETRIC) {
        return Ok(MetricStep::Skipped);
    }

    // Pre-existing `incomplete` from the run-side (S2 cold-cache
    // skip) — the gate observes it and fails outright. The
    // bootstrap is not run; the per-run vector is reported as-is.
    if mango_metric.verdict == Some(Verdict::Incomplete)
        || bbolt_metric.verdict == Some(Verdict::Incomplete)
    {
        return Ok(MetricStep::Incomplete(MergedMetric {
            metric: name.to_owned(),
            kind,
            mango_runs: mango_metric.engine_runs.clone(),
            bbolt_runs: bbolt_metric.engine_runs.clone(),
            ratio_mean: f64::NAN,
            ratio_lower_95: f64::NAN,
            ratio_upper_95: f64::NAN,
            verdict: Verdict::Incomplete,
            fairness,
        }));
    }

    // Bootstrap CI per metric. Seed deterministically per metric so
    // each line in the report is reproducible.
    let metric_seed = mix_seed(rng_seed, name);
    let ci: BootstrapCi = stats::bootstrap_ci(
        &mango_metric.engine_runs,
        &bbolt_metric.engine_runs,
        kind.higher_is(),
        stats::DEFAULT_RESAMPLES,
        metric_seed,
    )
    .map_err(|source| GateError::Stats {
        metric: name.to_owned(),
        source,
    })?;

    Ok(MetricStep::Computed(MergedMetric {
        metric: name.to_owned(),
        kind,
        mango_runs: mango_metric.engine_runs.clone(),
        bbolt_runs: bbolt_metric.engine_runs.clone(),
        ratio_mean: ci.ratio_mean,
        ratio_lower_95: ci.ratio_lower_95,
        ratio_upper_95: ci.ratio_upper_95,
        verdict: ci.verdict,
        fairness,
    }))
}

/// Outcome of `process_metric`. Three cases the aggregate has to
/// route differently.
enum MetricStep {
    Skipped,
    Incomplete(MergedMetric),
    Computed(MergedMetric),
}

/// Apply the gate to a (`mango_dir`, `bbolt_dir`) pair already
/// loaded into [`ResultFile`]s. The two `dir` arguments are the
/// directories the JSONs were read from — the gate uses them to
/// resolve `signature_path`. The `rng_seed` determines the
/// bootstrap RNG.
///
/// Returns a [`GateReport`] on any non-fatal outcome (including
/// `Fail`). Returns [`GateError`] only for *structural* failures —
/// schema mismatch, missing signature, non-Tier-1 hardware, etc.
/// Those are not benchmark losses; they mean the inputs are not
/// well-formed enough to gate.
pub fn gate(
    mango: &ResultFile,
    mango_dir: &Path,
    bbolt: &ResultFile,
    bbolt_dir: &Path,
    rng_seed: u64,
) -> Result<GateReport, GateError> {
    validate_headers(mango, bbolt)?;
    let (mango_sig, bbolt_sig) = validate_signatures(mango, mango_dir, bbolt, bbolt_dir)?;
    let (mango_by_name, bbolt_by_name) = pair_metrics(mango, bbolt)?;

    let mut agg = MetricAggregate {
        merged: Vec::with_capacity(mango_by_name.len()),
        ..MetricAggregate::default()
    };

    for (name, mango_metric) in &mango_by_name {
        let bbolt_metric =
            bbolt_by_name
                .get(name)
                .copied()
                .ok_or_else(|| GateError::MissingMetricPair {
                    which: "bbolt",
                    metric: (*name).to_owned(),
                })?;
        match process_metric(name, mango_metric, bbolt_metric, rng_seed)? {
            MetricStep::Skipped => {
                agg.skipped
                    .push(((*name).to_owned(), "fairness=asymmetric"));
            }
            MetricStep::Incomplete(merged) => {
                agg.fail_reasons.push(format!("incomplete on {name}"));
                agg.merged.push(merged);
            }
            MetricStep::Computed(merged) => {
                tally_verdict(&merged, &mut agg);
                agg.merged.push(merged);
            }
        }
    }

    let verdict = decide_verdict(&mut agg);

    Ok(GateReport {
        verdict,
        fail_reasons: agg.fail_reasons,
        merged: agg.merged,
        skipped: agg.skipped,
        mango_signature: mango_sig,
        bbolt_signature: bbolt_sig,
        workload_sha256: mango.workload_sha256.clone(),
        workload_version: mango.workload_version,
    })
}

/// Update the win/loss counters and fail-reason list based on the
/// computed verdict. `Incomplete` is filtered upstream; we still
/// route it defensively here in case `bootstrap_ci`'s contract
/// ever changes.
fn tally_verdict(merged: &MergedMetric, agg: &mut MetricAggregate) {
    match merged.verdict {
        Verdict::Win => {
            #[allow(
                clippy::arithmetic_side_effects,
                reason = "wins is bounded by the metric set size (single digit)"
            )]
            {
                agg.wins += 1;
            }
        }
        Verdict::Loss => {
            #[allow(
                clippy::arithmetic_side_effects,
                reason = "losses is bounded by the metric set size (single digit)"
            )]
            {
                agg.losses += 1;
            }
            agg.fail_reasons.push(format!("loss on {}", merged.metric));
        }
        Verdict::Tie => {}
        Verdict::Incomplete => {
            agg.fail_reasons
                .push(format!("incomplete on {}", merged.metric));
        }
    }
}

/// Roll up the aggregate into the final `Pass` / `Fail` verdict.
/// Adds an "all-ties" fail reason when no metric reached Win and
/// the reasons list is otherwise empty (the only way `Pass` is
/// missed without an explicit reason).
fn decide_verdict(agg: &mut MetricAggregate) -> GateVerdict {
    if agg.losses == 0 && agg.wins >= 1 && agg.fail_reasons.is_empty() {
        GateVerdict::Pass
    } else {
        if agg.wins == 0 && agg.fail_reasons.is_empty() {
            agg.fail_reasons
                .push("no metric reached Win (all ties)".to_owned());
        }
        GateVerdict::Fail
    }
}

/// Mix the operator-supplied `rng_seed` with the metric name so
/// per-metric bootstraps don't share the same RNG stream. We use a
/// FNV-1a 64-bit fold of the metric name into a `u64`, then xor
/// into the operator seed. FNV is non-cryptographic but cheap and
/// fully deterministic, which is what we want for the bootstrap.
fn mix_seed(rng_seed: u64, name: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    rng_seed ^ hash
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::float_cmp
    )]

    use super::*;
    use crate::measure::{MetricRecord, ResultFile};
    use std::fs;

    /// Build a `MetricRecord` skeleton with caller-supplied per-run
    /// vector and optional fairness flag. The other aggregate
    /// fields (mean/median/stddev/verdict/ratio) are filled with
    /// placeholders the gate doesn't consume on the per-engine
    /// record (only `engine_runs` and `fairness` matter).
    fn metric(
        name: &str,
        engine: &str,
        runs: Vec<f64>,
        fairness: Option<&str>,
        verdict: Option<Verdict>,
    ) -> MetricRecord {
        MetricRecord {
            metric: name.to_owned(),
            engine: engine.to_owned(),
            engine_runs: runs,
            mean: 0.0,
            median: 0.0,
            stddev: 0.0,
            ratio_lower_95: None,
            ratio_upper_95: None,
            verdict,
            fairness: fairness.map(str::to_owned),
        }
    }

    fn result_file(engine: &str, sig: &str, metrics: Vec<MetricRecord>) -> ResultFile {
        ResultFile {
            format_version: 1,
            engine: engine.to_owned(),
            workload_sha256: "deadbeef".to_owned(),
            workload_version: 1,
            signature_path: sig.to_owned(),
            started_at: "2026-05-03T18:30:11Z".to_owned(),
            runs: Vec::new(),
            metrics,
        }
    }

    /// Write a `signature.txt` with a `BENCH_HW v1: …` line and
    /// caller-controlled trailer. Returns the absolute directory.
    fn write_sig_dir(parent: &tempfile::TempDir, body: &str) -> PathBuf {
        let dir = parent.path().to_path_buf();
        let p = dir.join("signature.txt");
        fs::write(&p, body).unwrap();
        dir
    }

    fn linux_tier1_sig() -> String {
        "BENCH_HW v1: os=linux tier=1 cpu=epyc mem_gb=64 git_sha=abc123\n".to_owned()
    }

    #[test]
    fn higher_is_per_kind_is_correct() {
        assert_eq!(MetricKind::Throughput.higher_is(), HigherIs::Better);
        assert_eq!(MetricKind::Latency.higher_is(), HigherIs::Worse);
        assert_eq!(MetricKind::Size.higher_is(), HigherIs::Worse);
    }

    #[test]
    fn infer_kind_table_is_complete_for_phase_1_metrics() {
        assert_eq!(
            infer_kind("write_throughput_unbatched"),
            Some(MetricKind::Throughput)
        );
        assert_eq!(
            infer_kind("write_throughput_batched"),
            Some(MetricKind::Throughput)
        );
        assert_eq!(infer_kind("range_throughput"), Some(MetricKind::Throughput));
        assert_eq!(
            infer_kind("read_latency_p99_hot"),
            Some(MetricKind::Latency)
        );
        assert_eq!(
            infer_kind("read_latency_p99_cold"),
            Some(MetricKind::Latency)
        );
        assert_eq!(
            infer_kind("read_latency_p99_zipfian"),
            Some(MetricKind::Latency)
        );
        assert_eq!(infer_kind("on_disk_size"), Some(MetricKind::Size));
        assert_eq!(infer_kind("write_throughput_furlongs"), None);
        assert_eq!(infer_kind(""), None);
    }

    #[test]
    fn parse_signature_accepts_linux_tier1() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("signature.txt");
        fs::write(&p, linux_tier1_sig()).unwrap();
        let info = parse_signature_text(&linux_tier1_sig(), "mango", &p).unwrap();
        assert_eq!(info.os, "linux");
        assert_eq!(info.tier, 1);
        assert!(info.raw.iter().any(|(k, _)| k == "git_sha"));
    }

    #[test]
    fn parse_signature_rejects_missing_header() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("signature.txt");
        let err = parse_signature_text("not a bench signature\n", "mango", &p).unwrap_err();
        assert!(matches!(
            err,
            GateError::SignatureParse {
                reason: "no `BENCH_HW v1:` line found",
                ..
            }
        ));
    }

    #[test]
    fn parse_signature_rejects_missing_os_or_tier() {
        let p = Path::new("/tmp/x");
        let err = parse_signature_text("BENCH_HW v1: tier=1\n", "mango", p).unwrap_err();
        assert!(matches!(
            err,
            GateError::SignatureParse {
                reason: "missing `os=` field",
                ..
            }
        ));
        let err = parse_signature_text("BENCH_HW v1: os=linux\n", "mango", p).unwrap_err();
        assert!(matches!(
            err,
            GateError::SignatureParse {
                reason: "missing `tier=` field",
                ..
            }
        ));
    }

    #[test]
    fn parse_signature_rejects_non_numeric_tier() {
        let p = Path::new("/tmp/x");
        let err =
            parse_signature_text("BENCH_HW v1: os=linux tier=primary\n", "mango", p).unwrap_err();
        assert!(matches!(
            err,
            GateError::SignatureParse {
                reason: "`tier=` value is not a u32",
                ..
            }
        ));
    }

    #[test]
    fn parse_signature_rejects_malformed_token() {
        let p = Path::new("/tmp/x");
        let err = parse_signature_text("BENCH_HW v1: os=linux tier=1 lonely_token\n", "mango", p)
            .unwrap_err();
        assert!(matches!(
            err,
            GateError::SignatureParse {
                reason: "malformed key=value token",
                ..
            }
        ));
    }

    #[test]
    fn read_signature_rejects_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_signature(dir.path(), "no-such.txt", "mango").unwrap_err();
        assert!(matches!(err, GateError::SignatureIo { .. }));
    }

    #[test]
    fn read_signature_rejects_non_co_resident_path() {
        // Two sibling temp dirs; sig lives in one, the JSON dir
        // (the one we pass) is the other.
        let json_dir = tempfile::tempdir().unwrap();
        let sig_dir = tempfile::tempdir().unwrap();
        let sig_path = sig_dir.path().join("signature.txt");
        fs::write(&sig_path, linux_tier1_sig()).unwrap();

        // Build a "../sibling/signature.txt" relative path from
        // `json_dir`. We compute it explicitly because tempdir
        // names are random.
        let json_canon = json_dir.path().canonicalize().unwrap();
        let sig_canon = sig_path.canonicalize().unwrap();
        let sig_canon_str = sig_canon.to_string_lossy().to_string();
        let json_canon_str = json_canon.to_string_lossy().to_string();
        // We can pass the absolute path; canonicalize will
        // collapse it. The function should still detect that the
        // parent is `sig_dir`, not `json_dir`.
        let err = read_signature(&json_canon, &sig_canon_str, "mango").unwrap_err();
        assert!(
            matches!(err, GateError::SignatureNotCoResident { .. }),
            "expected NotCoResident, got {err:?} (json={json_canon_str} sig={sig_canon_str})"
        );
    }

    /// Symlink axis of the co-residency check (N9 §3): a contributor
    /// MUST NOT be able to reach a non-co-resident signature by
    /// dropping a symlink inside `json_dir` that points at a sibling
    /// directory's signature file. `Path::canonicalize` resolves the
    /// link, the resolved parent diverges from `json_dir`, and the
    /// gate refuses with `SignatureNotCoResident`.
    #[cfg(unix)]
    #[test]
    fn read_signature_rejects_symlink_to_sibling_dir() {
        use std::os::unix::fs::symlink;

        let json_dir = tempfile::tempdir().unwrap();
        let sig_dir = tempfile::tempdir().unwrap();
        let real_sig = sig_dir.path().join("signature.txt");
        fs::write(&real_sig, linux_tier1_sig()).unwrap();

        // Drop a symlink at `json_dir/signature.txt` → real_sig.
        let link_in_json = json_dir.path().join("signature.txt");
        symlink(&real_sig, &link_in_json).unwrap();

        // The signature_path is a co-resident-looking *string*, but
        // canonicalize resolves the link to the sibling tempdir.
        let err = read_signature(json_dir.path(), "signature.txt", "mango").unwrap_err();
        assert!(
            matches!(err, GateError::SignatureNotCoResident { .. }),
            "expected NotCoResident on symlinked signature, got {err:?}"
        );
    }

    /// Helper: build mango+bbolt result files that should pass.
    /// Mango wins clearly on `write_throughput_unbatched`, ties on
    /// the others — meets "≥ 1 Win, 0 Loss".
    fn passing_pair() -> (ResultFile, ResultFile) {
        let n = 20;
        // Synthetic: mango_w = 110, bbolt_w = 100 (mango wins by 10%)
        let mango_w: Vec<f64> = (0..n).map(|i| 110.0 + (f64::from(i) % 3.0)).collect();
        let bbolt_w: Vec<f64> = (0..n).map(|i| 100.0 + (f64::from(i) % 3.0)).collect();
        // Latency ties: 50 µs both sides (in ns).
        let lat: Vec<f64> = (0..n).map(|i| 50_000.0 + f64::from(i)).collect();

        let mango = result_file(
            "mango",
            "signature.txt",
            vec![
                metric("write_throughput_unbatched", "mango", mango_w, None, None),
                metric("read_latency_p99_hot", "mango", lat.clone(), None, None),
                metric("on_disk_size", "mango", lat.clone(), None, None),
            ],
        );
        let bbolt = result_file(
            "bbolt",
            "signature.txt",
            vec![
                metric("write_throughput_unbatched", "bbolt", bbolt_w, None, None),
                metric("read_latency_p99_hot", "bbolt", lat.clone(), None, None),
                metric("on_disk_size", "bbolt", lat, None, None),
            ],
        );
        (mango, bbolt)
    }

    #[test]
    fn pass_when_one_win_zero_loss() {
        let mango_dir = tempfile::tempdir().unwrap();
        let bbolt_dir = tempfile::tempdir().unwrap();
        let m_dir = write_sig_dir(&mango_dir, &linux_tier1_sig());
        let b_dir = write_sig_dir(&bbolt_dir, &linux_tier1_sig());

        let (mango, bbolt) = passing_pair();
        let report = gate(&mango, &m_dir, &bbolt, &b_dir, 12345).unwrap();
        assert_eq!(report.verdict, GateVerdict::Pass, "report = {report:?}");
        assert!(report.fail_reasons.is_empty());
        assert!(report
            .merged
            .iter()
            .any(|m| m.metric == "write_throughput_unbatched" && m.verdict == Verdict::Win));
    }

    #[test]
    fn fail_on_loss() {
        let mango_dir = tempfile::tempdir().unwrap();
        let bbolt_dir = tempfile::tempdir().unwrap();
        let m_dir = write_sig_dir(&mango_dir, &linux_tier1_sig());
        let b_dir = write_sig_dir(&bbolt_dir, &linux_tier1_sig());

        // Mango LOSES on write_throughput: 90 vs 110.
        let n = 20;
        let mango_w: Vec<f64> = (0..n).map(|i| 90.0 + (f64::from(i) % 3.0)).collect();
        let bbolt_w: Vec<f64> = (0..n).map(|i| 110.0 + (f64::from(i) % 3.0)).collect();
        let lat: Vec<f64> = (0..n).map(|i| 50_000.0 + f64::from(i)).collect();

        let mango = result_file(
            "mango",
            "signature.txt",
            vec![
                metric("write_throughput_unbatched", "mango", mango_w, None, None),
                metric("read_latency_p99_hot", "mango", lat.clone(), None, None),
            ],
        );
        let bbolt = result_file(
            "bbolt",
            "signature.txt",
            vec![
                metric("write_throughput_unbatched", "bbolt", bbolt_w, None, None),
                metric("read_latency_p99_hot", "bbolt", lat, None, None),
            ],
        );
        let report = gate(&mango, &m_dir, &bbolt, &b_dir, 17).unwrap();
        assert_eq!(report.verdict, GateVerdict::Fail);
        assert!(
            report
                .fail_reasons
                .iter()
                .any(|s| s.contains("loss on write_throughput_unbatched")),
            "reasons = {:?}",
            report.fail_reasons
        );
    }

    #[test]
    fn fail_on_all_ties_no_win() {
        let mango_dir = tempfile::tempdir().unwrap();
        let bbolt_dir = tempfile::tempdir().unwrap();
        let m_dir = write_sig_dir(&mango_dir, &linux_tier1_sig());
        let b_dir = write_sig_dir(&bbolt_dir, &linux_tier1_sig());

        // Identical distributions → all Tie.
        let n = 20;
        let v: Vec<f64> = (0..n).map(|i| 100.0 + f64::from(i)).collect();
        let mango = result_file(
            "mango",
            "signature.txt",
            vec![metric(
                "write_throughput_unbatched",
                "mango",
                v.clone(),
                None,
                None,
            )],
        );
        let bbolt = result_file(
            "bbolt",
            "signature.txt",
            vec![metric("write_throughput_unbatched", "bbolt", v, None, None)],
        );
        let report = gate(&mango, &m_dir, &bbolt, &b_dir, 7).unwrap();
        assert_eq!(report.verdict, GateVerdict::Fail);
        assert!(
            report
                .fail_reasons
                .iter()
                .any(|s| s.contains("no metric reached Win")),
            "reasons = {:?}",
            report.fail_reasons
        );
    }

    #[test]
    fn skip_metric_marked_asymmetric() {
        let mango_dir = tempfile::tempdir().unwrap();
        let bbolt_dir = tempfile::tempdir().unwrap();
        let m_dir = write_sig_dir(&mango_dir, &linux_tier1_sig());
        let b_dir = write_sig_dir(&bbolt_dir, &linux_tier1_sig());

        // Mango "loses" on range_throughput, but it's asymmetric →
        // skipped. write_throughput is a clean win → Pass overall.
        let n = 20;
        let mango_w: Vec<f64> = (0..n).map(|i| 110.0 + (f64::from(i) % 3.0)).collect();
        let bbolt_w: Vec<f64> = (0..n).map(|i| 100.0 + (f64::from(i) % 3.0)).collect();
        let mango_r: Vec<f64> = (0..n).map(|i| 50.0 + (f64::from(i) % 3.0)).collect();
        let bbolt_r: Vec<f64> = (0..n).map(|i| 200.0 + (f64::from(i) % 3.0)).collect();

        let mango = result_file(
            "mango",
            "signature.txt",
            vec![
                metric("write_throughput_unbatched", "mango", mango_w, None, None),
                metric(
                    "range_throughput",
                    "mango",
                    mango_r,
                    Some(FAIRNESS_ASYMMETRIC),
                    None,
                ),
            ],
        );
        let bbolt = result_file(
            "bbolt",
            "signature.txt",
            vec![
                metric("write_throughput_unbatched", "bbolt", bbolt_w, None, None),
                metric(
                    "range_throughput",
                    "bbolt",
                    bbolt_r,
                    Some(FAIRNESS_ASYMMETRIC),
                    None,
                ),
            ],
        );
        let report = gate(&mango, &m_dir, &bbolt, &b_dir, 5).unwrap();
        assert_eq!(report.verdict, GateVerdict::Pass);
        assert!(report.skipped.iter().any(|(n, _)| n == "range_throughput"));
        assert!(
            !report.merged.iter().any(|m| m.metric == "range_throughput"),
            "asymmetric metric leaked into merged: {:?}",
            report.merged
        );
    }

    #[test]
    fn fail_on_incomplete_metric() {
        let mango_dir = tempfile::tempdir().unwrap();
        let bbolt_dir = tempfile::tempdir().unwrap();
        let m_dir = write_sig_dir(&mango_dir, &linux_tier1_sig());
        let b_dir = write_sig_dir(&bbolt_dir, &linux_tier1_sig());

        // Mango wins on write but the cold-cache metric is
        // `incomplete` (macOS-style). Gate must fail anyway —
        // L829's "no incomplete" rule.
        let n = 20;
        let mango_w: Vec<f64> = (0..n).map(|i| 110.0 + (f64::from(i) % 3.0)).collect();
        let bbolt_w: Vec<f64> = (0..n).map(|i| 100.0 + (f64::from(i) % 3.0)).collect();
        let lat: Vec<f64> = (0..n).map(|i| 50_000.0 + f64::from(i)).collect();

        let mango = result_file(
            "mango",
            "signature.txt",
            vec![
                metric("write_throughput_unbatched", "mango", mango_w, None, None),
                metric(
                    "read_latency_p99_cold",
                    "mango",
                    lat.clone(),
                    None,
                    Some(Verdict::Incomplete),
                ),
            ],
        );
        let bbolt = result_file(
            "bbolt",
            "signature.txt",
            vec![
                metric("write_throughput_unbatched", "bbolt", bbolt_w, None, None),
                metric(
                    "read_latency_p99_cold",
                    "bbolt",
                    lat,
                    None,
                    Some(Verdict::Incomplete),
                ),
            ],
        );
        let report = gate(&mango, &m_dir, &bbolt, &b_dir, 11).unwrap();
        assert_eq!(report.verdict, GateVerdict::Fail);
        assert!(
            report
                .fail_reasons
                .iter()
                .any(|s| s.contains("incomplete on read_latency_p99_cold")),
            "reasons = {:?}",
            report.fail_reasons
        );
    }

    #[test]
    fn rejects_format_version_mismatch() {
        let mango_dir = tempfile::tempdir().unwrap();
        let bbolt_dir = tempfile::tempdir().unwrap();
        let m_dir = write_sig_dir(&mango_dir, &linux_tier1_sig());
        let b_dir = write_sig_dir(&bbolt_dir, &linux_tier1_sig());

        let (mut mango, bbolt) = passing_pair();
        mango.format_version = 2;
        let err = gate(&mango, &m_dir, &bbolt, &b_dir, 0).unwrap_err();
        assert!(matches!(
            err,
            GateError::FormatVersion {
                which: "mango",
                actual: 2
            }
        ));
    }

    #[test]
    fn rejects_workload_hash_mismatch() {
        let mango_dir = tempfile::tempdir().unwrap();
        let bbolt_dir = tempfile::tempdir().unwrap();
        let m_dir = write_sig_dir(&mango_dir, &linux_tier1_sig());
        let b_dir = write_sig_dir(&bbolt_dir, &linux_tier1_sig());

        let (mango, mut bbolt) = passing_pair();
        bbolt.workload_sha256 = "feedface".to_owned();
        let err = gate(&mango, &m_dir, &bbolt, &b_dir, 0).unwrap_err();
        assert!(matches!(err, GateError::WorkloadHashMismatch { .. }));
    }

    #[test]
    fn rejects_engine_mismatch() {
        let mango_dir = tempfile::tempdir().unwrap();
        let bbolt_dir = tempfile::tempdir().unwrap();
        let m_dir = write_sig_dir(&mango_dir, &linux_tier1_sig());
        let b_dir = write_sig_dir(&bbolt_dir, &linux_tier1_sig());

        let (mango, mut bbolt) = passing_pair();
        bbolt.engine = "mango".to_owned(); // two mango files
        let err = gate(&mango, &m_dir, &bbolt, &b_dir, 0).unwrap_err();
        assert!(matches!(err, GateError::EngineMismatch { .. }));
    }

    #[test]
    fn rejects_unknown_metric() {
        let mango_dir = tempfile::tempdir().unwrap();
        let bbolt_dir = tempfile::tempdir().unwrap();
        let m_dir = write_sig_dir(&mango_dir, &linux_tier1_sig());
        let b_dir = write_sig_dir(&bbolt_dir, &linux_tier1_sig());

        let n = 20;
        let v: Vec<f64> = (0..n).map(|i| 100.0 + f64::from(i)).collect();
        let mango = result_file(
            "mango",
            "signature.txt",
            vec![metric("invented_metric", "mango", v.clone(), None, None)],
        );
        let bbolt = result_file(
            "bbolt",
            "signature.txt",
            vec![metric("invented_metric", "bbolt", v, None, None)],
        );
        let err = gate(&mango, &m_dir, &bbolt, &b_dir, 0).unwrap_err();
        assert!(matches!(err, GateError::UnknownMetric { .. }));
    }

    #[test]
    fn rejects_missing_metric_pair() {
        let mango_dir = tempfile::tempdir().unwrap();
        let bbolt_dir = tempfile::tempdir().unwrap();
        let m_dir = write_sig_dir(&mango_dir, &linux_tier1_sig());
        let b_dir = write_sig_dir(&bbolt_dir, &linux_tier1_sig());

        let n = 20;
        let v: Vec<f64> = (0..n).map(|i| 100.0 + f64::from(i)).collect();
        let mango = result_file(
            "mango",
            "signature.txt",
            vec![
                metric("write_throughput_unbatched", "mango", v.clone(), None, None),
                metric("read_latency_p99_hot", "mango", v.clone(), None, None),
            ],
        );
        let bbolt = result_file(
            "bbolt",
            "signature.txt",
            vec![metric("write_throughput_unbatched", "bbolt", v, None, None)],
        );
        let err = gate(&mango, &m_dir, &bbolt, &b_dir, 0).unwrap_err();
        assert!(matches!(err, GateError::MissingMetricPair { .. }));
    }

    #[test]
    fn rejects_non_linux_signature() {
        let mango_dir = tempfile::tempdir().unwrap();
        let bbolt_dir = tempfile::tempdir().unwrap();
        // Mango signature is darwin → reject.
        let m_dir = write_sig_dir(&mango_dir, "BENCH_HW v1: os=darwin tier=1 cpu=m2\n");
        let b_dir = write_sig_dir(&bbolt_dir, &linux_tier1_sig());
        let (mango, bbolt) = passing_pair();
        let err = gate(&mango, &m_dir, &bbolt, &b_dir, 0).unwrap_err();
        assert!(matches!(err, GateError::SignatureNotTier1 { .. }));
    }

    #[test]
    fn rejects_non_tier1_signature() {
        let mango_dir = tempfile::tempdir().unwrap();
        let bbolt_dir = tempfile::tempdir().unwrap();
        let m_dir = write_sig_dir(&mango_dir, &linux_tier1_sig());
        let b_dir = write_sig_dir(&bbolt_dir, "BENCH_HW v1: os=linux tier=3 cpu=tiny\n");
        let (mango, bbolt) = passing_pair();
        let err = gate(&mango, &m_dir, &bbolt, &b_dir, 0).unwrap_err();
        assert!(matches!(
            err,
            GateError::SignatureNotTier1 {
                which: "bbolt",
                tier: 3,
                ..
            }
        ));
    }

    #[test]
    fn mix_seed_is_deterministic_and_per_metric_distinct() {
        let a = mix_seed(42, "write_throughput_unbatched");
        let b = mix_seed(42, "read_latency_p99_hot");
        assert_eq!(a, mix_seed(42, "write_throughput_unbatched"));
        assert_ne!(
            a, b,
            "two different metric names produced the same mixed seed"
        );
        // Different operator seed → different output for same name.
        assert_ne!(a, mix_seed(43, "write_throughput_unbatched"));
    }
}
