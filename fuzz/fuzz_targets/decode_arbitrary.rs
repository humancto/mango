//! L853: decode_key tolerates arbitrary bytes (no panic) AND
//! every Ok-decoding round-trips to itself byte-for-byte
//! (canonicalization).
//!
//! Canonicalization is the load-bearing property — "no two
//! distinct byte strings decode to the same `(rev, kind)`" — and
//! it is NOT covered by the existing proptest, which drives
//! `encode -> decode`, not the inverse direction.
//!
//! Specifically catches:
//!
//! - a future "trailing-zero tolerance" change in `decode_key`,
//! - any future addition of an alternative encoding for the same
//!   `(rev, kind)`,
//! - any change that quietly accepts non-canonical separator or
//!   marker bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mango_mvcc::{decode_key, encode_key};

fuzz_target!(|data: &[u8]| {
    if let Ok((rev, kind)) = decode_key(data) {
        // `decode_key` rejects all lengths != 17 / 18, so on the
        // Ok arm `data.len()` is exactly the encoded length. A
        // simple byte-equality therefore suffices — no slicing
        // needed.
        let enc = encode_key(rev, kind);
        assert_eq!(
            enc.as_bytes(),
            data,
            "decode->encode canonicalization broken: input {data:?} \
             decoded to {rev:?}/{kind:?} but re-encodes to {:?}",
            enc.as_bytes(),
        );
    }
    // Err is fine — the crash-free property is the point;
    // libfuzzer detects panics implicitly.
});
