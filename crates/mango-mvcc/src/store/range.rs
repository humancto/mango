//! `Range` request / result types and [`KeyValue`].
//!
//! Mirrors etcd's `etcdserverpb.RangeRequest` /
//! `etcdserverpb.RangeResponse` shape. Wire-format parity (gRPC
//! `etcdserverpb`) is Phase 6's surface; L844 ships the in-process
//! types only. Header is `i64` for now — the full
//! `Header { revision, raft_term, cluster_id, member_id }` lands
//! when gRPC ships (review item M3 of the L844 plan).

use bytes::Bytes;

use super::lease::LeaseId;
use crate::Revision;

/// A single key-value entry returned by a range read.
///
/// Etcd parity: `mvccpb.KeyValue`
/// (`api/mvccpb/kv.proto`). Field types match Mango's
/// in-process shape (`Bytes` for cheap clones, [`Revision`] instead
/// of two raw `i64`s).
///
/// `lease` is `None` until Phase 4 (Lease) wires it up. The gRPC
/// adapter wire-encodes `None → 0`, matching etcd's "no lease"
/// sentinel (review item B5 of the L844 plan).
#[derive(Clone, Eq, PartialEq, Debug)]
#[non_exhaustive]
pub struct KeyValue {
    /// Raw key bytes.
    pub key: Bytes,
    /// Revision at which this key was first created (live span
    /// start). Reset on a tombstone-then-put cycle.
    pub create_revision: Revision,
    /// Revision at which this key was last modified. Equals
    /// `create_revision` for the first put of a key.
    pub mod_revision: Revision,
    /// Number of times this key has been put within the current
    /// live span. Resets to `0` after a tombstone.
    pub version: i64,
    /// Raw value bytes.
    pub value: Bytes,
    /// Attached lease id, or `None` for "no lease."
    /// Always `None` in L844 (set by Phase 4).
    pub lease: Option<LeaseId>,
}

/// Request for `MvccStore::range` (lands in a later commit).
///
/// Etcd parity: `etcdserverpb.RangeRequest`. The half-open range
/// `[key, end)` matches etcd; an empty `end` denotes a single-key
/// point lookup. `revision = None` reads at the current revision;
/// `Some(r)` reads at exactly `r` (returns `Compacted` if `r` is
/// below the compacted floor, `FutureRevision` if above current).
#[derive(Clone, Eq, PartialEq, Debug, Default)]
#[non_exhaustive]
pub struct RangeRequest {
    /// Range start. When `end` is empty, this is the sole key
    /// looked up.
    pub key: Vec<u8>,
    /// Range end (exclusive). Empty means point-lookup on `key`.
    pub end: Vec<u8>,
    /// Revision to read at. `None` = current.
    pub revision: Option<i64>,
    /// Maximum number of [`KeyValue`] entries to return. `None` =
    /// unlimited. Ignored when `count_only` is `true`.
    pub limit: Option<usize>,
    /// When `true`, the result's [`KeyValue::value`] fields are
    /// empty. Skips the on-disk value fetch but still returns
    /// every key in range with revision metadata.
    pub keys_only: bool,
    /// When `true`, returns no [`KeyValue`]s at all and reports
    /// only the total match count via [`RangeResult::count`].
    /// `limit` is ignored. Etcd parity: `RangeRequest.count_only`.
    pub count_only: bool,
}

/// Response from `MvccStore::range` (lands in a later commit).
///
/// Etcd parity: `etcdserverpb.RangeResponse`. `count` is the total
/// number of matches **ignoring `limit`**, while `kvs.len()` is
/// capped at `limit`. For `limit = 10` over 100 matches:
/// `kvs.len() == 10`, `more == true`, `count == 100` (review item
/// M4 of the L844 plan).
#[derive(Clone, Eq, PartialEq, Debug, Default)]
#[non_exhaustive]
pub struct RangeResult {
    /// Returned entries (empty when `count_only` was requested).
    pub kvs: Vec<KeyValue>,
    /// `true` if `limit` was hit (more matches existed).
    pub more: bool,
    /// Total matches in `[key, end)` ignoring `limit`.
    pub count: u64,
    /// Highest fully-published revision at the time of the read.
    /// In L844 this is a bare `i64`; Phase 6 expands to a full
    /// `Header { revision, raft_term, cluster_id, member_id }`.
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

    use super::{KeyValue, RangeRequest, RangeResult};
    use crate::Revision;
    use bytes::Bytes;

    #[test]
    fn range_request_default_is_point_lookup_at_current() {
        let req = RangeRequest::default();
        assert!(req.key.is_empty());
        assert!(req.end.is_empty());
        assert_eq!(req.revision, None);
        assert_eq!(req.limit, None);
        assert!(!req.keys_only);
        assert!(!req.count_only);
    }

    #[test]
    fn range_result_default_is_empty() {
        let r = RangeResult::default();
        assert!(r.kvs.is_empty());
        assert!(!r.more);
        assert_eq!(r.count, 0);
        assert_eq!(r.header_revision, 0);
    }

    #[test]
    fn key_value_is_cloneable() {
        let kv = KeyValue {
            key: Bytes::from_static(b"k"),
            create_revision: Revision::new(1, 0),
            mod_revision: Revision::new(2, 0),
            version: 2,
            value: Bytes::from_static(b"v"),
            lease: None,
        };
        let kv2 = kv.clone();
        assert_eq!(kv, kv2);
    }
}
