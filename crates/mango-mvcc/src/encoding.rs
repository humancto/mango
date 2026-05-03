//! On-disk key encoding for the `key` bucket.
//!
//! Byte-for-byte equal to etcd's `BucketKeyToBytes`
//! (`server/storage/mvcc/revision.go`):
//!
//! ```text
//! Put       (17 bytes): [main BE u64 (8)] [ '_' 0x5f ] [sub BE u64 (8)]
//! Tombstone (18 bytes): [main BE u64 (8)] [ '_' 0x5f ] [sub BE u64 (8)] [ 't' 0x74 ]
//! ```
//!
//! Put has **no marker byte** — it is distinguished from Tombstone
//! by length (17 vs 18), not by a tag at a fixed position. The `_`
//! (`0x5f`) separator at offset 8 is mandatory.
//!
//! Encoders accept the full `i64` range (matching etcd's struct
//! shape). Decoders REJECT negative `main` or `sub` values with
//! [`KeyDecodeError::NegativeRevision`] — etcd panics in the
//! equivalent path; Mango returns a typed error. Anything that
//! round-trips through the storage backend is therefore guaranteed
//! non-negative.
//!
//! Allocation-free: `encode_key` returns a stack-allocated
//! [`EncodedKey`] (max 18 bytes). Hot paths can call `as_bytes()`
//! and pass the slice directly to `WriteBatch::put`.
//!
//! See the rustdoc on each item for the etcd source citation.

use crate::revision::Revision;

/// Length in bytes of a `Put`-kind encoded key.
pub const ENCODED_PUT_LEN: usize = 17;

/// Length in bytes of a `Tombstone`-kind encoded key.
pub const ENCODED_TOMBSTONE_LEN: usize = 18;

/// The mandatory separator byte at offset 8 (`_`, ASCII 0x5f).
const SEPARATOR: u8 = b'_';

/// The tombstone marker byte at offset 17 (`t`, ASCII 0x74).
const MARK_TOMBSTONE: u8 = b't';

/// What kind of revision is being encoded.
///
/// Etcd distinguishes `Put` from `Tombstone` by **length** (a
/// trailing `'t'` byte appended for tombstones), not by a marker at
/// a fixed offset. We mirror.
///
/// Not `#[repr(u8)]` — the on-disk encoding is a function of this
/// enum, not its discriminant. In particular `Put` has no marker
/// byte at all, so a `Put = 0x00` discriminant would be a lie.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
#[non_exhaustive]
pub enum KeyKind {
    /// A live revision — the `key` bucket value at this revision is
    /// the user's `KeyValue` payload.
    Put,

    /// A deletion marker — the `key` bucket carries a tombstone
    /// entry for this revision.
    Tombstone,
}

/// A fully-formed on-disk key for the MVCC `key` bucket.
///
/// `as_bytes()` returns 17 bytes for `Put`, 18 for `Tombstone`. The
/// inner buffer is always 18 bytes wide; the trailing byte is unused
/// (and zero) for `Put`. Stack-allocated; clones are byte copies.
#[derive(Clone, Eq, PartialEq, Hash, Debug)]
pub struct EncodedKey {
    buf: [u8; ENCODED_TOMBSTONE_LEN],
    /// Always [`ENCODED_PUT_LEN`] (17) or [`ENCODED_TOMBSTONE_LEN`] (18).
    len: u8,
}

impl EncodedKey {
    /// The encoded bytes — 17 (`Put`) or 18 (`Tombstone`).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        let len = usize::from(self.len);
        match self.buf.get(..len) {
            Some(slice) => slice,
            // SAFETY-by-construction: `self.len` is set only by
            // `encode_key`, which assigns either `ENCODED_PUT_LEN`
            // or `ENCODED_TOMBSTONE_LEN`, both `<= self.buf.len()`.
            // The `None` arm cannot trigger; we return an empty
            // slice rather than panic to keep `clippy::indexing_slicing`
            // happy without `unwrap`.
            None => &[],
        }
    }
}

/// Errors returned by [`decode_key`].
///
/// `#[non_exhaustive]` per workspace policy — new failure modes can
/// be added without breaking downstream `match`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KeyDecodeError {
    /// Encoded key is not 17 or 18 bytes.
    #[error("encoded key is {got} bytes, expected 17 (Put) or 18 (Tombstone)")]
    BadLength {
        /// The actual length received.
        got: usize,
    },

    /// Encoded key has the wrong byte at offset 8 (separator).
    #[error("encoded key separator at offset 8 is {got:#04x}, expected 0x5f ('_')")]
    BadSeparator {
        /// The actual byte at offset 8.
        got: u8,
    },

    /// Encoded key is 18 bytes but the trailing marker byte is not `'t'`.
    #[error("encoded key marker at offset 17 is {got:#04x}, expected 0x74 ('t')")]
    UnknownMark {
        /// The actual marker byte.
        got: u8,
    },

    /// Encoded key parses to a `Revision` with a negative `main` or
    /// `sub`. Etcd panics here; Mango rejects with a typed error.
    /// Indicates either bucket corruption or an attacker-supplied
    /// blob.
    #[error("encoded key has negative revision {main}_{sub}; on-disk negatives are rejected")]
    NegativeRevision {
        /// The decoded `main` (negative).
        main: i64,
        /// The decoded `sub` (may also be negative).
        sub: i64,
    },
}

/// Encode `(rev, kind)` into the on-disk byte format. Allocation-free.
///
/// Accepts negative `i64` values for `main` and `sub` (matches etcd's
/// struct shape). The decoder rejects them; round-tripping a
/// negative-revision encoding through [`decode_key`] returns
/// [`KeyDecodeError::NegativeRevision`].
#[must_use]
pub fn encode_key(rev: Revision, kind: KeyKind) -> EncodedKey {
    let mut buf = [0u8; ENCODED_TOMBSTONE_LEN];
    let main_be = rev.main().to_be_bytes();
    let sub_be = rev.sub().to_be_bytes();

    // Layout: [main 0..8] [_ at 8] [sub 9..17] [t? at 17]
    let (main_slot, rest) = buf.split_at_mut(8);
    main_slot.copy_from_slice(&main_be);

    let (sep_slot, rest) = rest.split_at_mut(1);
    sep_slot.copy_from_slice(&[SEPARATOR]);

    let (sub_slot, mark_slot) = rest.split_at_mut(8);
    sub_slot.copy_from_slice(&sub_be);

    let len = match kind {
        KeyKind::Put => {
            // mark_slot's single byte stays zero — it's outside
            // `as_bytes()`'s view (len = 17).
            ENCODED_PUT_LEN
        }
        KeyKind::Tombstone => {
            mark_slot.copy_from_slice(&[MARK_TOMBSTONE]);
            ENCODED_TOMBSTONE_LEN
        }
    };

    EncodedKey {
        buf,
        // Both constants are `<= 18 = u8::MAX`-clamped; no truncation risk.
        // `arithmetic_side_effects` only fires on `+ - * / %`, not casts.
        #[allow(clippy::cast_possible_truncation)]
        len: len as u8,
    }
}

/// Decode an on-disk encoded key back into `(Revision, KeyKind)`.
///
/// # Errors
///
/// - [`KeyDecodeError::BadLength`] — `bytes` is not 17 or 18 long.
/// - [`KeyDecodeError::BadSeparator`] — `bytes[8]` is not `0x5f`.
/// - [`KeyDecodeError::UnknownMark`] — 18-byte input where
///   `bytes[17]` is not `0x74`.
/// - [`KeyDecodeError::NegativeRevision`] — decoded `main` or `sub`
///   is negative.
pub fn decode_key(bytes: &[u8]) -> Result<(Revision, KeyKind), KeyDecodeError> {
    let kind = match bytes.len() {
        ENCODED_PUT_LEN => KeyKind::Put,
        ENCODED_TOMBSTONE_LEN => KeyKind::Tombstone,
        got => return Err(KeyDecodeError::BadLength { got }),
    };

    // Length-checked above; `get` keeps clippy::indexing_slicing happy.
    let main_bytes: [u8; 8] = bytes
        .get(..8)
        .and_then(|s| s.try_into().ok())
        .ok_or(KeyDecodeError::BadLength { got: bytes.len() })?;
    let sep = bytes
        .get(8)
        .copied()
        .ok_or(KeyDecodeError::BadLength { got: bytes.len() })?;
    let sub_bytes: [u8; 8] = bytes
        .get(9..17)
        .and_then(|s| s.try_into().ok())
        .ok_or(KeyDecodeError::BadLength { got: bytes.len() })?;

    if sep != SEPARATOR {
        return Err(KeyDecodeError::BadSeparator { got: sep });
    }

    if matches!(kind, KeyKind::Tombstone) {
        let mark = bytes
            .get(17)
            .copied()
            .ok_or(KeyDecodeError::BadLength { got: bytes.len() })?;
        if mark != MARK_TOMBSTONE {
            return Err(KeyDecodeError::UnknownMark { got: mark });
        }
    }

    let main = i64::from_be_bytes(main_bytes);
    let sub = i64::from_be_bytes(sub_bytes);
    if main < 0 || sub < 0 {
        return Err(KeyDecodeError::NegativeRevision { main, sub });
    }

    Ok((Revision::new(main, sub), kind))
}

/// A `proptest` `Strategy` adapter for [`KeyKind`].
///
/// Available under the `proptest` feature for the same reason as
/// [`crate::revision::arb_revision`].
#[cfg(feature = "proptest")]
pub fn arb_key_kind() -> impl proptest::strategy::Strategy<Value = KeyKind> {
    proptest::prelude::prop_oneof![
        proptest::prelude::Just(KeyKind::Put),
        proptest::prelude::Just(KeyKind::Tombstone),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use super::*;
    use proptest::prelude::*;

    fn roundtrip(rev: Revision, kind: KeyKind) {
        let enc = encode_key(rev, kind);
        let (rev2, kind2) = decode_key(enc.as_bytes()).expect("decode");
        assert_eq!(rev, rev2);
        assert_eq!(kind, kind2);
    }

    #[test]
    fn encode_decode_roundtrip_zero() {
        roundtrip(Revision::new(0, 0), KeyKind::Put);
    }

    #[test]
    fn encode_decode_roundtrip_max() {
        roundtrip(Revision::new(i64::MAX, i64::MAX), KeyKind::Put);
    }

    #[test]
    fn encode_decode_roundtrip_tombstone() {
        roundtrip(Revision::new(1, 0), KeyKind::Tombstone);
    }

    #[test]
    fn encoded_put_length_is_17() {
        let enc = encode_key(Revision::new(1, 2), KeyKind::Put);
        assert_eq!(enc.as_bytes().len(), ENCODED_PUT_LEN);
    }

    #[test]
    fn encoded_tombstone_length_is_18() {
        let enc = encode_key(Revision::new(1, 2), KeyKind::Tombstone);
        assert_eq!(enc.as_bytes().len(), ENCODED_TOMBSTONE_LEN);
    }

    #[test]
    fn encoded_separator_at_offset_8() {
        let put = encode_key(Revision::new(42, 7), KeyKind::Put);
        let tomb = encode_key(Revision::new(42, 7), KeyKind::Tombstone);
        assert_eq!(put.as_bytes().get(8).copied(), Some(SEPARATOR));
        assert_eq!(tomb.as_bytes().get(8).copied(), Some(SEPARATOR));
    }

    #[test]
    fn tombstone_distinguishable_by_length_only() {
        let put = encode_key(Revision::new(7, 3), KeyKind::Put);
        let tomb = encode_key(Revision::new(7, 3), KeyKind::Tombstone);
        assert_eq!(put.as_bytes().len(), 17);
        assert_eq!(tomb.as_bytes().len(), 18);
        // First 17 bytes are byte-equal — distinction is purely the
        // 18th byte's presence (and value 't').
        assert_eq!(put.as_bytes(), tomb.as_bytes().get(..17).expect("17B"));
        assert_eq!(tomb.as_bytes().get(17).copied(), Some(MARK_TOMBSTONE));
    }

    #[test]
    fn decode_rejects_short_input() {
        let err = decode_key(&[0u8; 16]).expect_err("must reject");
        assert!(matches!(err, KeyDecodeError::BadLength { got: 16 }));
    }

    #[test]
    fn decode_rejects_long_input() {
        let err = decode_key(&[0u8; 19]).expect_err("must reject");
        assert!(matches!(err, KeyDecodeError::BadLength { got: 19 }));
    }

    #[test]
    fn decode_rejects_zero_length() {
        let err = decode_key(&[]).expect_err("must reject");
        assert!(matches!(err, KeyDecodeError::BadLength { got: 0 }));
    }

    #[test]
    fn decode_rejects_bad_separator() {
        // 17 bytes, all zero except offset 8 = '.' (0x2e)
        let mut bytes = [0u8; 17];
        if let Some(slot) = bytes.get_mut(8) {
            *slot = b'.';
        }
        let err = decode_key(&bytes).expect_err("must reject");
        assert!(
            matches!(err, KeyDecodeError::BadSeparator { got: 0x2e }),
            "got {err:?}"
        );
    }

    #[test]
    fn decode_rejects_unknown_mark() {
        // Build a valid 18-byte tombstone, then corrupt the marker.
        let enc = encode_key(Revision::new(1, 2), KeyKind::Tombstone);
        let mut bytes = enc.as_bytes().to_vec();
        if let Some(slot) = bytes.get_mut(17) {
            *slot = 0xFF;
        }
        let err = decode_key(&bytes).expect_err("must reject");
        assert!(
            matches!(err, KeyDecodeError::UnknownMark { got: 0xFF }),
            "got {err:?}"
        );
    }

    #[test]
    fn decode_rejects_negative_main() {
        let enc = encode_key(Revision::new(-1, 0), KeyKind::Put);
        let err = decode_key(enc.as_bytes()).expect_err("must reject");
        assert!(
            matches!(err, KeyDecodeError::NegativeRevision { main: -1, sub: 0 }),
            "got {err:?}"
        );
    }

    #[test]
    fn decode_rejects_negative_sub() {
        let enc = encode_key(Revision::new(0, -1), KeyKind::Put);
        let err = decode_key(enc.as_bytes()).expect_err("must reject");
        assert!(
            matches!(err, KeyDecodeError::NegativeRevision { main: 0, sub: -1 }),
            "got {err:?}"
        );
    }

    #[test]
    fn decode_rejects_negative_tombstone() {
        let enc = encode_key(Revision::new(-5, -2), KeyKind::Tombstone);
        let err = decode_key(enc.as_bytes()).expect_err("must reject");
        assert!(
            matches!(err, KeyDecodeError::NegativeRevision { main: -5, sub: -2 }),
            "got {err:?}"
        );
    }

    /// Cross-check the encoder against an actual etcd source.
    ///
    /// Reference: etcd-io/etcd@release-3.5
    /// (commit `c95eaa0ad84ce32d4a2c84a7d6a18b09bce0d4d3`),
    /// `server/storage/mvcc/revision.go`:
    ///
    /// ```go
    /// const (
    ///     revBytesLen        = 8 + 1 + 8
    ///     markedRevBytesLen  = revBytesLen + 1
    ///     markBytePosition   = markedRevBytesLen - 1
    ///     markTombstone byte = 't'
    /// )
    /// // BucketKeyToBytes:
    /// //   binary.BigEndian.PutUint64(bytes, uint64(rev.Main))
    /// //   bytes[8] = '_'
    /// //   binary.BigEndian.PutUint64(bytes[9:], uint64(rev.Sub))
    /// //   if isTombstone { bytes[markBytePosition] = markTombstone }
    /// ```
    ///
    /// Running that encoder on `Revision{Main: 1, Sub: 2}` Put yields
    /// the 17-byte sequence below (verified by hand against the Go
    /// source — `binary.BigEndian.PutUint64(b, 1)` writes
    /// `[0,0,0,0,0,0,0,1]`).
    #[test]
    fn golden_put_vs_etcd() {
        let enc = encode_key(Revision::new(1, 2), KeyKind::Put);
        let expected: [u8; ENCODED_PUT_LEN] = [
            // main = 1, BE
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, //
            // separator
            b'_', //
            // sub = 2, BE
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02,
        ];
        assert_eq!(enc.as_bytes(), &expected[..]);
    }

    /// Same as `golden_put_vs_etcd` but with the trailing tombstone
    /// marker. Etcd's `BucketKeyToBytes(... isTombstone: true)`
    /// produces an 18-byte sequence: the 17-byte Put plus `'t'`
    /// (0x74) at position 17.
    #[test]
    fn golden_tombstone_vs_etcd() {
        let enc = encode_key(Revision::new(1, 2), KeyKind::Tombstone);
        let expected: [u8; ENCODED_TOMBSTONE_LEN] = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, //
            b'_', //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, //
            b't', //
        ];
        assert_eq!(enc.as_bytes(), &expected[..]);
    }

    proptest! {
        /// Lex-byte-order on `encode_key(_, Put)` matches `Ord` on
        /// `Revision` for the entire range the decoder will accept
        /// (non-negative `main` and `sub`). This is the load-bearing
        /// property for "scan the `key` bucket in revision order".
        #[test]
        fn lex_byte_order_matches_revision_order_for_non_negative(
            (m1, s1) in (0_i64..=i64::MAX, 0_i64..=i64::MAX),
            (m2, s2) in (0_i64..=i64::MAX, 0_i64..=i64::MAX),
        ) {
            let r1 = Revision::new(m1, s1);
            let r2 = Revision::new(m2, s2);
            let b1 = encode_key(r1, KeyKind::Put);
            let b2 = encode_key(r2, KeyKind::Put);
            prop_assert_eq!(r1.cmp(&r2), b1.as_bytes().cmp(b2.as_bytes()));
        }

        /// Round-trip on the decoder-legal range. ~10k cases. A
        /// failure here means either the encoder or decoder drifted
        /// from the spec.
        #[test]
        fn proptest_roundtrip_decoder_legal(
            (main, sub) in (0_i64..=i64::MAX, 0_i64..=i64::MAX),
            kind in prop_oneof![Just(KeyKind::Put), Just(KeyKind::Tombstone)],
        ) {
            let rev = Revision::new(main, sub);
            let enc = encode_key(rev, kind);
            let (rev2, kind2) = decode_key(enc.as_bytes()).expect("decode");
            prop_assert_eq!(rev, rev2);
            prop_assert_eq!(kind, kind2);
        }

        /// Negative-revision rejection. Decoder MUST refuse anything
        /// where the parsed `main` or `sub` is negative.
        #[test]
        fn proptest_decoder_rejects_negatives(
            main in proptest::prop_oneof![i64::MIN..0_i64, 0_i64..=i64::MAX],
            sub in proptest::prop_oneof![i64::MIN..0_i64, 0_i64..=i64::MAX],
            kind in prop_oneof![Just(KeyKind::Put), Just(KeyKind::Tombstone)],
        ) {
            prop_assume!(main < 0 || sub < 0);
            let enc = encode_key(Revision::new(main, sub), kind);
            let err = decode_key(enc.as_bytes()).expect_err("must reject");
            prop_assert!(
                matches!(err, KeyDecodeError::NegativeRevision { main: m, sub: s } if m == main && s == sub),
                "wrong error for ({main}, {sub}): {err:?}"
            );
        }
    }
}
