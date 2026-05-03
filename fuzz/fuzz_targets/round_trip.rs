//! L853: encode_key/decode_key round-trip property.
//!
//! Fuzz target: typed `Arbitrary` input -> encode -> decode -> equal.
//! The `decode_arbitrary` target covers the inverse direction
//! (raw bytes -> decode). Together they pin the canonicalization
//! contract from both sides.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use mango_mvcc::revision::Revision;
use mango_mvcc::{decode_key, encode_key, KeyKind};

#[derive(Debug, Clone, Arbitrary)]
struct Input {
    /// Constrained to the non-negative half-range — `decode_key`
    /// rejects negatives (`KeyDecodeError::NegativeRevision`) and
    /// the round-trip would otherwise spuriously fail.
    ///
    /// `u32` (not `u64`-half) because:
    /// 1. The encoding is byte-shuffled big-endian; the round-trip
    ///    property holds at every bit position. Width is irrelevant
    ///    for what this target proves.
    /// 2. Smaller inputs let libfuzzer's mutator find any
    ///    round-trip violation faster.
    /// 3. The full `0..=i64::MAX` typed range is already covered
    ///    by the proptest at
    ///    `crates/mango-mvcc/src/encoding.rs#round_trip_proptest`.
    /// The negative-rejection invariant is covered by the proptest
    /// at `crates/mango-mvcc/src/encoding.rs#decode_rejects_negative_proptest`.
    main: u32,
    sub: u32,
    is_tombstone: bool,
}

fuzz_target!(|inp: Input| {
    let kind = if inp.is_tombstone {
        KeyKind::Tombstone
    } else {
        KeyKind::Put
    };
    let rev = Revision::new(i64::from(inp.main), i64::from(inp.sub));
    let enc = encode_key(rev, kind);
    let (rev2, kind2) = decode_key(enc.as_bytes()).expect("round-trip must succeed");
    assert_eq!(rev, rev2);
    assert_eq!(kind, kind2);
});
