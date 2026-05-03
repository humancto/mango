//! Mango MVCC primitives.
//!
//! This crate carries the pure-data foundations of Mango's MVCC
//! store (per ROADMAP.md Phase 2):
//!
//! - [`Revision`] — the `(main, sub)` revision pair Raft assigns to
//!   each apply-batch and each op within it.
//! - [`encode_key`] / [`decode_key`] — the on-disk byte format used
//!   by the `key` bucket. **Byte-for-byte equal to etcd's
//!   `server/mvcc/revision.go::revToBytes` plus
//!   `server/mvcc/kvstore.go::appendMarkTombstone` at tag `v3.5.16`**
//!   so L839's restart path can ingest etcd recovery dumps and the
//!   Phase 13 differential-fuzz harness can share fixtures with etcd
//!   unchanged.
//! - [`bucket`] — the `BucketId` and name reservations for the
//!   `key` and `key_index` buckets, plus the [`bucket::register`]
//!   helper that wires both into a [`mango_storage::Backend`].
//!
//! What this crate is NOT (each is a separate ROADMAP item):
//!
//! - The in-memory `KeyIndex` (L839)
//! - The `KV` API (L844)
//! - Read transactions / snapshot publication (L845/L846)
//! - Compaction (L849/L850)
//! - Property test against a model (L851)
//! - `cargo fuzz` target (L853)
//!
//! The crate is `unsafe`-free and allocation-free on the encoding
//! hot path.

pub mod bucket;
pub mod encoding;
pub mod key_history;
pub mod revision;

pub use bucket::{
    register, KEY_BUCKET_ID, KEY_BUCKET_NAME, KEY_INDEX_BUCKET_ID, KEY_INDEX_BUCKET_NAME,
};
pub use encoding::{decode_key, encode_key, EncodedKey, KeyDecodeError, KeyKind};
pub use key_history::{KeyAtRev, KeyHistory, KeyHistoryError, RestoreInvalidReason};
pub use revision::Revision;
