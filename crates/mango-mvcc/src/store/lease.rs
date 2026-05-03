//! [`LeaseId`] — non-zero `i64` newtype around an etcd-shape lease id.
//!
//! Phase 4 owns lease semantics; this newtype is shipped early in
//! L844 so the [`super::range::KeyValue`] field doesn't change shape
//! later (review item B5 of the L844 plan). The inner field is
//! private so a future invariant beyond non-zeroness can land
//! without an API break.
//!
//! Wire format: `i64` (gRPC `int64`). `None` round-trips as `0`,
//! which is the etcd parity for "no lease."

use core::num::NonZeroI64;

/// Newtype around a non-zero [`i64`] lease id.
///
/// Etcd's `lease.LeaseID` (`server/lease/lessor.go`) is a 63-bit
/// non-zero `int64`. Zero is reserved for "no lease" and is encoded
/// over the wire as the absence of a lease association — Mango
/// represents that as `Option<LeaseId>::None`.
///
/// Construction: [`LeaseId::new`]. Projection: [`LeaseId::get`]. The
/// inner field is private so a future invariant (e.g. "high bit
/// reserved for cluster id") can be added without a breaking change.
///
/// Phase 4 (Lease) owns the semantics of how lease ids are allocated
/// and how attached keys are revoked. L844 only ships the type so
/// the [`super::range::KeyValue::lease`] field is forward-compatible.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct LeaseId(NonZeroI64);

impl LeaseId {
    /// Wrap a raw `i64`. Returns `None` for zero (the "no lease"
    /// sentinel), `Some(LeaseId)` otherwise. `i64::MIN` is allowed
    /// because [`NonZeroI64`] permits it; lease allocators are free
    /// to restrict the range further.
    #[must_use]
    pub const fn new(raw: i64) -> Option<Self> {
        match NonZeroI64::new(raw) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Wire-format integer (gRPC `int64`). Always non-zero.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0.get()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::LeaseId;

    #[test]
    fn new_zero_is_none() {
        assert!(LeaseId::new(0).is_none());
    }

    #[test]
    fn new_nonzero_roundtrips() {
        for raw in [1_i64, -1, i64::MAX, i64::MIN, 42, -42] {
            let lid = LeaseId::new(raw).expect("non-zero");
            assert_eq!(lid.get(), raw);
        }
    }

    #[test]
    fn equality_and_hash_match_inner() {
        let a = LeaseId::new(7).expect("nonzero");
        let b = LeaseId::new(7).expect("nonzero");
        let c = LeaseId::new(8).expect("nonzero");
        assert_eq!(a, b);
        assert_ne!(a, c);
        // Hash is structurally derived from NonZeroI64; sanity-check
        // by storing in a HashSet.
        let mut set = std::collections::HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }

    #[test]
    fn copy_and_clone() {
        let a = LeaseId::new(5).expect("nonzero");
        let b = a;
        let c = a;
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn debug_does_not_panic() {
        let a = LeaseId::new(123).expect("nonzero");
        let s = format!("{a:?}");
        assert!(s.contains("123"));
    }
}
