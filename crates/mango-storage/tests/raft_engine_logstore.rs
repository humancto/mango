//! Integration tests for [`mango_storage::RaftEngineLogStore`]
//! (ROADMAP:818).
//!
//! Every test opens a real `raft-engine` under a `tempfile::TempDir`
//! and exercises the [`mango_storage::RaftLogStore`] trait surface —
//! the trait contract is what is verified, not the impl internals.
//!
//! All tests explicitly call `close()` before the handle drops per the
//! close-always-before-drop invariant documented on
//! [`mango_storage::RaftEngineLogStore`]; dropping without closing
//! risks a background-thread join panic on the upstream `Drop` path.
//!
//! Under `--cfg madsim` this file is excluded — raft-engine runs real
//! `fs2` file locks and real `fdatasync` which the simulator does not
//! substitute. The madsim smoke lives in
//! `raft_engine_madsim_smoke.rs`.

#![cfg(not(madsim))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use bytes::Bytes;
use mango_storage::{
    BackendError, HardState, RaftEngineConfig, RaftEngineLogStore, RaftEntry, RaftEntryType,
    RaftLogStore, RaftSnapshotMetadata,
};
use tempfile::TempDir;

fn open(dir: &TempDir) -> RaftEngineLogStore {
    RaftEngineLogStore::open(RaftEngineConfig::new(dir.path().to_path_buf())).expect("open")
}

fn entry(index: u64, term: u64, data: &'static [u8]) -> RaftEntry {
    RaftEntry::new(
        index,
        term,
        RaftEntryType::Normal,
        Bytes::from_static(data),
        Bytes::new(),
    )
}

fn make_batch(start: u64, count: u64, term: u64) -> Vec<RaftEntry> {
    (0..count)
        .map(|i| entry(start + i, term, b"payload"))
        .collect()
}

// ---------- basic round-trip ----------------------------------------

#[tokio::test]
async fn open_append_read_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);

    let batch = (1_u64..=5)
        .map(|i| {
            RaftEntry::new(
                i,
                1,
                RaftEntryType::Normal,
                Bytes::copy_from_slice(&i.to_le_bytes()),
                Bytes::from_static(b"ctx"),
            )
        })
        .collect::<Vec<_>>();
    store.append(&batch).await.unwrap();

    let back = store.entries(1, 6).unwrap();
    assert_eq!(back.len(), 5);
    for (i, e) in back.iter().enumerate() {
        let expected_idx = u64::try_from(i).unwrap() + 1;
        assert_eq!(e.index, expected_idx);
        assert_eq!(e.term, 1);
        assert_eq!(e.entry_type, RaftEntryType::Normal);
        assert_eq!(e.data.as_ref(), &expected_idx.to_le_bytes());
        assert_eq!(e.context.as_ref(), b"ctx");
    }

    assert_eq!(store.last_index().unwrap(), 5);
    assert_eq!(store.first_index().unwrap(), 1);

    store.close().unwrap();
}

#[test]
fn empty_store_indices_are_zero() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    assert_eq!(store.last_index().unwrap(), 0);
    assert_eq!(store.first_index().unwrap(), 0);
    assert_eq!(store.hard_state().unwrap(), HardState::default());
    store.close().unwrap();
}

// ---------- append validation ----------------------------------------

#[tokio::test]
async fn append_rejects_non_consecutive_first_entry() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    store.append(&[entry(1, 1, b"a")]).await.unwrap();
    let bad = entry(3, 1, b"b");
    let err = store.append(std::slice::from_ref(&bad)).await.unwrap_err();
    match err {
        BackendError::Other(msg) => {
            assert!(msg.contains("expected index 2"), "got: {msg}");
            assert!(msg.contains("got 3"), "got: {msg}");
        }
        other => panic!("expected Other, got {other:?}"),
    }
    store.close().unwrap();
}

#[tokio::test]
async fn append_rejects_non_monotonic_within_batch() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    let bad = vec![entry(1, 1, b"a"), entry(3, 1, b"b")];
    let err = store.append(&bad).await.unwrap_err();
    assert!(matches!(err, BackendError::Other(_)), "got {err:?}");
    store.close().unwrap();
}

#[tokio::test]
async fn append_empty_is_noop() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    store.append(&[]).await.unwrap();
    assert_eq!(store.last_index().unwrap(), 0);
    store.close().unwrap();
}

// ---------- compact & snapshot --------------------------------------

#[tokio::test]
async fn compact_shifts_first_index_and_persists_across_reopen() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    store.append(&make_batch(1, 10, 1)).await.unwrap();
    store.compact(5).await.unwrap();
    assert_eq!(store.first_index().unwrap(), 5);
    store.close().unwrap();

    let store = open(&tmp);
    assert_eq!(store.first_index().unwrap(), 5);
    assert_eq!(store.last_index().unwrap(), 10);
    store.close().unwrap();
}

#[tokio::test]
async fn compact_beyond_last_is_noop_or_clamped() {
    // raft-engine may either no-op or clamp when compacting past
    // last_index. Both are acceptable — the trait contract is that no
    // error surfaces and the store stays usable.
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    store.append(&make_batch(1, 5, 1)).await.unwrap();
    store.compact(100).await.unwrap();
    store.close().unwrap();
}

#[tokio::test]
async fn install_snapshot_updates_first_index_and_persists() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    // Populate, then install a snapshot past all existing entries.
    store.append(&make_batch(1, 50, 1)).await.unwrap();
    let snap = RaftSnapshotMetadata::new(100, 3, Bytes::new());
    store.install_snapshot(&snap).await.unwrap();

    // Trait post-conditions: first_index == snap.index + 1,
    // last_index >= snap.index.
    assert_eq!(store.first_index().unwrap(), 101);
    assert_eq!(store.last_index().unwrap(), 100);
    store.close().unwrap();

    // Reopen — the cursor is recovered from SNAPSHOT_META_KEY.
    let store = open(&tmp);
    assert_eq!(store.first_index().unwrap(), 101);
    assert_eq!(store.last_index().unwrap(), 100);
    // A fresh append MUST start at snap.index + 1.
    store.append(&[entry(101, 3, b"post-snap")]).await.unwrap();
    assert_eq!(store.last_index().unwrap(), 101);
    store.close().unwrap();
}

// ---------- hard state ----------------------------------------------

#[tokio::test]
async fn save_and_read_hard_state_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    let hs = HardState::new(3, 7, 42);
    store.save_hard_state(&hs).await.unwrap();
    assert_eq!(store.hard_state().unwrap(), hs);
    store.close().unwrap();
}

#[tokio::test]
async fn hard_state_persists_across_reopen() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    let hs = HardState::new(5, 9, 100);
    store.save_hard_state(&hs).await.unwrap();
    store.close().unwrap();

    let store = open(&tmp);
    assert_eq!(store.hard_state().unwrap(), hs);
    store.close().unwrap();
}

// ---------- lifecycle ------------------------------------------------

#[tokio::test]
async fn closed_store_returns_closed_error() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    store.close().unwrap();

    assert!(matches!(store.last_index(), Err(BackendError::Closed)));
    assert!(matches!(store.first_index(), Err(BackendError::Closed)));
    assert!(matches!(store.entries(1, 2), Err(BackendError::Closed)));
    assert!(matches!(store.hard_state(), Err(BackendError::Closed)));
    assert!(matches!(
        store.append(&[entry(1, 1, b"x")]).await,
        Err(BackendError::Closed)
    ));
    assert!(matches!(store.compact(1).await, Err(BackendError::Closed)));
    assert!(matches!(
        store
            .install_snapshot(&RaftSnapshotMetadata::new(1, 1, Bytes::new()))
            .await,
        Err(BackendError::Closed)
    ));
    assert!(matches!(
        store.save_hard_state(&HardState::default()).await,
        Err(BackendError::Closed)
    ));

    // close is idempotent.
    store.close().unwrap();
}

#[tokio::test]
async fn reopen_preserves_entries_and_hard_state() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    store.append(&make_batch(1, 7, 2)).await.unwrap();
    let hs = HardState::new(2, 3, 7);
    store.save_hard_state(&hs).await.unwrap();
    store.close().unwrap();

    let store = open(&tmp);
    assert_eq!(store.first_index().unwrap(), 1);
    assert_eq!(store.last_index().unwrap(), 7);
    assert_eq!(store.hard_state().unwrap(), hs);
    let back = store.entries(1, 8).unwrap();
    assert_eq!(back.len(), 7);
    for (i, e) in back.iter().enumerate() {
        assert_eq!(e.index, u64::try_from(i).unwrap() + 1);
        assert_eq!(e.term, 2);
    }
    store.close().unwrap();
}

#[tokio::test]
async fn concurrent_appenders_with_external_serialization() {
    // Documents the contract: the RaftLogStore wrapper does NOT
    // serialize concurrent appenders — the caller (raft-rs state
    // machine) does. We serialize here by awaiting t1 before kicking
    // off t2 and assert the combined log.
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    let s1 = store.clone();
    let t1 = tokio::spawn(async move {
        s1.append(&make_batch(1, 100, 1)).await.unwrap();
    });
    t1.await.unwrap();

    let s2 = store.clone();
    let t2 = tokio::spawn(async move {
        s2.append(&make_batch(101, 100, 1)).await.unwrap();
    });
    t2.await.unwrap();

    assert_eq!(store.last_index().unwrap(), 200);
    assert_eq!(store.first_index().unwrap(), 1);
    store.close().unwrap();
}

// ---------- range errors ---------------------------------------------

#[tokio::test]
async fn entries_rejects_inverted_range() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    store.append(&make_batch(1, 3, 1)).await.unwrap();
    let err = store.entries(5, 2).unwrap_err();
    assert!(matches!(err, BackendError::InvalidRange(_)), "got {err:?}");
    store.close().unwrap();
}

#[tokio::test]
async fn entries_rejects_out_of_bounds() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    store.append(&make_batch(1, 3, 1)).await.unwrap();
    // last_index = 3, so (1, 5) is out of range.
    let err = store.entries(1, 5).unwrap_err();
    assert!(matches!(err, BackendError::InvalidRange(_)), "got {err:?}");
    store.close().unwrap();
}

#[tokio::test]
async fn entries_empty_range_is_ok() {
    let tmp = TempDir::new().unwrap();
    let store = open(&tmp);
    store.append(&make_batch(1, 3, 1)).await.unwrap();
    let out = store.entries(2, 2).unwrap();
    assert!(out.is_empty());
    store.close().unwrap();
}
