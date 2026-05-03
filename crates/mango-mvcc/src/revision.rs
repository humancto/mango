//! [`Revision`] — Mango's MVCC `(main, sub)` revision pair.
//!
//! Etcd's `mvcc.Revision` (`server/storage/mvcc/revision.go`) is the
//! reference shape: `main` is the Raft-apply revision, monotonically
//! non-decreasing across the cluster lifetime; `sub` is the
//! within-batch ordinal so multiple writes inside a single
//! Raft-apply get distinct revisions. Both are `i64` to match etcd's
//! wire format byte-for-byte (see `crate::encoding`).
//!
//! `(0, 0)` is the sentinel "before any revision exists" — etcd's
//! Go zero-value `Revision{}` has the same meaning. Mango starts
//! numbering at `(1, 0)`.
//!
//! Fields are private; access via [`Revision::main`] and
//! [`Revision::sub`]. Both accessors are `const fn` and zero-cost.
//! Privacy lets a future invariant (e.g. "`main >= 0`") land
//! without a breaking change. The struct is intentionally NOT
//! `#[non_exhaustive]` because adding a third field would break
//! `Copy` semantics regardless — the type's shape is part of the
//! load-bearing on-disk contract.

use core::fmt;

/// A Mango MVCC revision: `(main, sub)`.
///
/// `main` is the Raft-apply revision (one per applied entry); `sub`
/// is the within-entry ordinal (one per op within the entry's batch).
/// Lex-ordered on `(main, sub)`.
///
/// Construct via [`Revision::new`] (infallible, `const`). Project
/// the components with [`Revision::main`] / [`Revision::sub`].
///
/// # Sentinel
///
/// [`Revision::default`] returns `Revision::new(0, 0)`. This is the
/// "before any revision exists" sentinel — Mango writers MUST issue
/// revisions starting at `(1, 0)`. APIs that need to express "no
/// revision yet" SHOULD prefer `Option<Revision>` over the sentinel
/// where ergonomic. Today no consumer does; L839's `KeyHistory` is
/// the first.
///
/// # Display
///
/// `Revision::new(5, 2)` formats as `5_2`, matching etcd's log
/// format (`server/storage/mvcc/revision.go::Revision.String`). Lets
/// `grep` correlate Mango logs with etcd logs in mixed clusters.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Ord, PartialOrd, Default)]
pub struct Revision {
    main: i64,
    sub: i64,
}

impl Revision {
    /// Construct a `Revision`. No validation — `main` and `sub` may
    /// each independently be `0`, negative, or `i64::MAX`.
    ///
    /// Mango's storage layer rejects negative revisions on decode
    /// (see `crate::encoding::KeyDecodeError::NegativeRevision`),
    /// so any `Revision` that round-trips through the `key` bucket
    /// is guaranteed non-negative. The constructor is infallible to
    /// keep `const`-friendliness and match etcd's struct shape.
    #[must_use]
    pub const fn new(main: i64, sub: i64) -> Self {
        Self { main, sub }
    }

    /// The Raft-apply component of this revision.
    #[must_use]
    pub const fn main(self) -> i64 {
        self.main
    }

    /// The within-batch ordinal of this revision.
    #[must_use]
    pub const fn sub(self) -> i64 {
        self.sub
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}_{}", self.main, self.sub)
    }
}

/// A `proptest` `Strategy` adapter for [`Revision`] that produces
/// only the values the decoder will accept (`main >= 0 && sub >= 0`).
///
/// Available under the `proptest` feature so downstream property
/// tests (notably L851's MVCC model proptest) can reuse it without
/// re-deriving the strategy or accidentally generating invalid
/// revisions.
#[cfg(feature = "proptest")]
pub fn arb_revision() -> impl proptest::strategy::Strategy<Value = Revision> {
    use proptest::strategy::Strategy;
    (0_i64..=i64::MAX, 0_i64..=i64::MAX).prop_map(|(main, sub)| Revision::new(main, sub))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::*;

    #[test]
    fn default_is_zero_zero() {
        assert_eq!(Revision::default(), Revision::new(0, 0));
    }

    #[test]
    fn accessors_return_constructor_args() {
        let r = Revision::new(42, 7);
        assert_eq!(r.main(), 42);
        assert_eq!(r.sub(), 7);
    }

    #[test]
    fn ord_is_lex_on_main_then_sub() {
        let table = [
            Revision::new(0, 0),
            Revision::new(0, 1),
            Revision::new(1, 0),
            Revision::new(1, 1),
            Revision::new(i64::MAX, 0),
        ];
        for (lo, hi) in table.iter().zip(table.iter().skip(1)) {
            assert!(lo < hi, "{lo:?} < {hi:?}");
        }
    }

    #[test]
    fn equal_main_orders_by_sub() {
        assert!(Revision::new(5, 0) < Revision::new(5, 1));
        assert!(Revision::new(5, 1) > Revision::new(5, 0));
        assert_eq!(Revision::new(5, 7), Revision::new(5, 7));
    }

    #[test]
    fn display_format_matches_etcd() {
        assert_eq!(Revision::new(5, 2).to_string(), "5_2");
        assert_eq!(Revision::new(0, 0).to_string(), "0_0");
        // Negative input survives `Display` even though the decoder
        // rejects on-disk negatives — `Display` is for log lines,
        // and `Revision::new` is infallible. Etcd's `Revision.String`
        // does the same with Go's `%d_%d`.
        assert_eq!(Revision::new(-1, 0).to_string(), "-1_0");
    }

    #[test]
    fn copy_clone_eq_hash_derive_ok() {
        let r = Revision::new(1, 2);
        let r2 = r;
        assert_eq!(r, r2);

        let mut s = std::collections::HashSet::new();
        s.insert(r);
        assert!(s.contains(&r2));
    }
}
