//! Bench-harness `run` binary.
//!
//! Drives a workload against one storage engine and emits a JSON
//! result file. Filled in by a follow-up commit on this branch;
//! this stub exists so the crate compiles in the scaffold commit.

use std::io::Write as _;

fn main() -> std::process::ExitCode {
    // Direct write to stderr handle: `eprintln!` macro is denied by
    // workspace clippy::print_stderr; the io::Write API is the
    // blessed bypass for binaries that legitimately need a CLI message.
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "mango-bench-storage run: scaffold stub - see ROADMAP:829"
    );
    std::process::ExitCode::from(2)
}
