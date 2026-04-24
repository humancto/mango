//! Type conversions at the `raft-engine` / `raft-rs` boundary.
//!
//! raft-engine stores entries as `raft::eraftpb::Entry` (rust-protobuf
//! v2 types, generated with `carllerche_bytes_for_bytes_all = true`
//! so `data` and `context` fields are `bytes::Bytes`). The public
//! [`super::super::backend::RaftLogStore`] trait exposes the engine-
//! agnostic [`RaftEntry`] / [`HardState`] / [`RaftSnapshotMetadata`]
//! surface. This module converts in both directions.
//!
//! Conversions are infallible except for `EntryType` narrowing:
//! `EntryConfChangeV2` is folded to [`RaftEntryType::ConfChange`]
//! per the backend contract (see `backend.rs` docstring on
//! [`RaftEntryType`] — "config changes ride as normal entries").
//!
//! # Cost
//!
//! `bytes::Bytes` is reference-counted so the `data` / `context`
//! field clones are refcount bumps, not buffer copies. The `ConfState`
//! for `RaftSnapshotMetadata` IS serialized to bytes on the way down
//! (rust-protobuf `write_to_bytes`) and parsed on the way up — that
//! is a real allocation, but snapshot installs are rare.

use bytes::Bytes;
// Re-export path `::raft::protocompat::PbMessage` is the stable
// `Message` surface for the protobuf codec regardless of whether
// raft-rs is built with `protobuf-codec` or `prost-codec`. We consume
// raft-rs with `features = ["protobuf-codec"]` in `Cargo.toml`; this
// import shape means a future switch would fail to compile here
// rather than in every call site.
use ::raft::protocompat::PbMessage as _;

use crate::backend::{BackendError, HardState, RaftEntry, RaftEntryType, RaftSnapshotMetadata};

/// Convert a mango [`RaftEntry`] into the protobuf `Entry` raft-engine
/// expects. The `data` and `context` `Bytes` are consumed by value —
/// no deep copy.
pub(super) fn entry_to_proto(e: RaftEntry) -> ::raft::eraftpb::Entry {
    let mut proto = ::raft::eraftpb::Entry::default();
    proto.set_entry_type(entry_type_to_proto(e.entry_type));
    proto.term = e.term;
    proto.index = e.index;
    // `data` / `context` on the proto side are already `bytes::Bytes`
    // (see module docstring re: `carllerche_bytes_for_bytes_all`).
    proto.data = e.data;
    proto.context = e.context;
    proto
}

/// Convert a raft-engine-returned `Entry` into the mango
/// [`RaftEntry`] shape. Consumes the proto — all bytes fields are
/// `Bytes` already, so this is refcount-only.
pub(super) fn entry_from_proto(mut p: ::raft::eraftpb::Entry) -> RaftEntry {
    // `take_data` / `take_context` drain the field (leave Bytes::new()
    // behind) and hand us ownership without a clone.
    let data = p.take_data();
    let context = p.take_context();
    RaftEntry::new(
        p.index,
        p.term,
        entry_type_from_proto(p.get_entry_type()),
        data,
        context,
    )
}

fn entry_type_to_proto(t: RaftEntryType) -> ::raft::eraftpb::EntryType {
    // `RaftEntryType` is `#[non_exhaustive]` (see backend.rs). A
    // future variant reaching this path would mean the caller
    // constructed an entry type the storage layer has no mapping
    // for; we fall through to `EntryNormal` as a defensive default.
    // Silent mis-persistence is prevented upstream because adding a
    // new variant breaks exhaustiveness here at compile time in
    // this crate (the `#[non_exhaustive]` only hides variants from
    // *external* crates).
    match t {
        RaftEntryType::Normal => ::raft::eraftpb::EntryType::EntryNormal,
        RaftEntryType::ConfChange => ::raft::eraftpb::EntryType::EntryConfChange,
    }
}

fn entry_type_from_proto(t: ::raft::eraftpb::EntryType) -> RaftEntryType {
    match t {
        ::raft::eraftpb::EntryType::EntryNormal => RaftEntryType::Normal,
        // Both V1 and V2 config changes collapse to the single
        // mango `ConfChange` variant per the backend contract.
        ::raft::eraftpb::EntryType::EntryConfChange
        | ::raft::eraftpb::EntryType::EntryConfChangeV2 => RaftEntryType::ConfChange,
    }
}

/// Convert a mango [`HardState`] to the protobuf `HardState`
/// raft-engine persists.
pub(super) fn hard_state_to_proto(hs: HardState) -> ::raft::eraftpb::HardState {
    ::raft::eraftpb::HardState {
        term: hs.term,
        vote: hs.vote,
        commit: hs.commit,
        ..::raft::eraftpb::HardState::default()
    }
}

/// Convert a protobuf `HardState` (as read from raft-engine) back to
/// the mango surface.
pub(super) fn hard_state_from_proto(p: &::raft::eraftpb::HardState) -> HardState {
    HardState::new(p.term, p.vote, p.commit)
}

/// Convert a mango [`RaftSnapshotMetadata`] into the protobuf shape.
/// `conf_state` bytes are parsed as a protobuf `ConfState`; an empty
/// `Bytes` yields `ConfState::default()` without a parse call.
///
/// # Errors
///
/// [`BackendError::Corruption`] if the encoded `ConfState` fails to
/// parse.
pub(super) fn snapshot_metadata_to_proto(
    m: &RaftSnapshotMetadata,
) -> Result<::raft::eraftpb::SnapshotMetadata, BackendError> {
    let mut p = ::raft::eraftpb::SnapshotMetadata {
        index: m.index,
        term: m.term,
        ..::raft::eraftpb::SnapshotMetadata::default()
    };
    if !m.conf_state.is_empty() {
        let conf_state =
            <::raft::eraftpb::ConfState as ::raft::protocompat::PbMessage>::parse_from_bytes(
                &m.conf_state,
            )
            .map_err(|e| BackendError::Corruption(format!("ConfState decode: {e}")))?;
        p.set_conf_state(conf_state);
    }
    Ok(p)
}

/// Convert a protobuf `SnapshotMetadata` back into the mango surface.
/// Re-encodes `conf_state` to `Bytes` so the public struct stays
/// engine-agnostic.
///
/// Currently exercised only by the unit-test roundtrip; Phase 3
/// (`mango-raft`) is the first real caller, when the raft-rs
/// `Storage::snapshot` bridge needs to read back the installed
/// metadata. Kept in this module so the inverse direction is defined
/// next to the forward one — the conversions must stay symmetric to
/// avoid a lossy pipeline.
///
/// # Errors
///
/// [`BackendError::Corruption`] on a protobuf encode failure; in
/// practice this is impossible for a well-formed value, but
/// rust-protobuf's API is fallible so we propagate.
#[allow(dead_code)]
pub(super) fn snapshot_metadata_from_proto(
    mut p: ::raft::eraftpb::SnapshotMetadata,
) -> Result<RaftSnapshotMetadata, BackendError> {
    let conf_state_bytes: Bytes = if p.has_conf_state() {
        let cs = p.take_conf_state();
        let encoded = cs
            .write_to_bytes()
            .map_err(|e| BackendError::Corruption(format!("ConfState encode: {e}")))?;
        Bytes::from(encoded)
    } else {
        Bytes::new()
    };
    Ok(RaftSnapshotMetadata::new(p.index, p.term, conf_state_bytes))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn entry_roundtrip_normal() {
        let original = RaftEntry::new(
            42,
            7,
            RaftEntryType::Normal,
            Bytes::from_static(b"payload"),
            Bytes::from_static(b"ctx"),
        );
        let proto = entry_to_proto(original.clone());
        let back = entry_from_proto(proto);
        assert_eq!(back, original);
    }

    #[test]
    fn entry_roundtrip_conf_change() {
        let original = RaftEntry::new(
            100,
            3,
            RaftEntryType::ConfChange,
            Bytes::from_static(b"conf-change-payload"),
            Bytes::new(),
        );
        let proto = entry_to_proto(original.clone());
        let back = entry_from_proto(proto);
        assert_eq!(back, original);
    }

    #[test]
    fn entry_v2_conf_change_folds_to_conf_change() {
        // raft-engine can hand us an `EntryConfChangeV2` entry on
        // read (older logs written by an upstream raft-rs at a
        // different EntryType). The backend contract collapses V1
        // and V2 onto the single mango variant.
        let mut proto = ::raft::eraftpb::Entry::default();
        proto.set_entry_type(::raft::eraftpb::EntryType::EntryConfChangeV2);
        proto.term = 1;
        proto.index = 1;
        proto.data = Bytes::from_static(b"v2");
        let mango = entry_from_proto(proto);
        assert_eq!(mango.entry_type, RaftEntryType::ConfChange);
    }

    #[test]
    fn hard_state_roundtrip() {
        let hs = HardState::new(5, 7, 42);
        let proto = hard_state_to_proto(hs);
        let back = hard_state_from_proto(&proto);
        assert_eq!(back, hs);
    }

    #[test]
    fn hard_state_default_roundtrip() {
        let hs = HardState::default();
        let proto = hard_state_to_proto(hs);
        let back = hard_state_from_proto(&proto);
        assert_eq!(back, HardState::default());
    }

    #[test]
    fn snapshot_metadata_roundtrip_empty_conf_state() {
        let m = RaftSnapshotMetadata::new(100, 9, Bytes::new());
        let proto = snapshot_metadata_to_proto(&m).unwrap();
        let back = snapshot_metadata_from_proto(proto).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn snapshot_metadata_roundtrip_populated_conf_state() {
        // Build a real ConfState, encode to bytes, hand it to the
        // mango surface, roundtrip, and assert the encoded bytes
        // are semantically identical (proto equality on the decoded
        // struct — not byte-equality, because rust-protobuf doesn't
        // guarantee canonical field ordering on re-encode).
        let mut cs = ::raft::eraftpb::ConfState::default();
        cs.mut_voters().extend([1_u64, 2, 3]);
        cs.mut_learners().extend([4_u64]);
        let encoded = cs.write_to_bytes().unwrap();

        let m = RaftSnapshotMetadata::new(50, 3, Bytes::from(encoded));
        let proto = snapshot_metadata_to_proto(&m).unwrap();
        assert_eq!(proto.index, 50);
        assert_eq!(proto.term, 3);
        assert_eq!(proto.get_conf_state().get_voters(), &[1, 2, 3]);
        assert_eq!(proto.get_conf_state().get_learners(), &[4]);

        // Round-trip: proto → mango → proto → decode.
        let mango = snapshot_metadata_from_proto(proto).unwrap();
        let re_proto = snapshot_metadata_to_proto(&mango).unwrap();
        assert_eq!(re_proto.get_conf_state().get_voters(), &[1, 2, 3]);
        assert_eq!(re_proto.get_conf_state().get_learners(), &[4]);
    }

    #[test]
    fn snapshot_metadata_corruption_surfaces_as_backend_error() {
        // Garbage `conf_state` bytes must produce `Corruption`, not
        // a panic or silent drop.
        let m = RaftSnapshotMetadata::new(1, 1, Bytes::from_static(b"\xff\xff\xff\xff"));
        let err = snapshot_metadata_to_proto(&m).expect_err("garbage bytes must fail to decode");
        assert!(
            matches!(err, BackendError::Corruption(_)),
            "expected Corruption, got {err:?}"
        );
    }
}
