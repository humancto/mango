//! Bench-harness `gate` binary.
//!
//! Reads two result JSONs (`mango.json`, `bbolt.json`) and applies
//! the win/loss verdict per S1 / S2 / S3 / N9 / N10. The verdict
//! logic is in `mango_bench_storage::gate`; this binary is the CLI
//! wrapper that does the JSON I/O + exit-code mapping.
//!
//! Exit codes:
//! - `0` — gate passed (≥ 1 Win, 0 Loss, no `incomplete`,
//!   signatures parse as Linux Tier-1).
//! - `1` — gate failed (loss, no-win-only-tie, or any
//!   `incomplete`).
//! - `2` — structural error (schema mismatch, missing signature,
//!   non-Tier-1 hardware, unknown metric, etc.). Distinguished
//!   from `1` so a CI script can tell "the gate ran and decided
//!   the bench is bad" from "the gate refused to run".
//!
//! Usage:
//! ```text
//! gate <mango.json> <bbolt.json> [--rng-seed <u64>]
//! ```

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mango_bench_storage::gate::{self, GateReport, GateVerdict, DEFAULT_GATE_RNG_SEED};
use mango_bench_storage::measure::ResultFile;

fn main() -> ExitCode {
    match run() {
        Ok(ExitOutcome::Pass) => ExitCode::from(0),
        Ok(ExitOutcome::Fail) => ExitCode::from(1),
        Err(err) => {
            let mut stderr = std::io::stderr();
            let _ = writeln!(stderr, "gate: structural error: {err}");
            ExitCode::from(2)
        }
    }
}

enum ExitOutcome {
    Pass,
    Fail,
}

#[derive(Debug, thiserror::Error)]
enum BinError {
    #[error("usage: gate <mango.json> <bbolt.json> [--rng-seed <u64>]")]
    Usage,
    #[error("--rng-seed requires a u64 value")]
    BadSeed,
    #[error("read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path}: result JSON has no parent directory; cannot resolve signature_path")]
    NoParent { path: PathBuf },
    #[error("{0}")]
    Gate(#[from] gate::GateError),
}

fn run() -> Result<ExitOutcome, BinError> {
    let (mango_path, bbolt_path, rng_seed) = parse_args()?;
    let mango = load_result(&mango_path)?;
    let bbolt = load_result(&bbolt_path)?;
    let mango_dir = parent_or_err(&mango_path)?;
    let bbolt_dir = parent_or_err(&bbolt_path)?;

    let report = gate::gate(&mango, mango_dir, &bbolt, bbolt_dir, rng_seed)?;
    print_report(&report);
    Ok(match report.verdict {
        GateVerdict::Pass => ExitOutcome::Pass,
        // Fail is the only other current variant; `non_exhaustive`
        // on the enum forces the wildcard. A future variant lands
        // in the conservative bucket (exit 1 = Fail) until the bin
        // teaches itself how to render it.
        GateVerdict::Fail | _ => ExitOutcome::Fail,
    })
}

fn parse_args() -> Result<(PathBuf, PathBuf, u64), BinError> {
    let mut args = env::args_os().skip(1);
    let mut positional: Vec<PathBuf> = Vec::with_capacity(2);
    let mut rng_seed: u64 = DEFAULT_GATE_RNG_SEED;

    while let Some(arg) = args.next() {
        if arg == OsStr::new("--rng-seed") {
            let v = args.next().ok_or(BinError::BadSeed)?;
            let s = v.to_string_lossy();
            rng_seed = s.parse::<u64>().map_err(|_| BinError::BadSeed)?;
        } else if arg == OsStr::new("--help") || arg == OsStr::new("-h") {
            return Err(BinError::Usage);
        } else {
            positional.push(PathBuf::from(arg));
        }
    }

    if positional.len() != 2 {
        return Err(BinError::Usage);
    }
    let bbolt_path = positional.pop().ok_or(BinError::Usage)?;
    let mango_path = positional.pop().ok_or(BinError::Usage)?;
    Ok((mango_path, bbolt_path, rng_seed))
}

fn load_result(path: &Path) -> Result<ResultFile, BinError> {
    let bytes = fs::read(path).map_err(|source| BinError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| BinError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn parent_or_err(path: &Path) -> Result<&Path, BinError> {
    path.parent().ok_or_else(|| BinError::NoParent {
        path: path.to_path_buf(),
    })
}

fn print_report(report: &GateReport) {
    let mut stdout = std::io::stdout();
    let _ = writeln!(
        stdout,
        "gate: workload_sha256={} workload_version={}",
        report.workload_sha256, report.workload_version
    );
    let _ = writeln!(
        stdout,
        "gate: mango signature: os={} tier={}",
        report.mango_signature.os, report.mango_signature.tier
    );
    let _ = writeln!(
        stdout,
        "gate: bbolt signature: os={} tier={}",
        report.bbolt_signature.os, report.bbolt_signature.tier
    );
    for m in &report.merged {
        let _ = writeln!(
            stdout,
            "gate: metric={} kind={:?} verdict={:?} ratio_mean={:.4} ci=[{:.4}, {:.4}]",
            m.metric, m.kind, m.verdict, m.ratio_mean, m.ratio_lower_95, m.ratio_upper_95
        );
    }
    for (name, reason) in &report.skipped {
        let _ = writeln!(stdout, "gate: skipped {name}: {reason}");
    }
    for reason in &report.fail_reasons {
        let _ = writeln!(stdout, "gate: fail-reason: {reason}");
    }
    let _ = writeln!(stdout, "gate: verdict={:?}", report.verdict);
}
