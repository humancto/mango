//! Fixture: a `pub enum` with a per-enum escape: a `// reason:`
//! line-comment followed by `#[allow(clippy::exhaustive_enums)]`.
//! This is the MSRV-1.80-compatible shape of the escape hatch
//! documented in `docs/api-stability.md`. clippy must accept it at
//! `-D clippy::exhaustive_enums`.

// reason: exhaustive-by-contract — closed set by design, adding a variant is a major break.
#[allow(clippy::exhaustive_enums)]
pub enum Parity {
    Even,
    Odd,
}
