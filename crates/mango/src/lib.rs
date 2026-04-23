//! mango — a distributed reliable key-value store written in Rust.
//!
//! This crate is currently a placeholder. Real functionality lands per the
//! phases described in `ROADMAP.md` at the workspace root.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::unnecessary_literal_unwrap,
        clippy::arithmetic_side_effects
    )]

    use super::*;

    #[test]
    fn version_matches_cargo_manifest() {
        assert_eq!(VERSION, "0.1.0");
    }

    // Watchdog regression smoke. Sleeps past the 30s unit-class budget
    // declared in `.config/nextest.toml` so `scripts/test-watchdog.sh`
    // can assert nextest actually terminates it. `#[ignore]` keeps it
    // out of the default CI pass; the script runs it explicitly with
    // `--run-ignored only` plus a test-name filter. If anyone later
    // removes `terminate-after` from `nextest.toml`, the script flips
    // red. Do NOT remove without understanding why it is here.
    #[test]
    #[ignore = "watchdog smoke; run via scripts/test-watchdog.sh only"]
    fn watchdog_kill_smoke() {
        std::thread::sleep(std::time::Duration::from_secs(90));
    }
}
