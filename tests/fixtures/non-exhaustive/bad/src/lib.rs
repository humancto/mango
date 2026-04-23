//! Fixture: a bare `pub enum` without `#[non_exhaustive]` and
//! without an escape. clippy::exhaustive_enums must reject this at
//! `-D` level — that rejection is what the self-test asserts on.

pub enum Bad {
    A,
    B,
}
