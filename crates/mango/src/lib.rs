//! mango — a distributed reliable key-value store written in Rust.
//!
//! This crate is currently a placeholder. Real functionality lands per the
//! phases described in `ROADMAP.md` at the workspace root.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_cargo_manifest() {
        assert_eq!(VERSION, "0.1.0");
    }
}
