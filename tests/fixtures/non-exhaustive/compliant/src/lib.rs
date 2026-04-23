//! Fixture: a `pub enum` annotated with `#[non_exhaustive]`.
//! clippy::exhaustive_enums must accept this at `-D` level.

#[non_exhaustive]
pub enum Direction {
    Ingress,
    Egress,
}
