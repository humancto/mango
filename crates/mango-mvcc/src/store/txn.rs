//! `Txn` request / response types and the compare / op enums.
//!
//! Mirrors etcd's `etcdserverpb.TxnRequest` /
//! `etcdserverpb.TxnResponse` shape. Wire-format parity is Phase
//! 6's surface; L844 ships the in-process types only.
//!
//! **Nested `Txn` is intentionally absent** (no `RequestOp::Txn`
//! variant). `#[non_exhaustive]` on [`RequestOp`] keeps the enum
//! extensible — Phase 4 or later can add a nested variant without
//! a breaking change. See the L844 plan §6 row "Nested Txn."

use super::range::{RangeRequest, RangeResult};
use crate::Revision;

/// Comparison operator for [`Compare`].
///
/// Etcd parity: `etcdserverpb.Compare.CompareResult`. Operators
/// apply pointwise to the chosen target field.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[non_exhaustive]
pub enum CompareOp {
    /// `==`
    Equal,
    /// `!=`
    NotEqual,
    /// `>` (lexicographic for `Value`)
    Greater,
    /// `<` (lexicographic for `Value`)
    Less,
}

/// One precondition checked by a [`TxnRequest`].
///
/// Etcd parity: `etcdserverpb.Compare`. Each variant pairs a
/// target-field selector with an [`CompareOp`] and an expected
/// value.
///
/// **Compare against an absent key**: defaults are
/// `version = 0`, `create_revision = (0, 0)`, `mod_revision =
/// (0, 0)`, and `value = b""` — matching etcd's
/// `mvcc/kvstore_txn.go::checkCompare` zero-value path (review item
/// B4 of the L844 plan).
#[derive(Clone, Eq, PartialEq, Debug)]
#[non_exhaustive]
pub enum Compare {
    /// Compare against the live `version` counter (number of puts
    /// since last tombstone; `0` if absent).
    Version {
        /// Key to compare.
        key: Vec<u8>,
        /// Operator to apply.
        op: CompareOp,
        /// Expected `version`.
        target: i64,
    },
    /// Compare against the `create_revision.main` (`0` if absent).
    CreateRevision {
        /// Key to compare.
        key: Vec<u8>,
        /// Operator to apply.
        op: CompareOp,
        /// Expected `create_revision.main`.
        target: i64,
    },
    /// Compare against the `mod_revision.main` (`0` if absent).
    ModRevision {
        /// Key to compare.
        key: Vec<u8>,
        /// Operator to apply.
        op: CompareOp,
        /// Expected `mod_revision.main`.
        target: i64,
    },
    /// Compare against the raw value bytes (`b""` if absent).
    Value {
        /// Key to compare.
        key: Vec<u8>,
        /// Operator to apply (lexicographic for ordered ops).
        op: CompareOp,
        /// Expected value bytes.
        target: Vec<u8>,
    },
}

/// One operation inside a [`TxnRequest`] branch.
///
/// Etcd parity: `etcdserverpb.RequestOp` minus `request_txn` (no
/// nested transactions in L844 — see module docs). Order within
/// the branch is preserved on commit; per-op subs increment
/// monotonically per **physical write**.
#[derive(Clone, Eq, PartialEq, Debug)]
#[non_exhaustive]
pub enum RequestOp {
    /// A range read. Sees the post-commit state if it follows a
    /// `Put` / `DeleteRange` in the same branch.
    Range(RangeRequest),
    /// A single-key put.
    Put {
        /// Key to put.
        key: Vec<u8>,
        /// Value to associate.
        value: Vec<u8>,
    },
    /// Tombstone every key in `[key, end)`.
    DeleteRange {
        /// Range start.
        key: Vec<u8>,
        /// Range end (exclusive). Empty = single-key.
        end: Vec<u8>,
    },
}

/// Response paired with each [`RequestOp`] in the chosen branch.
///
/// Etcd parity: `etcdserverpb.ResponseOp` minus `response_txn`.
/// Index-aligned with the `RequestOp` slice that produced it.
#[derive(Clone, Eq, PartialEq, Debug)]
#[non_exhaustive]
pub enum ResponseOp {
    /// Result of a [`RequestOp::Range`].
    Range(RangeResult),
    /// Result of a [`RequestOp::Put`].
    Put {
        /// The previous revision of the key, or `None` if absent.
        /// L844 always returns `None`; etcd populates this only
        /// when `prev_kv = true`. Phase 6 wires `prev_kv` through.
        prev_revision: Option<Revision>,
    },
    /// Result of a [`RequestOp::DeleteRange`].
    DeleteRange {
        /// Number of keys tombstoned.
        deleted: u64,
    },
}

/// Request for `MvccStore::txn` (lands in a later commit).
///
/// Etcd parity: `etcdserverpb.TxnRequest`. Evaluation:
/// 1. Run every `compare` against the head revision.
/// 2. If all pass, execute `success`; else execute `failure`.
/// 3. Allocate at most one `main` revision (skipped if the chosen
///    branch performs zero physical writes — read-only `Txn`
///    parity with etcd's `storeTxnRead` path).
///
/// **Empty `compare` list succeeds** (etcd parity, review item M1).
#[derive(Clone, Eq, PartialEq, Debug, Default)]
#[non_exhaustive]
pub struct TxnRequest {
    /// Preconditions, all of which must pass for `success` to run.
    pub compare: Vec<Compare>,
    /// Branch executed when all `compare`s pass.
    pub success: Vec<RequestOp>,
    /// Branch executed when any `compare` fails.
    pub failure: Vec<RequestOp>,
}

/// Response from `MvccStore::txn` (lands in a later commit).
///
/// Etcd parity: `etcdserverpb.TxnResponse`. `responses` is
/// index-aligned with the executed branch (`success` if
/// `succeeded`, else `failure`).
#[derive(Clone, Eq, PartialEq, Debug, Default)]
#[non_exhaustive]
pub struct TxnResponse {
    /// `true` if all compares passed and `success` ran.
    pub succeeded: bool,
    /// One [`ResponseOp`] per [`RequestOp`] in the executed branch.
    pub responses: Vec<ResponseOp>,
    /// Highest fully-published revision after the txn. For
    /// read-only / no-op txns this equals the pre-txn current rev.
    /// L844 ships `i64` only; Phase 6 expands to a full Header.
    pub header_revision: i64,
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::{Compare, CompareOp, RequestOp, ResponseOp, TxnRequest, TxnResponse};
    use crate::store::range::RangeRequest;

    #[test]
    fn txn_request_default_is_empty_branches() {
        let req = TxnRequest::default();
        assert!(req.compare.is_empty());
        assert!(req.success.is_empty());
        assert!(req.failure.is_empty());
    }

    #[test]
    fn txn_response_default_is_failed() {
        let r = TxnResponse::default();
        assert!(!r.succeeded);
        assert!(r.responses.is_empty());
        assert_eq!(r.header_revision, 0);
    }

    #[test]
    fn compare_variants_construct() {
        let v = Compare::Version {
            key: b"k".to_vec(),
            op: CompareOp::Equal,
            target: 1,
        };
        let cr = Compare::CreateRevision {
            key: b"k".to_vec(),
            op: CompareOp::Greater,
            target: 0,
        };
        let mr = Compare::ModRevision {
            key: b"k".to_vec(),
            op: CompareOp::Less,
            target: 100,
        };
        let val = Compare::Value {
            key: b"k".to_vec(),
            op: CompareOp::NotEqual,
            target: b"v".to_vec(),
        };
        // Just confirm match coverage compiles for non_exhaustive
        // enum from inside the same crate.
        for c in [v, cr, mr, val] {
            match c {
                Compare::Version { .. }
                | Compare::CreateRevision { .. }
                | Compare::ModRevision { .. }
                | Compare::Value { .. } => {}
            }
        }
    }

    #[test]
    fn request_op_variants_construct() {
        let r = RequestOp::Range(RangeRequest::default());
        let p = RequestOp::Put {
            key: b"k".to_vec(),
            value: b"v".to_vec(),
        };
        let d = RequestOp::DeleteRange {
            key: b"a".to_vec(),
            end: b"z".to_vec(),
        };
        for op in [r, p, d] {
            match op {
                RequestOp::Range(_) | RequestOp::Put { .. } | RequestOp::DeleteRange { .. } => {}
            }
        }
    }

    #[test]
    fn response_op_variants_construct() {
        let r = ResponseOp::Range(super::super::range::RangeResult::default());
        let p = ResponseOp::Put {
            prev_revision: None,
        };
        let d = ResponseOp::DeleteRange { deleted: 0 };
        for op in [r, p, d] {
            match op {
                ResponseOp::Range(_) | ResponseOp::Put { .. } | ResponseOp::DeleteRange { .. } => {}
            }
        }
    }
}
