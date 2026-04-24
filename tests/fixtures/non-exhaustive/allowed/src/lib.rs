//! Fixture: a `pub enum` with a per-enum escape: a `// reason:`
//! line-comment followed by `#[allow(clippy::exhaustive_enums)]`.
//! This is the legacy MSRV-≤-1.80 shape of the escape hatch, kept
//! as a clippy-acceptance oracle — clippy still accepts this form,
//! and a regression in that acceptance would surface here even
//! though workspace policy has moved to the inline
//! `#[allow(lint, reason = "...")]` form at MSRV 1.89 (see ADR 0003
//! and `docs/api-stability.md`). The tripwire in
//! `scripts/non-exhaustive-check.sh:321` forbids this form in
//! publishable crates; this fixture is deliberately outside that
//! scope.

// reason: exhaustive-by-contract — closed set by design, adding a variant is a major break.
#[allow(clippy::exhaustive_enums)]
pub enum Parity {
    Even,
    Odd,
}
