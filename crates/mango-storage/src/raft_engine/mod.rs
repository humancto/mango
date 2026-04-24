//! `raft-engine`-backed [`RaftLogStore`](crate::RaftLogStore) impl.
//!
//! The public entry points are [`RaftEngineLogStore`] and the
//! companion [`RaftEngineConfig`]. Supporting modules:
//!
//! * [`config`] — public configuration surface; re-exports
//!   [`ReadableSize`] and [`RecoveryMode`] from raft-engine for
//!   callers that do not want to import the engine crate directly.
//! * [`convert`] — conversions between mango types
//!   ([`crate::RaftEntry`], [`crate::HardState`],
//!   [`crate::RaftSnapshotMetadata`]) and the `raft::eraftpb::*`
//!   protobuf types raft-engine persists.
//! * [`EntryMessageExt`] — the single [`::raft_engine::MessageExt`]
//!   impl used by the engine's `add_entries` / `fetch_entries_to`
//!   APIs. Supplied locally because raft-engine ships its own impl
//!   only in its `#[cfg(test)]` tree.
//!
//! # Close-always-before-drop invariant
//!
//! [`::raft_engine::Engine::drop`](https://docs.rs/raft-engine/0.4.2/raft_engine/engine/struct.Engine.html)
//! unconditionally joins its background purge/rewrite threads and
//! panics if any of them errored out (engine.rs:441-446 in the
//! humancto fork, unchanged from upstream). That is an upstream bug
//! we cannot fix from here, so **every caller MUST invoke
//! [`RaftEngineLogStore::close`] before the handle drops**. Tests use
//! scope-guards to enforce this; production callers in `mango-raft`
//! will do the same inside their shutdown path.
//!
//! # Truncation lives above this layer
//!
//! raft-rs's `Storage::append` contract says the storage MUST
//! truncate already-present entries when a new leader sends entries
//! at overlapping indices. Phase 1 [`RaftLogStore::append`] REJECTS
//! non-consecutive appends with [`BackendError::Other`]. The
//! buffering + `compact + append` adapter that handles truncation
//! lives in Phase 3 (`mango-raft`). This mirrors `TiKV`'s architecture:
//! the engine stays pure append-only; the raft-rs adapter owns
//! truncation. See `.planning/raft-engine-logstore.plan.md`
//! §"Non-goals" for the full rationale.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::backend::{BackendError, HardState, RaftEntry, RaftLogStore, RaftSnapshotMetadata};

mod config;
mod convert;

pub use config::{RaftEngineConfig, ReadableSize, RecoveryMode};

/// Hard-coded Raft group id. Phase 1 runs a single Raft group;
/// multi-raft fan-out lands in Phase 10 (see ROADMAP). The const is
/// `pub(crate)` so follow-up modules (e.g. crash-recovery tests)
/// share the same id without re-declaring it.
pub(crate) const RAFT_GROUP_ID: u64 = 1;

/// Key under which the current [`HardState`] is persisted.
/// raft-engine reserves the `__` prefix (`INTERNAL_KEY_PREFIX` in
/// raft-engine/src/lib.rs); keys outside that range are free. `b"hs"`
/// is readable in a hexdump and distinct from
/// [`SNAPSHOT_META_KEY`].
const HARD_STATE_KEY: &[u8] = b"hs";

/// Key under which the current snapshot metadata is persisted. Same
/// reserved-prefix rules as [`HARD_STATE_KEY`].
const SNAPSHOT_META_KEY: &[u8] = b"snap_meta";

/// `MessageExt` impl for `raft::eraftpb::Entry`. raft-engine's
/// `add_entries<M: MessageExt>` / `fetch_entries_to<M: MessageExt>`
/// APIs are generic over this trait so they can probe the `index` of
/// any protobuf-flavored entry type; raft-engine ships the impl only
/// in its `#[cfg(test)]` tree (raft-engine/src/lib.rs:226), so every
/// downstream must supply its own.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EntryMessageExt;

impl ::raft_engine::MessageExt for EntryMessageExt {
    type Entry = ::raft::eraftpb::Entry;

    fn index(e: &Self::Entry) -> u64 {
        e.index
    }
}

/// raft-engine-backed implementation of [`RaftLogStore`]. Cheaply
/// cloneable (`Arc<Inner>` internally); every clone shares the same
/// engine handle and `close` state.
///
/// See the [module docstring](self) for the
/// close-always-before-drop invariant and the Phase 3 truncation
/// deferral.
#[derive(Clone)]
pub struct RaftEngineLogStore {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for RaftEngineLogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // raft-engine's `Engine` does not implement `Debug`; surface
        // the operator-relevant state without leaking engine
        // internals.
        f.debug_struct("RaftEngineLogStore")
            .field("data_dir", &self.inner.data_dir)
            .field("closed", &self.inner.closed.load(Ordering::Acquire))
            .finish()
    }
}

struct Inner {
    /// `Arc<Engine>` because every engine method (`write`,
    /// `fetch_entries_to`, `get_message`, `first_index`,
    /// `last_index`) takes `&self`. The `Mutex<Option<_>>` wrapper
    /// exists ONLY so [`RaftEngineLogStore::close`] can
    /// deterministically drop the last reference in-handle; normal
    /// call paths hold the mutex for the clone only.
    ///
    /// Not a `RwLock`: there is no `&mut self` operation on `Engine`
    /// (no equivalent of `redb::Database::compact`).
    engine: Mutex<Option<Arc<::raft_engine::Engine>>>,
    /// `true` once `close` has returned `Ok(())`. Checked via
    /// `load(Acquire)` at the top of every public method so
    /// concurrent in-flight operations see one consistent "closed"
    /// cut.
    closed: AtomicBool,
    /// Index of the most recently installed snapshot (0 when no
    /// snapshot has been installed). Consulted by [`Self::first_index`]
    /// and [`Self::last_index`] so the trait's
    /// "`first_index() == snapshot.index + 1` after install" contract
    /// holds even when all log entries have been compacted out.
    ///
    /// raft-engine itself has no notion of a snapshot cursor — its
    /// `first_index` / `last_index` only report the bounds of extant
    /// log entries. We persist the cursor alongside the snapshot
    /// metadata under [`SNAPSHOT_META_KEY`] and cache it here to keep
    /// index lookups O(1) on the hot path.
    snapshot_index: AtomicU64,
    /// Data directory; captured at open time for error messages and
    /// future diagnostics. Intentionally unused on the hot path.
    #[allow(dead_code)]
    data_dir: PathBuf,
}

impl RaftEngineLogStore {
    /// Open (or create) a raft-engine log store under
    /// `cfg.data_dir`. raft-engine itself creates missing
    /// directories, so no pre-flight `create_dir_all` is required.
    ///
    /// # Errors
    ///
    /// Any engine-level error (I/O, corruption, invalid argument) is
    /// translated through [`map_engine_error`].
    pub fn open(cfg: RaftEngineConfig) -> Result<Self, BackendError> {
        let data_dir = cfg.data_dir.clone();
        let engine_cfg = cfg.into_engine_config();
        let engine = ::raft_engine::Engine::open(engine_cfg).map_err(map_engine_error)?;

        // Recover the snapshot cursor from SNAPSHOT_META_KEY so
        // `first_index` / `last_index` honor the
        // "install_snapshot → first_index == snap.index+1" invariant
        // across restarts.
        let snap_proto: Option<::raft::eraftpb::SnapshotMetadata> = engine
            .get_message(RAFT_GROUP_ID, SNAPSHOT_META_KEY)
            .map_err(map_engine_error)?;
        let snapshot_index = snap_proto.map_or(0, |p| p.index);

        Ok(Self {
            inner: Arc::new(Inner {
                engine: Mutex::new(Some(Arc::new(engine))),
                closed: AtomicBool::new(false),
                snapshot_index: AtomicU64::new(snapshot_index),
                data_dir,
            }),
        })
    }

    /// Idempotent close. Drops the engine handle, which causes
    /// raft-engine to join its background threads. Subsequent calls
    /// return `Ok(())` without side-effect.
    ///
    /// **Must be called before the last `RaftEngineLogStore` clone
    /// is dropped** — see the close-always-before-drop invariant in
    /// the module docstring.
    ///
    /// # Errors
    ///
    /// Currently never returns `Err`; reserved for a future
    /// graceful-shutdown hook. Returning `Result` keeps the trait
    /// door open if raft-engine ever exposes a fallible close.
    pub fn close(&self) -> Result<(), BackendError> {
        if self
            .inner
            .closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // Drop the last Arc<Engine> reference held inside the
            // handle. If any other `clone()` is still live, this
            // does NOT close the engine — but every method on this
            // type checks `closed` first and short-circuits with
            // `BackendError::Closed`, so no further operations will
            // run against the engine from this type.
            let mut guard = self.inner.engine.lock();
            *guard = None;
        }
        Ok(())
    }

    /// Explicit purge of old log files beyond the configured
    /// threshold. Surfaced on the public type because
    /// [`RaftEngineConfig::purge_threshold`] would otherwise be an
    /// orphaned knob — raft-engine does not purge automatically.
    ///
    /// Returns the set of Raft group ids that still hold references
    /// into the oldest active file (empty in the single-group
    /// Phase 1 layout under normal operation).
    ///
    /// # Errors
    ///
    /// Engine-level errors translate through [`map_engine_error`].
    pub fn purge_expired_files(&self) -> Result<Vec<u64>, BackendError> {
        let engine = self.engine_handle()?;
        engine.purge_expired_files().map_err(map_engine_error)
    }

    /// Clone the `Arc<Engine>` or return [`BackendError::Closed`].
    /// Factored because every method repeats the same check. The
    /// mutex is held for the clone only.
    fn engine_handle(&self) -> Result<Arc<::raft_engine::Engine>, BackendError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(BackendError::Closed);
        }
        self.inner
            .engine
            .lock()
            .as_ref()
            .cloned()
            .ok_or(BackendError::Closed)
    }

    /// Validate and convert an append batch. Returns `Ok(None)` for
    /// the empty-input fast path (no `spawn_blocking` needed); returns
    /// `Ok(Some((engine, batch)))` ready for `engine.write` on any
    /// non-empty input; returns `Err` for a closed store or a
    /// consecutivity violation. Factored out of [`Self::append`] so
    /// the synchronous validation path stays clippy-clean without
    /// breaking the `async fn` return type.
    #[allow(clippy::type_complexity)]
    fn prepare_append(
        &self,
        entries: &[RaftEntry],
    ) -> Result<Option<(Arc<::raft_engine::Engine>, ::raft_engine::LogBatch)>, BackendError> {
        let engine = self.engine_handle()?;
        let Some((first, rest)) = entries.split_first() else {
            return Ok(None);
        };

        // Strict consecutivity: first.index == last_index + 1.
        // `last_index` here is the trait-level last_index (max of the
        // engine's last entry and any installed-snapshot cursor), so
        // the first post-snapshot append after an empty-log reopen
        // expects `snapshot.index + 1` rather than `1`. Truncation
        // lives in Phase 3 (see module docstring).
        let engine_last = engine.last_index(RAFT_GROUP_ID).unwrap_or(0);
        let snap_last = self.inner.snapshot_index.load(Ordering::Acquire);
        let expected = engine_last.max(snap_last).wrapping_add(1);
        if first.index != expected {
            return Err(BackendError::Other(format!(
                "non-consecutive append: expected index {expected}, got {actual}",
                actual = first.index,
            )));
        }

        // Strict monotonic-by-one within the batch. raft-rs
        // guarantees this upstream; the defensive check catches
        // caller bugs with a readable message instead of letting
        // raft-engine's panic-on-debug-assert fire.
        let mut prev = first.index;
        for entry in rest {
            let next = prev.wrapping_add(1);
            if entry.index != next {
                return Err(BackendError::Other(format!(
                    "non-consecutive entries in batch: {prev} -> {curr}",
                    curr = entry.index,
                )));
            }
            prev = entry.index;
        }

        let protos: Vec<::raft::eraftpb::Entry> = entries
            .iter()
            .cloned()
            .map(convert::entry_to_proto)
            .collect();

        let mut batch = ::raft_engine::LogBatch::default();
        batch
            .add_entries::<EntryMessageExt>(RAFT_GROUP_ID, &protos)
            .map_err(map_engine_error)?;

        Ok(Some((engine, batch)))
    }
}

impl RaftLogStore for RaftEngineLogStore {
    fn append(
        &self,
        entries: &[RaftEntry],
    ) -> impl Future<Output = Result<(), BackendError>> + Send {
        // Sync prologue: validate, convert, build the batch. All the
        // work that can fail on caller input happens here so the
        // async block only deals with engine I/O.
        let prepared = self.prepare_append(entries);
        async move {
            let Some((engine, mut batch)) = prepared? else {
                return Ok(());
            };
            tokio::task::spawn_blocking(move || {
                engine
                    .write(&mut batch, /*sync=*/ true)
                    .map(|_bytes| ())
                    .map_err(map_engine_error)
            })
            .await
            .map_err(|e| map_join_error(&e))?
        }
    }

    fn entries(&self, low: u64, high: u64) -> Result<Vec<RaftEntry>, BackendError> {
        let engine = self.engine_handle()?;
        if low > high {
            return Err(BackendError::InvalidRange("low > high"));
        }
        if low == high {
            return Ok(Vec::new());
        }
        // Range bounds are checked against the engine's actual entry
        // bounds (NOT the snapshot-adjusted trait-level indices) —
        // reading entries that have been compacted out must fail
        // even if `first_index()` reports them as notionally present
        // (post-snapshot, `first_index() == snap.index + 1` but the
        // engine may have nothing to return).
        let first = engine.first_index(RAFT_GROUP_ID).unwrap_or(0);
        let last = engine.last_index(RAFT_GROUP_ID).unwrap_or(0);
        if first == 0 || low < first || high > last.wrapping_add(1) {
            return Err(BackendError::InvalidRange(
                "range outside [first_index, last_index+1]",
            ));
        }
        // Capacity is advisory; clamp the u64 span to usize rather
        // than propagate a try_from error for a Vec allocation.
        let span = usize::try_from(high.saturating_sub(low)).unwrap_or(usize::MAX);
        let mut out = Vec::with_capacity(span);
        engine
            .fetch_entries_to::<EntryMessageExt>(RAFT_GROUP_ID, low, high, None, &mut out)
            .map_err(map_engine_error)?;
        Ok(out.into_iter().map(convert::entry_from_proto).collect())
    }

    fn last_index(&self) -> Result<u64, BackendError> {
        let engine = self.engine_handle()?;
        // Max of the engine's reported last entry and any installed
        // snapshot cursor, so the
        // "`last_index() >= snapshot.index` after install" invariant
        // holds even when all log entries have been compacted out.
        let engine_last = engine.last_index(RAFT_GROUP_ID).unwrap_or(0);
        let snap_last = self.inner.snapshot_index.load(Ordering::Acquire);
        Ok(engine_last.max(snap_last))
    }

    fn first_index(&self) -> Result<u64, BackendError> {
        let engine = self.engine_handle()?;
        // If the engine has any extant entries, its first_index is
        // authoritative (it is always `>= snap_index + 1` because
        // install_snapshot compacts through `snap_index`). When the
        // engine is empty post-snapshot, fall through to
        // `snap_index + 1` so the trait's
        // "`first_index() == snapshot.index + 1` after install"
        // invariant holds.
        let engine_first = engine.first_index(RAFT_GROUP_ID);
        if let Some(idx) = engine_first {
            return Ok(idx);
        }
        let snap = self.inner.snapshot_index.load(Ordering::Acquire);
        Ok(if snap == 0 { 0 } else { snap.wrapping_add(1) })
    }

    fn compact(&self, idx: u64) -> impl Future<Output = Result<(), BackendError>> + Send {
        let engine = self.engine_handle();
        async move {
            let engine = engine?;
            tokio::task::spawn_blocking(move || {
                // `Engine::compact_to` is `sync=false`; hand-roll a
                // batch so durability holds on return.
                let mut batch = ::raft_engine::LogBatch::default();
                batch.add_command(
                    RAFT_GROUP_ID,
                    ::raft_engine::Command::Compact { index: idx },
                );
                engine
                    .write(&mut batch, /*sync=*/ true)
                    .map(|_bytes| ())
                    .map_err(map_engine_error)
            })
            .await
            .map_err(|e| map_join_error(&e))?
        }
    }

    fn install_snapshot(
        &self,
        snapshot: &RaftSnapshotMetadata,
    ) -> impl Future<Output = Result<(), BackendError>> + Send {
        let prologue = (|| -> Result<_, BackendError> {
            let engine = self.engine_handle()?;
            let proto = convert::snapshot_metadata_to_proto(snapshot)?;
            let idx = snapshot.index;
            Ok((engine, proto, idx))
        })();
        let inner = self.inner.clone();

        async move {
            let (engine, proto, idx) = prologue?;
            tokio::task::spawn_blocking(move || {
                let mut batch = ::raft_engine::LogBatch::default();
                batch
                    .put_message(RAFT_GROUP_ID, SNAPSHOT_META_KEY.to_vec(), &proto)
                    .map_err(map_engine_error)?;
                // `Command::Compact { index: N }` removes entries with
                // `idx < N`. To discard every entry AT OR BEFORE
                // `snapshot.index` (per the trait contract: entries
                // strictly before `snapshot.index + 1` may be
                // discarded), pass `snapshot.index + 1`.
                batch.add_command(
                    RAFT_GROUP_ID,
                    ::raft_engine::Command::Compact {
                        index: idx.wrapping_add(1),
                    },
                );
                engine
                    .write(&mut batch, /*sync=*/ true)
                    .map(|_bytes| ())
                    .map_err(map_engine_error)
            })
            .await
            .map_err(|e| map_join_error(&e))??;

            // Cache the cursor only after the durable write succeeded.
            // Monotonic update: never regress on a stale install.
            let mut cur = inner.snapshot_index.load(Ordering::Acquire);
            while idx > cur {
                match inner.snapshot_index.compare_exchange_weak(
                    cur,
                    idx,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(observed) => cur = observed,
                }
            }
            Ok(())
        }
    }

    fn save_hard_state(
        &self,
        hs: &HardState,
    ) -> impl Future<Output = Result<(), BackendError>> + Send {
        let engine = self.engine_handle();
        let proto = convert::hard_state_to_proto(*hs);
        async move {
            let engine = engine?;
            tokio::task::spawn_blocking(move || {
                let mut batch = ::raft_engine::LogBatch::default();
                batch
                    .put_message(RAFT_GROUP_ID, HARD_STATE_KEY.to_vec(), &proto)
                    .map_err(map_engine_error)?;
                engine
                    .write(&mut batch, /*sync=*/ true)
                    .map(|_bytes| ())
                    .map_err(map_engine_error)
            })
            .await
            .map_err(|e| map_join_error(&e))?
        }
    }

    fn hard_state(&self) -> Result<HardState, BackendError> {
        let engine = self.engine_handle()?;
        let proto: Option<::raft::eraftpb::HardState> = engine
            .get_message(RAFT_GROUP_ID, HARD_STATE_KEY)
            .map_err(map_engine_error)?;
        Ok(proto
            .map(|p| convert::hard_state_from_proto(&p))
            .unwrap_or_default())
    }
}

/// Translate a [`::raft_engine::Error`] into [`BackendError`]. The
/// trait contract forbids leaking engine types (ADR 0002 §6 design
/// point 1). Each arm preserves the upstream string so operators can
/// correlate failures with raft-engine logs.
fn map_engine_error(e: ::raft_engine::Error) -> BackendError {
    match e {
        ::raft_engine::Error::Io(err) => BackendError::Io(err),
        ::raft_engine::Error::Corruption(s) => BackendError::Corruption(s),
        ::raft_engine::Error::InvalidArgument(s) => {
            BackendError::Other(format!("raft-engine invalid argument: {s}"))
        }
        ::raft_engine::Error::Codec(err) => {
            BackendError::Corruption(format!("raft-engine codec: {err}"))
        }
        ::raft_engine::Error::Protobuf(err) => {
            BackendError::Corruption(format!("raft-engine protobuf: {err}"))
        }
        ::raft_engine::Error::TryAgain(s) => {
            BackendError::Other(format!("raft-engine try-again: {s}"))
        }
        ::raft_engine::Error::EntryCompacted => {
            BackendError::InvalidRange("entry already compacted")
        }
        ::raft_engine::Error::EntryNotFound => BackendError::InvalidRange("entry not found"),
        ::raft_engine::Error::Full => BackendError::Other("raft-engine: log batch full".to_owned()),
        ::raft_engine::Error::Other(err) => BackendError::Other(format!("raft-engine: {err}")),
    }
}

/// Map a `tokio::task::JoinError` onto [`BackendError`]. Matches the
/// redb-side shape so operators see the same rendered prefix.
fn map_join_error(e: &tokio::task::JoinError) -> BackendError {
    BackendError::Other(format!("spawn_blocking join: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    // The async tests that exercise `append` use these; they are
    // gated `#[cfg(not(madsim))]` below because `spawn_blocking` is
    // not supported in the simulator. Mirror that gate on the imports
    // so the madsim build doesn't emit `unused_imports`.
    #[cfg(not(madsim))]
    use crate::backend::RaftEntryType;
    #[cfg(not(madsim))]
    use bytes::Bytes;
    use tempfile::TempDir;

    fn open_fresh() -> (RaftEngineLogStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let cfg = RaftEngineConfig::new(dir.path().to_path_buf());
        let store = RaftEngineLogStore::open(cfg).unwrap();
        (store, dir)
    }

    #[test]
    fn empty_store_indices_are_zero() {
        let (store, _dir) = open_fresh();
        assert_eq!(store.last_index().unwrap(), 0);
        assert_eq!(store.first_index().unwrap(), 0);
        assert_eq!(store.hard_state().unwrap(), HardState::default());
        store.close().unwrap();
    }

    #[test]
    fn map_engine_error_preserves_io_kind() {
        let e = ::raft_engine::Error::Io(std::io::Error::other("disk boom"));
        match map_engine_error(e) {
            BackendError::Io(inner) => {
                assert!(format!("{inner}").contains("disk boom"));
            }
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn map_engine_error_classifies_corruption() {
        let e = ::raft_engine::Error::Corruption("crc mismatch".into());
        match map_engine_error(e) {
            BackendError::Corruption(s) => assert_eq!(s, "crc mismatch"),
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    #[test]
    fn map_engine_error_classifies_entry_compacted_as_invalid_range() {
        let e = ::raft_engine::Error::EntryCompacted;
        match map_engine_error(e) {
            BackendError::InvalidRange(_) => {}
            other => panic!("expected InvalidRange, got {other:?}"),
        }
    }

    // The async tests below drive `append` / `save_hard_state`, which
    // route through `tokio::task::spawn_blocking`. Under `--cfg
    // madsim`, the simulator refuses to spawn OS threads; the full
    // async surface is exercised by `tests/raft_engine_logstore.rs`
    // (gated `#![cfg(not(madsim))]`) and the madsim-side smoke lives
    // in `tests/raft_engine_madsim_smoke.rs`.
    #[cfg(not(madsim))]
    #[tokio::test]
    async fn append_empty_is_noop() {
        let (store, _dir) = open_fresh();
        store.append(&[]).await.unwrap();
        assert_eq!(store.last_index().unwrap(), 0);
        store.close().unwrap();
    }

    #[cfg(not(madsim))]
    #[tokio::test]
    async fn append_rejects_non_consecutive() {
        let (store, _dir) = open_fresh();
        let bad = RaftEntry::new(
            5,
            1,
            RaftEntryType::Normal,
            Bytes::from_static(b"x"),
            Bytes::new(),
        );
        let err = store.append(std::slice::from_ref(&bad)).await.unwrap_err();
        match err {
            BackendError::Other(msg) => {
                assert!(msg.contains("expected index 1"), "got: {msg}");
                assert!(msg.contains("got 5"), "got: {msg}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
        store.close().unwrap();
    }

    #[cfg(not(madsim))]
    #[tokio::test]
    async fn closed_store_returns_closed_error() {
        let (store, _dir) = open_fresh();
        store.close().unwrap();
        let err = store.last_index().unwrap_err();
        assert!(matches!(err, BackendError::Closed), "got {err:?}");
        // close is idempotent.
        store.close().unwrap();
    }
}
