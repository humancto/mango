// Binary entry point for the cargo-vet `[exemptions]` TTL check.
//
// Reads `supply-chain/config.toml` (relative to the repo root that
// `CARGO_MANIFEST_DIR/..` points at, or the CWD — workflow always
// runs from the repo root). Parses every `[[exemptions.<crate>]]`
// entry, extracts `review-by: YYYY-MM-DD` from `notes`, and fails if
// any date is in the past. Complements `cargo vet renew --expiring`,
// which handles the `[trusted]` side of the TTL story.
//
// This binary is a CLI tool; `println!` / `eprintln!` are how it
// communicates to the contributor and to CI step summaries. The
// workspace-wide ban on print macros does not apply here.
#![allow(clippy::print_stdout, clippy::print_stderr)]
//
// Exit codes:
//   0  PASS — every exemption either has a future review-by or none
//             at all (the latter is advisory-only; see --strict)
//   1  FAIL — at least one exemption's review-by is in the past
//   2  FAIL — a review-by date was present but malformed, or the
//             config file failed TOML syntax
//
// Flags:
//   --list     print every (crate, version, review-by) tuple and exit 0
//   --strict   treat missing review-by tokens as hard errors (exit 1)
//   --config <path>  read from <path> instead of ./supply-chain/config.toml

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use time::OffsetDateTime;

use xtask_vet_ttl::{extract_exemption_reviews, partition_by_date, ParseError, DATE_FORMAT};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("xtask-vet-ttl: error: {e}");
            ExitCode::from(2)
        }
    }
}

struct Args {
    list: bool,
    strict: bool,
    config: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        list: false,
        strict: false,
        config: PathBuf::from("supply-chain/config.toml"),
    };
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--list" => args.list = true,
            "--strict" => args.strict = true,
            "--config" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--config requires a path".to_string())?;
                args.config = PathBuf::from(v);
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

fn print_help() {
    println!(
        "xtask-vet-ttl — TTL check for cargo-vet [exemptions] entries\n\
         \n\
         USAGE:\n    \
             cargo run -q -p xtask-vet-ttl -- [OPTIONS]\n\
         \n\
         OPTIONS:\n    \
             --list           print every (crate, version, review-by) and exit 0\n    \
             --strict         treat missing review-by as a failure (exit 1)\n    \
             --config <path>  read <path> instead of supply-chain/config.toml\n    \
             -h, --help       show this message\n\
         \n\
         EXIT CODES:\n    \
             0  PASS\n    \
             1  at least one review-by is in the past (or missing with --strict)\n    \
             2  malformed date or config.toml syntax error\n\
         "
    );
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let args = parse_args().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let raw = fs::read_to_string(&args.config)
        .map_err(|e| format!("cannot read {}: {e}", args.config.display()))?;
    let (reviews, soft_errors) = extract_exemption_reviews(&raw)?;

    // Any malformed-date soft error is always a hard fail — the
    // contributor wrote a TTL token but we couldn't parse it, which
    // means the intent exists and we shouldn't silently pass.
    let malformed: Vec<&ParseError> = soft_errors
        .iter()
        .filter(|e| matches!(e, ParseError::MalformedDate { .. }))
        .collect();
    let missing: Vec<&ParseError> = soft_errors
        .iter()
        .filter(|e| matches!(e, ParseError::MissingReviewBy { .. }))
        .collect();

    if args.list {
        println!("=== cargo-vet [exemptions] TTL check — listing ===");
        println!("config: {}", args.config.display());
        println!("entries with review-by: {}", reviews.len());
        for r in &reviews {
            let formatted = r
                .review_by
                .format(DATE_FORMAT)
                .unwrap_or_else(|_| "<unformattable>".to_string());
            println!("  {} @ {} -> {}", r.crate_name, r.version, formatted);
        }
        println!("entries missing review-by: {}", missing.len());
        for e in &missing {
            println!("  {e}");
        }
        println!("entries with malformed review-by: {}", malformed.len());
        for e in &malformed {
            println!("  {e}");
        }
        return Ok(ExitCode::from(0));
    }

    if !malformed.is_empty() {
        eprintln!("xtask-vet-ttl: malformed review-by date(s):");
        for e in &malformed {
            eprintln!("  {e}");
        }
        return Ok(ExitCode::from(2));
    }

    let today = OffsetDateTime::now_utc().date();
    let (expired, current) = partition_by_date(reviews, today);

    if !expired.is_empty() {
        let today_fmt = today.format(DATE_FORMAT).unwrap_or_else(|_| "???".into());
        eprintln!(
            "xtask-vet-ttl: {} exemption(s) past review-by (today = {}):",
            expired.len(),
            today_fmt
        );
        for r in &expired {
            let fmt = r
                .review_by
                .format(DATE_FORMAT)
                .unwrap_or_else(|_| "???".into());
            eprintln!("  {} @ {} -> review-by {}", r.crate_name, r.version, fmt);
        }
        eprintln!("\nrenew via `cargo vet renew --expiring` for [trusted] entries,");
        eprintln!("or bump the review-by date in supply-chain/config.toml for exemptions.");
        eprintln!("see docs/supply-chain-policy.md.");
        return Ok(ExitCode::from(1));
    }

    if args.strict && !missing.is_empty() {
        eprintln!(
            "xtask-vet-ttl: --strict: {} exemption(s) missing review-by:",
            missing.len()
        );
        for e in &missing {
            eprintln!("  {e}");
        }
        return Ok(ExitCode::from(1));
    }

    println!("xtask-vet-ttl: PASS");
    let today_fmt = today.format(DATE_FORMAT).unwrap_or_else(|_| "???".into());
    println!(
        "  today = {today_fmt}; exemption TTLs OK ({} with review-by, {} without)",
        current.len(),
        missing.len()
    );
    Ok(ExitCode::from(0))
}
