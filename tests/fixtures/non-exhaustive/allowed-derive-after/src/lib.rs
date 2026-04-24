//! Fixture: `#[derive(Debug)]` followed by `#[non_exhaustive]` on the
//! same enum. clippy::exhaustive_enums must accept (attribute order is
//! semantically free); the backstop's awk cluster scan must also accept
//! — the pre-cluster prev1/prev2 state machine false-rejected this
//! shape because `#[non_exhaustive]` landed on prev2 behind `#[derive]`.

#[derive(Debug)]
#[non_exhaustive]
pub enum Mode {
    Read,
    Write,
}
