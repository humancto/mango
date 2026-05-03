//! Value-level block compression codec for the redb backend
//! (ROADMAP:830).
//!
//! Every value stored in a user bucket through `apply_staged` is run
//! through [`encode`] before being handed to redb; every value read
//! back through `RedbSnapshot::get` / `RedbRangeIter::next` is run
//! through [`decode`]. The codec is the **single** definition of the
//! on-disk encoding; no other module hand-rolls the tag-byte layout.
//!
//! # Encoding
//!
//! ```text
//! stored_bytes := tag_byte || payload
//! tag_byte ∈ { 0x00, 0x01 }
//!   0x00 (Raw):  payload is the original value bytes verbatim. Length
//!                MUST be ≥ 1 — empty values are rejected at
//!                `WriteBatch::put` (see `redb::batch::EMPTY_VALUE_ERROR`),
//!                so a stored payload of `[0x00]` (raw tag with no
//!                body) signals corruption.
//!   0x01 (Lz4):  payload = lz4_flex::compress_prepend_size(value)
//!                (4-byte little-endian original-length prefix +
//!                LZ4 block). Encoder only emits this tag when (a) the
//!                value is at least [`COMPRESS_MIN_BYTES`] long AND (b)
//!                the LZ4-encoded payload is **strictly shorter** than
//!                the raw value. Otherwise the encoder falls back to
//!                `RAW || value`.
//! 0x02..=0xff:   reserved for future codecs (e.g. Zstd takes 0x02).
//!                Readers return [`BackendError::Corruption`] with
//!                `"compression: unknown tag …"`.
//! ```
//!
//! # Hard contracts
//!
//! - The codec **always** prefixes a tag byte. Encoder output length
//!   is ≤ `value.len() + 1` for any input. There is no tagless path.
//! - The decoder is **config-blind**: it dispatches on the tag byte
//!   alone, so a database written under one [`CompressionMode`] is
//!   readable under any other.
//! - Every codec-emitted [`BackendError::Corruption`] message starts
//!   with the literal prefix `"compression: "`. Operators routing on
//!   corruption-source can grep this prefix to disambiguate from
//!   redb-internal corruption (`"redb checksum mismatch"`, etc.).
//! - Tag values are **stable for the life of the project**. Renumbering
//!   would break every on-disk database. The header-tag stability
//!   tests pin this.

use bytes::Bytes;

use crate::backend::{BackendError, CompressionMode};

/// Tag byte values. Stable for the life of the project — see the
/// "Hard contracts" section in the module doc.
pub(crate) mod tag {
    /// Raw payload: byte `0x00`, then the value bytes verbatim.
    pub(crate) const RAW: u8 = 0x00;
    /// LZ4-compressed payload: byte `0x01`, then
    /// `lz4_flex::compress_prepend_size(value)`.
    pub(crate) const LZ4: u8 = 0x01;
}

/// Value-length floor below which [`encode`] always emits `RAW`,
/// regardless of [`CompressionMode`]. LZ4 has per-block overhead;
/// short values consistently fail the ratio check and the floor
/// avoids paying for the work.
///
/// Single tunable in the codec module — adjust here only.
pub(crate) const COMPRESS_MIN_BYTES: usize = 64;

/// Defensive upper bound on the value size accepted by [`encode`].
/// `lz4_flex::compress_prepend_size` writes a 4-byte little-endian
/// length prefix; inputs whose length does not fit in `u32` would
/// silently truncate. redb's own value-size limit is well below
/// `u32::MAX` so this is unreachable in practice — the check is
/// belt-and-suspenders for forward-compat.
///
/// Production value of `u32::MAX as usize`; tests override via
/// [`encode_with_size_limit`] to exercise the guard branch without
/// allocating 4 GiB.
const SIZE_LIMIT: usize = u32::MAX as usize;

/// Encode a value for on-disk storage. See the module doc for the
/// encoding contract.
///
/// # Errors
///
/// Returns [`BackendError::Other`] if `value.len()` exceeds the
/// internal [`SIZE_LIMIT`] (unreachable on real redb traffic; redb's
/// own value-size limit fires first).
pub(crate) fn encode(mode: CompressionMode, value: &[u8]) -> Result<Vec<u8>, BackendError> {
    encode_with_size_limit(mode, value, SIZE_LIMIT)
}

/// Same as [`encode`] but with a caller-supplied size limit. Test-
/// only path: production callers go through [`encode`].
fn encode_with_size_limit(
    mode: CompressionMode,
    value: &[u8],
    size_limit: usize,
) -> Result<Vec<u8>, BackendError> {
    if value.len() > size_limit {
        return Err(BackendError::Other(format!(
            "compression: value too large ({} bytes; limit {} bytes)",
            value.len(),
            size_limit,
        )));
    }
    match mode {
        CompressionMode::None => Ok(emit_raw(value)),
        CompressionMode::Lz4 => Ok(encode_lz4_with_fallback(value)),
    }
}

/// Build a `RAW || value` payload.
fn emit_raw(value: &[u8]) -> Vec<u8> {
    // Capacity is exact; saturating_add guards the workspace
    // arithmetic-side-effects lint without changing the value
    // (value.len() ≤ SIZE_LIMIT < usize::MAX in real callers).
    let mut out = Vec::with_capacity(value.len().saturating_add(1));
    out.push(tag::RAW);
    out.extend_from_slice(value);
    out
}

/// Try LZ4; fall back to RAW if the result is not strictly shorter
/// than the raw value, or if `value.len() < COMPRESS_MIN_BYTES`.
fn encode_lz4_with_fallback(value: &[u8]) -> Vec<u8> {
    if value.len() < COMPRESS_MIN_BYTES {
        return emit_raw(value);
    }
    let compressed = lz4_flex::compress_prepend_size(value);
    if compressed.len() < value.len() {
        let mut out = Vec::with_capacity(compressed.len().saturating_add(1));
        out.push(tag::LZ4);
        out.extend_from_slice(&compressed);
        out
    } else {
        emit_raw(value)
    }
}

/// Decode an on-disk payload to its original value bytes. Config-
/// blind: dispatches on the tag byte alone.
///
/// # Errors
///
/// Returns [`BackendError::Corruption`] with a `"compression: "`-
/// prefixed message on any of:
///
/// - empty stored payload (no tag byte);
/// - `RAW` tag with no body (would imply an empty value, but empty
///   values are rejected at `WriteBatch::put` so this is corruption);
/// - unknown tag byte (`0x02..=0xff`);
/// - LZ4-tagged payload that fails to decompress (truncated frame,
///   bad size prefix, decoder bounds-check rejection).
pub(crate) fn decode(stored: &[u8]) -> Result<Bytes, BackendError> {
    let (tag_byte, body) = stored
        .split_first()
        .ok_or_else(|| BackendError::Corruption("compression: empty stored payload".to_owned()))?;
    match *tag_byte {
        tag::RAW => {
            if body.is_empty() {
                // Per `redb::batch::EMPTY_VALUE_ERROR`, empty values
                // are rejected at write time. A `[0x00]`-only stored
                // payload therefore cannot be a legitimately-written
                // empty value — it's bit-rot or a pre-tag artifact.
                // Surface as Corruption so the operator sees the
                // signal rather than receiving `Ok(Bytes::new())`.
                Err(BackendError::Corruption(
                    "compression: empty raw payload".to_owned(),
                ))
            } else {
                Ok(Bytes::copy_from_slice(body))
            }
        }
        tag::LZ4 => match lz4_flex::decompress_size_prepended(body) {
            Ok(decoded) => Ok(Bytes::from(decoded)),
            Err(e) => Err(BackendError::Corruption(format!(
                "compression: lz4 decode failed: {e}"
            ))),
        },
        other => Err(BackendError::Corruption(format!(
            "compression: unknown tag 0x{other:02x}"
        ))),
    }
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

    // --- Round-trip parity -------------------------------------------

    #[test]
    fn round_trip_none_short_value() {
        let v = b"hello".to_vec();
        let enc = encode(CompressionMode::None, &v).unwrap();
        assert_eq!(enc[0], tag::RAW);
        let dec = decode(&enc).unwrap();
        assert_eq!(dec.as_ref(), v.as_slice());
    }

    #[test]
    fn round_trip_lz4_long_compressible_value() {
        let v = vec![0x41_u8; 1024];
        let enc = encode(CompressionMode::Lz4, &v).unwrap();
        assert_eq!(enc[0], tag::LZ4, "long compressible input must use LZ4 tag");
        // Compression actually shrunk it.
        assert!(
            enc.len() < v.len() + 1,
            "encoded length {} should be smaller than raw length {}",
            enc.len(),
            v.len()
        );
        let dec = decode(&enc).unwrap();
        assert_eq!(dec.as_ref(), v.as_slice());
    }

    // --- Threshold floor ---------------------------------------------

    #[test]
    fn encode_short_input_below_threshold_uses_raw_tag() {
        // 30 bytes < COMPRESS_MIN_BYTES; even under Lz4 mode the
        // encoder must emit RAW.
        let v = vec![0x41_u8; 30];
        let enc = encode(CompressionMode::Lz4, &v).unwrap();
        assert_eq!(enc[0], tag::RAW);
        assert_eq!(&enc[1..], v.as_slice());
    }

    #[test]
    fn encode_long_compressible_input_uses_lz4_tag_with_size_prefix() {
        // [0x41; 100] — comfortably above COMPRESS_MIN_BYTES and
        // highly compressible. Tag is LZ4; the next four bytes are
        // the original size in little-endian (lz4_flex
        // compress_prepend_size convention) — `100` = `0x64` →
        // [0x64, 0x00, 0x00, 0x00].
        let v = vec![0x41_u8; 100];
        let enc = encode(CompressionMode::Lz4, &v).unwrap();
        assert_eq!(enc[0], tag::LZ4);
        assert_eq!(&enc[1..5], &[0x64, 0x00, 0x00, 0x00]);
        let dec = decode(&enc).unwrap();
        assert_eq!(dec.as_ref(), v.as_slice());
    }

    #[test]
    fn encode_incompressible_input_above_threshold_uses_raw_tag() {
        // 200 effectively-random bytes (xorshift seeded for
        // determinism). LZ4 can't shrink random data; the encoder
        // must fall back to RAW so stored payload length is
        // value.len() + 1 (Hard contract #3).
        let v: Vec<u8> = {
            let mut state: u64 = 0xDEAD_BEEF_CAFE_F00D;
            (0..200_u32)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state & 0xFF) as u8
                })
                .collect()
        };
        let enc = encode(CompressionMode::Lz4, &v).unwrap();
        assert_eq!(
            enc[0],
            tag::RAW,
            "incompressible input must fall back to RAW tag",
        );
        assert_eq!(enc.len(), v.len() + 1);
        let dec = decode(&enc).unwrap();
        assert_eq!(dec.as_ref(), v.as_slice());
    }

    // --- Decoder corruption signalling -------------------------------

    #[test]
    fn decode_unknown_tag_is_corruption() {
        let bad = [0xFF, 1, 2, 3];
        let err = decode(&bad).unwrap_err();
        match err {
            BackendError::Corruption(msg) => {
                assert!(msg.starts_with("compression: "));
                assert!(msg.contains("unknown tag"));
                assert!(msg.contains("0xff"));
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    #[test]
    fn decode_empty_stored_payload_is_corruption() {
        let err = decode(&[]).unwrap_err();
        match err {
            BackendError::Corruption(msg) => {
                assert!(msg.starts_with("compression: "));
                assert!(msg.contains("empty stored payload"));
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    #[test]
    fn decode_raw_tag_with_empty_body_is_corruption() {
        let err = decode(&[tag::RAW]).unwrap_err();
        match err {
            BackendError::Corruption(msg) => {
                assert!(msg.starts_with("compression: "));
                assert!(msg.contains("empty raw payload"));
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    #[test]
    fn decode_truncated_lz4_is_corruption() {
        // LZ4 tag, valid-looking 4-byte size prefix (says 100), but
        // no compressed bytes follow. Decoder must return Corruption.
        let bad = [tag::LZ4, 0x64, 0x00, 0x00, 0x00];
        let err = decode(&bad).unwrap_err();
        match err {
            BackendError::Corruption(msg) => {
                assert!(msg.starts_with("compression: "));
                assert!(msg.contains("lz4 decode failed"));
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    #[test]
    fn all_corruption_messages_share_prefix() {
        // Single test that walks every corruption branch and checks
        // the shared prefix. Pins Hard contract #6 against future
        // refactors that add a new corruption arm without the prefix.
        let cases: Vec<&[u8]> = vec![
            &[],           // empty payload
            &[tag::RAW],   // raw tag, no body
            &[0xFF, 1, 2], // unknown tag
            // truncated lz4 (size prefix says 100, no body)
            &[tag::LZ4, 0x64, 0x00, 0x00, 0x00],
        ];
        for stored in cases {
            let err = decode(stored).expect_err("must be Corruption");
            match err {
                BackendError::Corruption(msg) => {
                    assert!(
                        msg.starts_with("compression: "),
                        "Corruption message {msg:?} missing 'compression: ' prefix",
                    );
                }
                other => panic!("expected Corruption from {stored:?}, got {other:?}"),
            }
        }
    }

    // --- Defensive size guard ----------------------------------------

    #[test]
    fn encode_rejects_oversize_value_via_size_limit() {
        // SIZE_LIMIT in production is u32::MAX; testing that
        // literally would require allocating 4 GiB. Inject a small
        // limit via the test-only entry point and verify the guard
        // branch fires with a "compression: " prefix.
        let v = vec![0x41_u8; 100];
        let err = encode_with_size_limit(CompressionMode::Lz4, &v, 50).unwrap_err();
        match err {
            BackendError::Other(msg) => {
                assert!(msg.starts_with("compression: "));
                assert!(msg.contains("too large"));
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn encode_at_exact_size_limit_succeeds() {
        // Boundary: value.len() == size_limit is allowed (`>`, not
        // `>=`, on the guard).
        let v = vec![0x41_u8; 50];
        encode_with_size_limit(CompressionMode::None, &v, 50).unwrap();
    }

    // --- Miri smoke ---------------------------------------------------

    /// Tight encode/decode loop under Miri. Deliberately small (Miri
    /// is slow); the goal is to drive memory-safety assertions on the
    /// `lz4_flex` decode path with all codec modes and threshold
    /// boundaries exercised. Gated `#[cfg(miri)]` so the broader
    /// release/test runs do not pay for it.
    #[cfg(miri)]
    #[test]
    fn miri_smoke() {
        let inputs: Vec<Vec<u8>> = vec![
            vec![0x41; 30],  // below threshold ⇒ RAW
            vec![0x41; 100], // above threshold, compressible ⇒ LZ4
            b"hello world".to_vec(),
            (0..32_u8).collect(),
        ];
        for v in inputs {
            for mode in [CompressionMode::None, CompressionMode::Lz4] {
                let enc = encode(mode, &v).unwrap();
                let dec = decode(&enc).unwrap();
                assert_eq!(dec.as_ref(), v.as_slice());
            }
        }
    }
}
