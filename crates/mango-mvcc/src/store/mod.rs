//! User-facing MVCC store (L844).
//!
//! This commit lands only the public types — `MvccStore` itself
//! and the writer / range / txn / compact implementations come in
//! subsequent commits per the L844 plan §8 commit sequence.

pub mod lease;
pub mod range;
pub mod txn;

pub use lease::LeaseId;
pub use range::{KeyValue, RangeRequest, RangeResult};
pub use txn::{Compare, CompareOp, RequestOp, ResponseOp, TxnRequest, TxnResponse};
