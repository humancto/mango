//! Bench-harness `gate` binary.
//!
//! Reads two result JSONs (mango + bbolt) and applies the win/loss
//! verdict per S1 / S2 / S3 / N9 / N10. Filled in by a follow-up
//! commit on this branch; this stub exists so the crate compiles
//! in the scaffold commit.

use std::io::Write as _;

fn main() -> std::process::ExitCode {
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "mango-bench-storage gate: scaffold stub - see ROADMAP:829"
    );
    std::process::ExitCode::from(2)
}
