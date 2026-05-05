//! User-facing MVCC store (L844).
//!
//! [`MvccStore`] wraps a [`mango_storage::Backend`] with the
//! etcd-shape `KV` API. This commit lands the struct skeleton plus
//! [`MvccStore::open`] and [`MvccStore::current_revision`]; `Put` /
//! `Range` / `DeleteRange` / `Txn` / `Compact` arrive in subsequent
//! commits per the L844 plan §8 commit sequence.
//!
//! # Locking model
//!
//! Single-writer / multi-reader, mirroring Raft's serial apply.
//!
//! - `writer: tokio::sync::Mutex<WriterState>` — held for the
//!   entire write op (`Put` / `DeleteRange` / `Txn` / `Compact`).
//!   Async-aware mutex so the guard is `Send` across
//!   `commit_batch().await` (`parking_lot` guards are `!Send`).
//! - `index: ShardedKeyIndex` — own per-shard `parking_lot::RwLock`
//!   s; reads parallel with writes.
//! - `keys_in_order: parking_lot::RwLock<BTreeMap<...>>` — ordered
//!   live-key set used by `Range`. **No `.await` is held under
//!   this lock.** `BTreeMap` (not `BTreeSet`) so a future watch
//!   cache can extend the value side without re-typing.
//! - `snapshot: arc_swap::ArcSwap<Snapshot>` — atomically
//!   published `(rev, compacted)` pair. Replaces the prior pair
//!   of independent `AtomicI64`s (L846). Every successful writer
//!   builds a new `Arc<Snapshot>` and swaps it in under the
//!   `writer` mutex; readers take one `load_full()` and observe
//!   a coherent pair. See [`crate::store::snapshot`] for the
//!   `load_full()` vs `load()` discipline.
//!
//! # Lock ordering
//!
//! `writer` → `keys_in_order` (write) → `index` shard locks. Never
//! the reverse. The `Range` path takes only `keys_in_order` (read)
//! → `index` shard locks (read), no `writer` involvement.
//!
//! # Type parameter
//!
//! `B: Backend` is generic so tests run against `InMemBackend` and
//! production uses `RedbBackend`. `Backend` is not object-safe;
//! `dyn Backend` does not exist (use `MvccStore<impl Backend>`
//! instead).

pub mod lease;
pub mod range;
pub mod snapshot;
pub mod txn;
mod writer;

pub use lease::LeaseId;
pub use range::{KeyValue, RangeRequest, RangeResult};
pub use snapshot::Snapshot;
pub use txn::{Compare, CompareOp, RequestOp, ResponseOp, TxnRequest, TxnResponse};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use bytes::Bytes;
use mango_storage::{Backend, ReadSnapshot, WriteBatch};

use crate::bucket::{register, KEY_BUCKET_ID};
use crate::encoding::{decode_key, encode_key, KeyKind};
use crate::error::{MvccError, OpenError};
use crate::key_history::{KeyAtRev, KeyEventKind, KeyHistoryError};
use crate::revision::Revision;
use crate::sharded_key_index::{KeyIndexError, ShardedKeyIndex};
use crate::watchable_store::{WatchEvent, WatchEventKind, WriteObserver};

use self::writer::WriterState;

/// Best-effort cap on the number of pre-existing key-bucket
/// entries reported in [`OpenError::NonEmptyBackend::found_revs`].
/// Higher values pay an iteration cost on a non-empty backend that
/// the caller is going to discard anyway.
const NON_EMPTY_PROBE_CAP: u64 = 1024;

/// Upper-bound sentinel for the `KEY_BUCKET_ID` emptiness probe.
/// MVCC encoded keys are 17 (`Put`) or 18 (`Tombstone`) bytes (see
/// `crate::encoding`); a 32-byte all-`0xFF` slice is strictly
/// greater than any 18-byte sequence (longer prefix wins on byte
/// comparison), so `[start = &[], end = NON_EMPTY_PROBE_END)`
/// covers every valid encoded key. The Backend `range` semantic is
/// half-open `[start, end)` with no infinity sentinel; without an
/// explicit upper bound the probe would be empty.
const NON_EMPTY_PROBE_END: &[u8] = &[0xFF; 32];

/// On-disk value bytes written for a tombstone entry.
///
/// Tombstones are identified entirely by the `KeyKind::Tombstone`
/// marker byte at the end of the encoded **key**; the value is
/// inert. The backend rejects empty values
/// (`mango_storage::redb::batch::EMPTY_VALUE_ERROR`, parity with
/// bbolt's `ErrValueNil`) so we write a single zero byte. The
/// constant is documented here so a future Range-on-tombstone path
/// or a dump tool can recognise the sentinel.
const TOMBSTONE_VALUE: &[u8] = &[0u8];

/// One entry in a [`MvccStore::txn`] mutating-branch write plan.
///
/// Built in the first pass over the chosen branch and consumed in
/// the commit / in-mem-update / response-building passes. One
/// entry per branch op, in branch order. Only `Put` and `Delete`
/// produce physical writes; `Read` is a placeholder so the plan
/// stays index-aligned with the branch.
enum OpPlan {
    /// Placeholder for [`RequestOp::Range`]. No physical write;
    /// response is computed in the third pass.
    Read,
    /// One sub-allocated put. `rev` is `(txn_main, sub)`.
    Put {
        /// User key (cloned from `RequestOp::Put.key` so the
        /// in-mem update pass doesn't borrow from `req`).
        key: Vec<u8>,
        /// Value bytes (cloned for the same reason).
        value: Vec<u8>,
        /// Allocated revision for this put.
        rev: Revision,
        /// Pre-mutation prev-kv for this put: the live version of
        /// `key` immediately before this op runs (or `None` if
        /// the key was absent / tombstoned). Captured BEFORE any
        /// in-flight or persisted index mutation, so emit-side
        /// consumers see the right value even when an earlier op
        /// in the same txn already overwrote `key`.
        ///
        /// Phase 3 plan §4.6 (ROADMAP.md:863): emitted as-is by
        /// [`emit_txn_events`].
        prev: Option<KeyValue>,
    },
    /// Zero or more sub-allocated tombstones for a single
    /// [`RequestOp::DeleteRange`]. Empty when no live keys
    /// matched (the op contributes no physical writes).
    Delete {
        /// One [`Tombstone`] per matched key, in match order.
        tombs: Vec<Tombstone>,
    },
}

/// One matched key inside an [`OpPlan::Delete`] entry.
///
/// Phase 3 plan §4.6 (ROADMAP.md:863): carries the pre-mutation
/// `prev` [`KeyValue`] so the watch emit path can publish a
/// `WatchEvent::Delete { prev: Some(...) }` without re-querying a
/// post-mutation index. Tombstones are only emitted for live keys,
/// so `prev` is unconditionally present.
struct Tombstone {
    /// Matched user key.
    key: Box<[u8]>,
    /// Allocated revision for this tombstone (`(txn_main, sub)`).
    rev: Revision,
    /// Pre-mutation prev-kv: the live version of `key`
    /// immediately before this tombstone runs. Captured BEFORE
    /// any index mutation by [`MvccStore::delete_range`] /
    /// [`MvccStore::build_txn_plan`] and emitted **as-is** by
    /// [`emit_txn_events`] — no second snapshot or index lookup
    /// runs on the writer hot path.
    prev: KeyValue,
}

/// In-flight per-txn key state used by [`MvccStore::build_txn_plan`]
/// to resolve `prev_kv` against the most-recent in-flight op rather
/// than the (post-mutation) index. Phase 3 plan §4.6.
///
/// - `Some(Some(kv))` — the key is live with this in-flight value
///   (a same-txn `Put` overwrote the prior version).
/// - `Some(None)` — the key was tombstoned earlier in this txn.
/// - absent — the key has not been touched by any prior op in this
///   txn; resolve `prev` against the pre-txn index via
///   [`MvccStore::compute_prev_kv_strict`].
///
/// Consulted **only** for `prev_kv` resolution. The `DeleteRange`
/// match-set is computed against the pre-txn `keys_in_order`
/// snapshot, exactly as before — same-branch put-then-delete-of-
/// new-key continues to be a no-op on the second op (existing
/// documented contract; see [`MvccStore::txn`]).
struct TentativeState {
    live: HashMap<Box<[u8]>, Option<KeyValue>>,
}

/// Increment a per-txn `sub` allocator with overflow surfaced as
/// [`MvccError::Internal`]. Workspace `arithmetic_side_effects`
/// deny rules out `+ 1` directly.
fn checked_add_sub(sub: i64) -> Result<i64, MvccError> {
    sub.checked_add(1).ok_or(MvccError::Internal {
        context: "txn sub overflow",
    })
}

/// Increment the per-txn physical-write counter with overflow
/// surfaced as [`MvccError::Internal`].
fn checked_add_total(total: usize) -> Result<usize, MvccError> {
    total.checked_add(1).ok_or(MvccError::Internal {
        context: "txn physical write count overflow",
    })
}

/// Walk a built [`OpPlan`] slice and append one [`WatchEvent`] per
/// physical write into `buf`. Phase 3 plan §4.2 (ROADMAP.md:862).
///
/// Per-variant mapping:
///
/// - [`OpPlan::Read`] — no event (read-only ops are not observable).
/// - [`OpPlan::Put`] — one [`WatchEventKind::Put`] event with
///   `revision = rev` from the plan entry. `prev` is the captured
///   pre-mutation [`KeyValue`] (see [`OpPlan::Put.prev`]) — `None`
///   if the key was absent or tombstoned at the moment the plan was
///   built.
/// - [`OpPlan::Delete`] — one [`WatchEventKind::Delete`] event per
///   [`Tombstone`] in `tombs`, in match order; `value` is empty.
///   `prev` is the tombstone's captured pre-mutation [`KeyValue`]
///   wrapped in `Some` — tombstones are only emitted for live keys,
///   so the prev-kv is unconditionally present.
///
/// Phase 3 plan §4.6 / §6 (ROADMAP.md:863): the captured prev-kvs
/// are emitted *as-is* — no second backend or index lookup runs on
/// the writer's hot path. INVARIANT X is asserted at the call site
/// in [`MvccStore::commit_txn_batch`].
fn emit_txn_events(plan: &[OpPlan], buf: &mut Vec<WatchEvent>) {
    for entry in plan {
        match entry {
            OpPlan::Read => {}
            OpPlan::Put {
                key,
                value,
                rev,
                prev,
            } => {
                buf.push(WatchEvent {
                    kind: WatchEventKind::Put,
                    key: Bytes::copy_from_slice(key),
                    value: Bytes::copy_from_slice(value),
                    prev: prev.clone(),
                    revision: *rev,
                });
            }
            OpPlan::Delete { tombs } => {
                for tomb in tombs {
                    buf.push(WatchEvent {
                        kind: WatchEventKind::Delete,
                        key: Bytes::copy_from_slice(&tomb.key),
                        value: Bytes::new(),
                        prev: Some(tomb.prev.clone()),
                        revision: tomb.rev,
                    });
                }
            }
        }
    }
}

/// User-facing MVCC store.
///
/// See module docs for locking model and lock ordering.
///
/// **L844 only opens against an empty backend** —
/// [`Self::open`] returns [`OpenError::NonEmptyBackend`] otherwise.
/// Restart-from-disk recovery lands in L852.
pub struct MvccStore<B: Backend> {
    /// Underlying storage backend. Owned (not `Arc`) so callers
    /// place this struct behind their own `Arc` if they want
    /// shared access.
    backend: B,
    /// Per-key revision history. Point-lookup; sharded.
    index: ShardedKeyIndex,
    /// Ordered live-key set, used by `Range`. The L846 substrate
    /// (a future commit may wrap the ordered key set in
    /// `arc_swap::ArcSwap<...>` — note: `ArcSwap<T>` already wraps
    /// `Arc<T>`, so the spelling is `ArcSwap<...>`, not
    /// `ArcSwap<Arc<...>>`), not a stopgap. Map (not Set) so the
    /// watch cache can extend
    /// the value side without a re-typing migration. The
    /// `zero_sized_map_values` clippy lint flags the `()` value
    /// type — silenced here because the type is forward-design
    /// for L859's watch cache, per the L844 plan §4.1.
    #[allow(clippy::zero_sized_map_values)]
    keys_in_order: parking_lot::RwLock<BTreeMap<Box<[u8]>, ()>>,
    /// Writer serialization. Async-aware mutex because the guard
    /// is held across `commit_batch().await` (`parking_lot` guards
    /// are `!Send`).
    writer: tokio::sync::Mutex<WriterState>,
    /// Atomically-published `(rev, compacted)` pair (L846).
    /// Replaces the prior `current_main` + `compacted` atomic
    /// fields. Every successful writer (`put`, `delete_range`,
    /// mutating `txn`, `compact`) builds a new `Arc<Snapshot>`
    /// and `store`s it into this slot before returning, while
    /// holding the `writer` mutex — so the
    /// `load_full -> mutate -> store` pattern is a CAS in spirit
    /// even though `ArcSwap::store` itself is not.
    snapshot: ArcSwap<Snapshot>,
    /// Single-occupancy observer slot for the Phase 3 Watch
    /// surface (ROADMAP.md:862). Empty by default (`Arc::new(None)`
    /// at `open` time); callers attach via
    /// [`Self::attach_observer`]. The writer hot path will
    /// dispatch to this observer in commit 2 of the Phase 3 plan;
    /// this commit lands the slot and the trait only — `put` /
    /// `delete_range` / `txn` are unchanged.
    ///
    /// **Why the `Option<Arc<dyn …>>` wrapper.** `arc_swap`'s
    /// `RefCnt` impls require the inner type to be `Sized`, so
    /// `ArcSwapOption<dyn WriteObserver>` does not compile (the
    /// trait object is unsized). Wrapping in `Option<Arc<…>>`
    /// behind a single `Arc` keeps the slot `Sized` (an
    /// `Option<Arc<…>>` is two pointer-sized words) and gives
    /// readers one `ArcSwap::load` plus a single `match` to test
    /// presence. The writer hot path's no-observer cost is one
    /// uncontended atomic load + one branch — bench-validated
    /// 5 ns range per Phase 3 plan §5.
    observer: ArcSwap<Option<Arc<dyn WriteObserver>>>,
    /// CAS gate guarding [`Self::observer`] against double-attach.
    /// `attach_observer` performs an `AcqRel` `compare_exchange`
    /// on this flag; only the winning thread proceeds to publish
    /// the observer. Avoids a TOCTOU window across
    /// concurrent `attach_observer` calls — single-shot
    /// callers (the typical `WatchableStore::new` path) pay one
    /// uncontended atomic, contended callers see deterministic
    /// `Err`. The winning thread proceeds to [`ArcSwap::store`]
    /// on [`Self::observer`].
    observer_attached: AtomicBool,
}

impl<B: Backend> std::fmt::Debug for MvccStore<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snap = self.snapshot.load();
        f.debug_struct("MvccStore")
            .field("rev", &snap.rev)
            .field("compacted", &snap.compacted)
            .finish_non_exhaustive()
    }
}

impl<B: Backend> MvccStore<B> {
    /// Open a store against `backend`.
    ///
    /// Registers the MVCC buckets (`key`, `key_index`) if missing,
    /// then probes the `key` bucket for pre-existing data. L844
    /// only supports opening against an empty backend; recovery
    /// lands in L852.
    ///
    /// # Errors
    ///
    /// - [`OpenError::Backend`] if bucket registration or snapshot
    ///   acquisition fails.
    /// - [`OpenError::NonEmptyBackend`] if any keys exist in the
    ///   `key` bucket. `found_revs` is best-effort, capped at
    ///   1024.
    pub fn open(backend: B) -> Result<Self, OpenError> {
        register(&backend)?;

        let snap = backend.snapshot()?;
        let mut iter = snap.range(KEY_BUCKET_ID, &[], NON_EMPTY_PROBE_END)?;
        let mut found: u64 = 0;
        for item in iter.by_ref() {
            // Surface backend-side iteration errors as Backend.
            item?;
            found = found.saturating_add(1);
            if found >= NON_EMPTY_PROBE_CAP {
                break;
            }
        }
        drop(iter);
        drop(snap);
        if found > 0 {
            return Err(OpenError::NonEmptyBackend { found_revs: found });
        }

        // `keys_in_order` carries `BTreeMap<Box<[u8]>, ()>`; clippy's
        // `zero_sized_map_values` flags this constructor too. Same
        // suppression rationale as the field decl: forward-design
        // for L859's watch cache.
        #[allow(clippy::zero_sized_map_values)]
        let keys_in_order = parking_lot::RwLock::new(BTreeMap::new());
        Ok(Self {
            backend,
            index: ShardedKeyIndex::new(),
            keys_in_order,
            writer: tokio::sync::Mutex::new(WriterState::new()),
            snapshot: ArcSwap::from_pointee(Snapshot::empty()),
            observer: ArcSwap::new(Arc::new(None)),
            observer_attached: AtomicBool::new(false),
        })
    }

    /// Attach a [`WriteObserver`] to the store. Single-occupancy:
    /// the second call returns
    /// [`MvccError::Internal`] without replacing the existing
    /// observer.
    ///
    /// # Concurrency
    ///
    /// The slot is gated by an [`AtomicBool`] CAS
    /// (`compare_exchange` with `AcqRel` on success / `Acquire` on
    /// failure). The CAS wins **before** the `ArcSwap::store`, so
    /// concurrent callers see deterministic outcomes:
    ///
    /// - First caller: CAS swings `false → true`, store proceeds.
    /// - All later callers: CAS observes `true`, returns `Err`
    ///   without touching the slot.
    ///
    /// The `Acquire` on failure ensures the failing caller's read
    /// of `observer_attached` happens-after the winner's
    /// `Release` store, which in turn happens-before the
    /// winner's `ArcSwap::store`. Anyone observing `Err(...)`
    /// therefore also sees a fully-published observer in the slot.
    ///
    /// # Errors
    ///
    /// - [`MvccError::Internal`] if the slot is already occupied
    ///   (`context: "observer slot already occupied"`).
    pub fn attach_observer(&self, obs: Arc<dyn WriteObserver>) -> Result<(), MvccError> {
        self.observer_attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| MvccError::Internal {
                context: "observer slot already occupied",
            })?;
        self.observer.store(Arc::new(Some(obs)));
        Ok(())
    }

    /// Test-only accessor for the observer slot. Used by the
    /// `attach_observer` smoke test in this module and by the
    /// commit-2 dispatch tests; does not appear in the public API
    /// surface.
    #[cfg(test)]
    pub(crate) fn observer_is_attached(&self) -> bool {
        self.observer.load().is_some()
    }

    /// Dispatch the writer's per-op `emit_buf` to the attached
    /// observer (if any). Phase 3 plan §4.2 (ROADMAP.md:862).
    ///
    /// **Caller contract.** Must be invoked under the writer
    /// `tokio::sync::Mutex` (same lock that owns `emit_buf`) AND
    /// **after** `self.snapshot.store()` has published the new
    /// revision. The dispatch-after-store ordering is load-bearing
    /// for the `start_rev` race-free contract — a watcher that
    /// receives an event for revision `R` is guaranteed to see
    /// `current_revision() >= R` from any later read (Phase 3 plan
    /// §4.2 / §11 test #11).
    ///
    /// `emit_buf` is unconditionally cleared at the end so the
    /// allocation amortizes across writes. The no-observer hot
    /// path is one `Vec::is_empty()` branch + one `ArcSwap::load`
    /// + one `match` + one `Vec::clear`.
    ///
    /// INVARIANT W (load-bearing — see `watchable_store.rs` §4.4
    /// catch-up race proof, ROADMAP.md:863):
    /// `snapshot.store(new_snap)` and the `obs.on_apply(...)`
    /// call below MUST run under the same writer-mutex
    /// acquisition with NO `.await` point between them. The
    /// catch-up `promote_to_synced` race-proof depends on this
    /// sequencing. Splitting these calls (e.g. by adding an
    /// `.await` between them, or by moving `snapshot.store` out
    /// of the writer-mutex critical section) BREAKS the
    /// catch-up correctness proof. If you must change this,
    /// re-derive the proof.
    fn dispatch_observer(&self, emit_buf: &mut Vec<WatchEvent>, at_main: i64) {
        if !emit_buf.is_empty() {
            let observer = self.observer.load();
            if let Some(obs) = (**observer).as_ref() {
                obs.on_apply(emit_buf, at_main);
            }
        }
        emit_buf.clear();
    }

    /// Highest fully-published revision. Returns `0` on a fresh
    /// store.
    ///
    /// Reads through the published [`Snapshot`]; the `Acquire`
    /// pair is provided by `arc_swap::ArcSwap::load`.
    #[must_use]
    pub fn current_revision(&self) -> i64 {
        // `load()` returns a Guard; we copy out `rev` (i64) and
        // drop immediately. Long-lived reader paths use
        // `current_snapshot()` instead.
        self.snapshot.load().rev
    }

    /// Snapshot of the current `(rev, compacted)` pair, as one
    /// `Arc<Snapshot>`.
    ///
    /// Use this when:
    ///
    /// - You need to read more than one field from the snapshot
    ///   (so the pair stays coherent).
    /// - You're holding the value across a long scan (>1000 keys)
    ///   or across `.await` points.
    ///
    /// Use [`Self::current_revision`] for one-shot revision reads
    /// — it goes through `ArcSwap::load` (a `Guard`), which
    /// avoids the refcount-bump but expects a short scope.
    ///
    /// See [`crate::store::snapshot`] for the discipline.
    #[must_use]
    pub fn current_snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    /// Borrow the underlying backend. Used by writer impls in
    /// subsequent commits; left `pub(crate)` to avoid leaking the
    /// backend into callers (they passed it in).
    #[allow(dead_code)]
    pub(crate) fn backend(&self) -> &B {
        &self.backend
    }

    /// Acquire a coherent `(mvcc_snap, backend_snap)` pair under
    /// the writer mutex.
    ///
    /// Phase 3 plan §4.5 (ROADMAP.md:863). Both snapshots observe
    /// the same point in time with respect to concurrent writes —
    /// most importantly [`Self::compact`], whose on-disk delete
    /// (`commit_batch`) and `snapshot.store(new_floor)` happen
    /// inside its own writer-lock hold. Acquiring the writer lock
    /// here means the catch-up driver either sees the pre-compact
    /// state or the post-compact state, never a torn pair.
    ///
    /// The Mvcc snapshot's `rev` is the upper bound for the
    /// catch-up scan; `compacted` is the floor. Caller reads both
    /// from this single returned `Arc<Snapshot>` (rust-expert nit
    /// 3 of v3 review: don't re-load `self.snapshot` after the
    /// pair is taken).
    ///
    /// Cost: one `tokio::sync::Mutex` acquisition + one
    /// [`ArcSwap::load_full`] + one [`Backend::snapshot`]. The
    /// writer is stalled for the duration of those three steps;
    /// on redb the backend snapshot is a single `begin_read()` in
    /// the µs range, so the stall is bounded. Catch-up driver
    /// caps total scan attempts at `MAX_CATCHUP_ATTEMPTS` so the
    /// total stall budget per watcher is bounded.
    ///
    /// # Errors
    ///
    /// - [`MvccError::Backend`] if `Backend::snapshot()` fails.
    pub(crate) async fn snapshot_pair_under_writer(
        &self,
    ) -> Result<(Arc<Snapshot>, B::Snapshot), MvccError> {
        let _guard = self.writer.lock().await;
        let mvcc = self.snapshot.load_full();
        let be = self.backend.snapshot()?;
        Ok((mvcc, be))
    }

    /// Pre-mutation prev-kv lookup: the live version of `key`
    /// strictly before `at_rev`, materialised as a [`KeyValue`]
    /// against the on-disk snapshot `snap_be`.
    ///
    /// Phase 3 plan §3.4 / §4.6 (ROADMAP.md:863). Walks the index
    /// via [`ShardedKeyIndex::get_strict_lt`] (which crosses
    /// generations, skipping the closing tombstone), then
    /// resolves the value bytes from `snap_be`. Returns `None` if
    /// no prior live revision exists in any generation.
    ///
    /// This is the building block for `prev_kv` population on the
    /// writer hot path (`put`, `delete_range`, `txn`) and on the
    /// catch-up scan path. The caller threads ONE
    /// [`Backend::snapshot`] through every prev-kv computation in
    /// a single writer call, amortising the snapshot cost across
    /// `N` matched keys.
    ///
    /// # Errors
    ///
    /// - [`MvccError::KeyIndex`] if the index lookup fails for a
    ///   reason other than `KeyNotFound` /
    ///   `History(RevisionNotFound)` (those two are the absent
    ///   path — not errors here).
    /// - [`MvccError::Backend`] if the on-disk value fetch fails.
    pub(crate) fn compute_prev_kv_strict(
        &self,
        key: &[u8],
        at_rev: Revision,
        snap_be: &B::Snapshot,
    ) -> Result<Option<KeyValue>, MvccError> {
        let at = match self.index.get_strict_lt(key, at_rev) {
            Ok(Some(at)) => at,
            // Both "no entry" and "no strict-lt predecessor" are
            // the absent path — return None, not Err.
            Ok(None)
            | Err(
                KeyIndexError::KeyNotFound
                | KeyIndexError::History(KeyHistoryError::RevisionNotFound),
            ) => return Ok(None),
            Err(e) => return Err(MvccError::KeyIndex(e)),
        };
        let enc = encode_key(at.modified, KeyKind::Put);
        let value = snap_be
            .get(KEY_BUCKET_ID, enc.as_bytes())?
            .unwrap_or_default();
        Ok(Some(KeyValue {
            key: Bytes::copy_from_slice(key),
            create_revision: at.created,
            mod_revision: at.modified,
            version: at.version,
            value,
            lease: None,
        }))
    }

    /// Catch-up scan over `[range_start, range_end)` for revisions
    /// `rev.main() in [lo, hi]`, against the writer-locked
    /// `(mvcc_snap, snap_be)` pair.
    ///
    /// Phase 3 plan §4.5 (ROADMAP.md:863). Used by the unsynced
    /// watcher's catch-up driver to replay history events between
    /// `start_rev` (the watcher's requested floor) and `mvcc_snap.rev`
    /// (the upper bound captured under the writer mutex).
    ///
    /// `range_end.is_empty()` selects the single-key case.
    ///
    /// Events are emitted in `(rev.main, rev.sub)` ascending order
    /// across the matched key set — etcd parity. Within one key the
    /// per-key history walk already yields revs in ascending order
    /// (`ShardedKeyIndex::events_in_range`); across keys we sort the
    /// merged list once at the end.
    ///
    /// `prev_kv` is computed for every event via
    /// [`Self::compute_prev_kv_strict`] against the same `snap_be`,
    /// matching the writer hot path's contract: `Put` events carry
    /// `Some(...)` only when a strictly-prior live version exists in
    /// any generation; `Delete` events always carry `Some(...)`
    /// (tombstones close a live generation).
    ///
    /// # Compaction guard
    ///
    /// Returns [`MvccError::Compacted`] when `mvcc_snap.compacted >= lo`.
    /// The driver maps this to a terminal
    /// `DisconnectReason::Compacted { floor }`.
    ///
    /// # Empty / inverted range
    ///
    /// Returns `Ok(Vec::new())` when `lo > hi`. The caller treats
    /// this as "no events to send this iteration; check for promote".
    ///
    /// # Errors
    ///
    /// - [`MvccError::Compacted`] if the floor advanced past `lo`.
    /// - [`MvccError::KeyIndex`] / [`MvccError::Backend`] surfaced
    ///   verbatim — the driver maps these to a terminal
    ///   `DisconnectReason::Internal` so the watcher sees a typed
    ///   reason rather than a silent abort.
    pub(crate) fn catchup_scan(
        &self,
        range_start: &[u8],
        range_end: &[u8],
        lo: i64,
        hi: i64,
        mvcc_snap: &Snapshot,
        snap_be: &B::Snapshot,
    ) -> Result<Vec<WatchEvent>, MvccError> {
        if lo > hi {
            return Ok(Vec::new());
        }
        if mvcc_snap.compacted >= lo {
            return Err(MvccError::Compacted {
                requested: lo,
                floor: mvcc_snap.compacted,
            });
        }

        // Snapshot the matched-keys list. Drop the read lock before
        // any backend / index work so writers are not blocked across
        // the per-key event walk.
        let matched: Vec<Box<[u8]>> = {
            let keys = self.keys_in_order.read();
            if range_end.is_empty() {
                if keys.contains_key(range_start) {
                    vec![range_start.into()]
                } else {
                    Vec::new()
                }
            } else {
                keys.range::<[u8], _>((Bound::Included(range_start), Bound::Excluded(range_end)))
                    .map(|(k, ())| k.clone())
                    .collect()
            }
        };

        let mut events: Vec<WatchEvent> = Vec::new();
        for key in &matched {
            let key_events = self.index.events_in_range(key, lo, hi);
            for (rev, kind) in key_events {
                let watch_kind = match kind {
                    KeyEventKind::Put => WatchEventKind::Put,
                    KeyEventKind::Tombstone => WatchEventKind::Delete,
                };
                let value = match watch_kind {
                    WatchEventKind::Put => {
                        let enc = encode_key(rev, KeyKind::Put);
                        snap_be
                            .get(KEY_BUCKET_ID, enc.as_bytes())?
                            .unwrap_or_default()
                    }
                    WatchEventKind::Delete => Bytes::new(),
                };
                let prev = self.compute_prev_kv_strict(key, rev, snap_be)?;
                events.push(WatchEvent {
                    kind: watch_kind,
                    key: Bytes::copy_from_slice(key),
                    value,
                    prev,
                    revision: rev,
                });
            }
        }

        // Cross-key merge: ascending by (main, sub). Within one key
        // the per-key walk already produces this order; across keys
        // we resort the combined list. `sort_by` is stable so equal
        // revisions (impossible by construction — a single rev maps
        // to one key) keep their input order.
        events.sort_by(|a, b| a.revision.cmp(&b.revision));
        Ok(events)
    }

    /// Single-key put.
    ///
    /// Allocates one `main` revision (sub = 0) and persists the
    /// `(key, value)` pair under the encoded `(rev, KeyKind::Put)`
    /// on-disk key. On success, the returned [`Revision`] is the
    /// allocation point — `current_revision()` will reflect it
    /// after this method returns.
    ///
    /// # Ordering
    ///
    /// 1. Acquire writer lock.
    /// 2. Allocate `rev = (state.next_main, 0)`.
    /// 3. Begin batch, put on-disk, commit (no fsync — Raft owns
    ///    durability at the WAL above us).
    /// 4. Insert `key` into `keys_in_order` under its write lock.
    /// 5. `index.put(key, rev)` (structurally infallible under the
    ///    writer-lock invariant; surfaces as
    ///    [`MvccError::Internal`] if violated).
    /// 6. Bump `next_main` (checked).
    /// 7. Publish a fresh `Arc<Snapshot>` via
    ///    [`arc_swap::ArcSwap::store`] so readers observe the new
    ///    head with the existing compaction floor (L846).
    ///
    /// # Errors
    ///
    /// - [`MvccError::Backend`] from `begin_batch` /
    ///   `commit_batch` / `WriteBatch::put`.
    /// - [`MvccError::Internal`] if `next_main` would overflow
    ///   `i64::MAX`, or if the in-memory index rejects a put that
    ///   the writer-lock invariant says it must accept (a Mango
    ///   bug — see plan §5.2 review item S2; surfaced rather than
    ///   panicked).
    pub async fn put(&self, key: &[u8], value: &[u8]) -> Result<Revision, MvccError> {
        let mut state = self.writer.lock().await;
        let rev = Revision::new(state.next_main, 0);

        // Phase 3 plan §4.6 (ROADMAP.md:863): capture pre-mutation
        // prev-kv BEFORE `index.put` runs. ONE backend snapshot is
        // taken per writer call; cost amortises over the read-then-
        // emit path. `None` if the key was absent or tombstoned.
        let prev_kv = {
            let snap_be = self.backend.snapshot()?;
            self.compute_prev_kv_strict(key, rev, &snap_be)?
        };

        let mut batch = self.backend.begin_batch()?;
        let encoded = encode_key(rev, KeyKind::Put);
        batch.put(KEY_BUCKET_ID, encoded.as_bytes(), value)?;
        // No fsync — durability is Raft's WAL contract above this
        // layer (plan §5.2 step 5).
        let _ = self.backend.commit_batch(batch, false).await?;

        // No `.await` is held under either of the in-memory locks
        // below. Ordering is **index first, then `keys_in_order`**
        // (rust-expert PR #75 review R1): a concurrent reader
        // observing the new `Snapshot` (published via `ArcSwap` at
        // the end of this fn) can scan `keys_in_order` and probe the
        // index. If we set `keys_in_order` first, there is a
        // sub-microsecond window where a reader sees the new key
        // in the ordered set but `index.get` returns
        // `KeyNotFound` — Range surfaces that as
        // `MvccError::Internal { context: "index/keys_in_order
        // disagree" }`. Index first means a reader sees either
        // (a) neither — pre-Put state, or (b) index set,
        // `keys_in_order` not yet — Range scans `keys_in_order`
        // and never visits the new key, or (c) both — fully
        // committed.
        //
        // Structurally: `KeyHistory::put` rejects only with
        // `NonMonotonic`, but `state.next_main` is allocated under
        // the writer lock and increases monotonically across all
        // writes — so any `rev` we assign strictly exceeds every
        // prior `modified` for any key. If this returns `Err` the
        // invariant is broken (plan §5.2 review item S2).
        if let Err(_e) = self.index.put(key, rev) {
            return Err(MvccError::Internal {
                context: "index.put failed under monotonic invariant",
            });
        }
        {
            let mut keys = self.keys_in_order.write();
            // `BTreeMap::insert` is idempotent on identical-key
            // overwrites — the value is `()`. Repeated puts on the
            // same user key keep the entry exactly once.
            let _ = keys.insert(key.into(), ());
        }

        let next = state.next_main.checked_add(1).ok_or(MvccError::Internal {
            context: "next_main overflow",
        })?;
        state.next_main = next;

        // Phase 3 plan §4.2 (ROADMAP.md:862): synthesize the watch
        // event before snapshot publication so the buffer is ready
        // to dispatch immediately after `snapshot.store()`.
        //
        // INVARIANT X (Phase 3 plan §4.6 / §6, ROADMAP.md:863):
        // `prev_kv` was resolved above against a `Backend::snapshot`
        // taken before any index mutation; no second backend or
        // index lookup runs on the emit path.
        state.emit_buf.push(WatchEvent {
            kind: WatchEventKind::Put,
            key: Bytes::copy_from_slice(key),
            value: Bytes::copy_from_slice(value),
            prev: prev_kv,
            revision: rev,
        });

        // Publish the new snapshot under the writer mutex
        // (L846): atomic `(rev, compacted)` pair, ArcSwap's
        // `store` is the Release that pairs with readers'
        // `load`/`load_full` Acquire. `compacted` carries
        // forward unchanged — Put never advances the floor.
        let prev = self.snapshot.load_full();
        self.snapshot.store(Arc::new(Snapshot {
            rev: rev.main(),
            compacted: prev.compacted,
        }));

        // Dispatch AFTER snapshot.store(), still under writer
        // mutex (Phase 3 plan §4.2 — load-bearing for the
        // `start_rev` race-free contract).
        self.dispatch_observer(&mut state.emit_buf, rev.main());

        Ok(rev)
    }

    /// Test-only hook: reset the writer's `next_main` to a chosen
    /// value. Used by `put_index_invariant_violation_returns_internal_not_panic`
    /// to construct the impossible state the structural invariant
    /// rules out (plan §5.2 review item S2). Holding the writer
    /// lock here mirrors the production allocator path so any
    /// future reordering catches concurrent calls.
    #[cfg(test)]
    pub(crate) async fn set_next_main_for_test(&self, value: i64) {
        let mut state = self.writer.lock().await;
        state.next_main = value;
    }

    /// Range read at `req.revision` (or current head if `None`).
    ///
    /// Synchronous: `Range` performs zero writes, so the writer
    /// lock is never taken. A consistent snapshot is acquired from
    /// the backend; concurrent writers proceed against the next
    /// snapshot generation.
    ///
    /// # Semantics
    ///
    /// - `req.end.is_empty()` → single-key lookup (etcd parity).
    /// - Otherwise the half-open `[req.key, req.end)` slice of the
    ///   live-key set is matched.
    /// - `req.revision = Some(rev)` reads at `rev` exactly; `None`
    ///   resolves to the current head once on entry (plan §5.0
    ///   review item B1).
    /// - `count` reports total matches **ignoring `limit`** so the
    ///   caller can paginate (plan §4.3 review item M4). `more`
    ///   is `true` iff `limit` was hit.
    /// - `count_only` short-circuits both value fetch and
    ///   `KeyValue` construction (review item M5).
    ///
    /// # Errors
    ///
    /// - [`MvccError::Compacted`] if `rev < compacted_floor`
    ///   (strict `<`; the floor itself remains readable per etcd
    ///   parity, plan review item B1).
    /// - [`MvccError::FutureRevision`] if `rev > snap.rev`.
    /// - [`MvccError::InvalidRange`] if `req.end` is non-empty
    ///   and `req.end < req.key`. Equal bounds are allowed (yield
    ///   an empty result).
    /// - [`MvccError::Backend`] from snapshot acquisition or
    ///   value fetch.
    pub fn range(&self, req: RangeRequest) -> Result<RangeResult, MvccError> {
        // Destructure to move every field out — clippy's
        // `needless_pass_by_value` requires consumption when the
        // public signature is by-value, which is the ergonomic
        // choice (callers can pass `RangeRequest::default()`
        // inline). Using ` ..` would be wrong here because
        // `RangeRequest` is `#[non_exhaustive]` from outside the
        // crate but defined inside it — destructure by name.
        let RangeRequest {
            key: req_key,
            end: req_end,
            revision: req_revision,
            limit,
            keys_only,
            count_only,
        } = req;

        // L846: one snapshot load → coherent (rev, compacted)
        // pair. Held across the whole Range so the floor check
        // and the index walk see the same publish point.
        let snap = self.snapshot.load_full();
        let current = snap.rev;
        let rev = req_revision.unwrap_or(current);
        let floor = snap.compacted;

        if rev < floor {
            return Err(MvccError::Compacted {
                requested: rev,
                floor,
            });
        }
        if rev > current {
            return Err(MvccError::FutureRevision {
                requested: rev,
                current,
            });
        }
        if !req_end.is_empty() && req_end.as_slice() < req_key.as_slice() {
            return Err(MvccError::InvalidRange);
        }

        let snap = self.backend.snapshot()?;

        // Snapshot the matched user-key set under read-lock and
        // drop the guard before touching `index` or the backend
        // snapshot. Lock-ordering edge: read-lock comes first; we
        // never hold it across an `await` (this method is sync
        // anyway, but the discipline is the same as the writer
        // path documented at module level).
        let matched: Vec<Box<[u8]>> = {
            let keys = self.keys_in_order.read();
            if req_end.is_empty() {
                if keys.contains_key(req_key.as_slice()) {
                    vec![req_key.into_boxed_slice()]
                } else {
                    Vec::new()
                }
            } else {
                keys.range::<[u8], _>((
                    Bound::Included(req_key.as_slice()),
                    Bound::Excluded(req_end.as_slice()),
                ))
                .map(|(k, ())| k.clone())
                .collect()
            }
        };

        let mut kvs: Vec<KeyValue> = Vec::new();
        let mut count: u64 = 0;
        let mut more = false;

        for key in &matched {
            let at = match self.index.get(key, rev) {
                Ok(at) => at,
                // The key was tombstoned at-or-before `rev`. Skip
                // silently — etcd parity for a tombstoned key on
                // a Range read.
                Err(KeyIndexError::History(KeyHistoryError::RevisionNotFound)) => continue,
                // `keys_in_order` and `index` are mutated together
                // under the writer lock; a `KeyNotFound` here
                // would indicate a writer-invariant violation.
                // Surface as `Internal` rather than `Range` lying
                // about the shape (plan review item S2).
                Err(KeyIndexError::KeyNotFound) => {
                    return Err(MvccError::Internal {
                        context: "index/keys_in_order disagree on key presence",
                    });
                }
                Err(e) => return Err(MvccError::KeyIndex(e)),
            };

            // `count` is `u64`; per workspace
            // `arithmetic_side_effects` deny we use `checked_add`.
            // Overflowing 2^64 matches in a single Range call is
            // a structural impossibility; surface as `Internal`.
            count = count.checked_add(1).ok_or(MvccError::Internal {
                context: "range count overflow",
            })?;

            if count_only {
                continue;
            }

            let limit_hit = limit.is_some_and(|l| kvs.len() >= l);
            if limit_hit {
                more = true;
                // Continue counting matches — `count` reports
                // total matches ignoring `limit` (M4). No further
                // value fetches.
                continue;
            }

            let key_bytes = Bytes::copy_from_slice(key);
            let value = if keys_only {
                Bytes::new()
            } else {
                let enc = encode_key(at.modified, KeyKind::Put);
                snap.get(KEY_BUCKET_ID, enc.as_bytes())?.unwrap_or_default()
            };

            kvs.push(KeyValue {
                key: key_bytes,
                create_revision: at.created,
                mod_revision: at.modified,
                version: at.version,
                value,
                lease: None,
            });
        }

        Ok(RangeResult {
            kvs,
            more,
            count,
            header_revision: current,
        })
    }

    /// Tombstone every live key in `[key, end)`.
    ///
    /// Allocates **at most one** `main` revision: the empty match
    /// set is a no-op and does not advance the head (plan §5.4
    /// review item S3 — etcd parity with `mvcc/kvstore_txn.go`'s
    /// `DeleteRange` returning early on zero matches). When matches
    /// are present, every tombstone shares the allocated `main` and
    /// is assigned a unique sub starting at `0`, in `keys_in_order`
    /// iteration order.
    ///
    /// # Semantics
    ///
    /// - `end.is_empty()` → single-key delete (etcd parity).
    /// - Already-tombstoned-at-`current` keys are filtered out
    ///   before allocation; they are not counted, do not consume a
    ///   sub, and do not appear on disk.
    /// - Tombstoned keys remain in `keys_in_order` until
    ///   `Compact` reaps them — etcd retains them so a `Range` at
    ///   a pre-tombstone revision still sees the key (plan §5.4
    ///   review item R3).
    ///
    /// # Errors
    ///
    /// - [`MvccError::Backend`] from `begin_batch` /
    ///   `WriteBatch::put` / `commit_batch`.
    /// - [`MvccError::Internal`] if `next_main` would overflow,
    ///   if the per-key sub allocator overflows, or if
    ///   `index.tombstone` rejects a call that the writer-lock
    ///   invariant says it must accept.
    pub async fn delete_range(&self, key: &[u8], end: &[u8]) -> Result<(u64, Revision), MvccError> {
        let mut state = self.writer.lock().await;
        // L846: take one snapshot for the rev side. The compacted
        // floor stays unchanged across this op (DeleteRange does
        // not advance compaction); we re-read it at publish time.
        let current = self.snapshot.load().rev;

        // Step 3: candidate match set under the read lock; drop
        // the guard before any further work.
        let candidates: Vec<Box<[u8]>> = {
            let keys = self.keys_in_order.read();
            if end.is_empty() {
                if keys.contains_key(key) {
                    vec![key.into()]
                } else {
                    Vec::new()
                }
            } else {
                keys.range::<[u8], _>((Bound::Included(key), Bound::Excluded(end)))
                    .map(|(k, ())| k.clone())
                    .collect()
            }
        };

        // Step 4: filter to keys live at `current` and CAPTURE
        // their pre-mutation `KeyAtRev` so prev-kv resolution does
        // not have to re-query a post-mutation index. Phase 3
        // plan §4.6 (ROADMAP.md:863). Already-tombstoned keys
        // must not consume a sub or appear on disk a second time.
        let mut matched: Vec<(Box<[u8]>, KeyAtRev)> = Vec::with_capacity(candidates.len());
        for k in candidates {
            match self.index.get(&k, current) {
                Ok(at) => matched.push((k, at)),
                // Already tombstoned at `current` (or before).
                Err(KeyIndexError::History(KeyHistoryError::RevisionNotFound)) => {}
                Err(KeyIndexError::KeyNotFound) => {
                    // `keys_in_order` and `index` are kept in sync
                    // under the writer lock; this would indicate a
                    // writer-invariant violation. Surface as
                    // Internal rather than silently undercounting.
                    return Err(MvccError::Internal {
                        context: "index/keys_in_order disagree on key presence",
                    });
                }
                Err(e) => return Err(MvccError::KeyIndex(e)),
            }
        }

        // Step 6: zero matches → no allocation, no advance.
        if matched.is_empty() {
            return Ok((0, Revision::new(current, 0)));
        }

        // Phase 3 plan §4.6 (ROADMAP.md:863): capture `prev` from
        // a pre-mutation backend snapshot BEFORE any index update.
        // Tombstones target live keys only, so every match has a
        // prev value. ONE snapshot per writer call amortises the
        // cost across N matches. The captured prev is plumbed onto
        // each `Tombstone` for commit 3 of the L863 series to
        // surface on the watch path; this commit only captures.
        let snap_be = self.backend.snapshot()?;
        let mut prev_kvs: Vec<KeyValue> = Vec::with_capacity(matched.len());
        for (k, at) in &matched {
            let enc = encode_key(at.modified, KeyKind::Put);
            let value = snap_be
                .get(KEY_BUCKET_ID, enc.as_bytes())?
                .unwrap_or_default();
            prev_kvs.push(KeyValue {
                key: Bytes::copy_from_slice(k),
                create_revision: at.created,
                mod_revision: at.modified,
                version: at.version,
                value,
                lease: None,
            });
        }
        drop(snap_be);

        // Step 7+: allocate a single main; sub increments per
        // physical write.
        let rev = Revision::new(state.next_main, 0);

        let mut batch = self.backend.begin_batch()?;
        let mut sub: i64 = 0;
        // Pair each key with the rev it gets tombstoned at, so the
        // post-commit `index.tombstone` calls reuse the on-disk
        // assignments verbatim. `prev` rides along on each
        // [`Tombstone`] for the L863 emit-side wiring (commit 3).
        let mut tombstones: Vec<Tombstone> = Vec::with_capacity(matched.len());
        for ((k, _at), prev) in matched.into_iter().zip(prev_kvs.into_iter()) {
            let key_rev = Revision::new(rev.main(), sub);
            let encoded = encode_key(key_rev, KeyKind::Tombstone);
            batch.put(KEY_BUCKET_ID, encoded.as_bytes(), TOMBSTONE_VALUE)?;
            sub = sub.checked_add(1).ok_or(MvccError::Internal {
                context: "delete_range sub overflow",
            })?;
            tombstones.push(Tombstone {
                key: k,
                rev: key_rev,
                prev,
            });
        }
        // No fsync — Raft's WAL above us owns durability (parity
        // with `Put`).
        let _ = self.backend.commit_batch(batch, false).await?;

        // Step 11: in-mem tombstones. Holding only the writer lock
        // here; `keys_in_order` is intentionally NOT modified
        // (review item R3). `index.tombstone` returning `Err` would
        // indicate the writer-lock invariant is broken (plan
        // §5.4 review item S3).
        for tomb in &tombstones {
            if let Err(_e) = self.index.tombstone(&tomb.key, tomb.rev) {
                return Err(MvccError::Internal {
                    context: "index.tombstone failed under writer-lock invariant",
                });
            }
        }

        let deleted = u64::try_from(tombstones.len()).map_err(|_| MvccError::Internal {
            context: "delete_range count exceeds u64",
        })?;

        let next = state.next_main.checked_add(1).ok_or(MvccError::Internal {
            context: "next_main overflow",
        })?;
        state.next_main = next;

        // Phase 3 plan §4.2 (ROADMAP.md:862): one Delete event per
        // tombstoned key, in match order, sub-revision matching the
        // on-disk assignment. `value` is empty for Delete; `prev`
        // rides through verbatim from the capture above.
        //
        // INVARIANT X (Phase 3 plan §4.6 / §6, ROADMAP.md:863):
        // every `prev` here was resolved against the single
        // pre-mutation `Backend::snapshot` taken upstream, before
        // `index.tombstone` mutated the index. The emit path
        // performs no second backend or index lookup. Holds because
        // the snapshot was dropped before the index mutation, and
        // no path between capture and emit mutates the captured
        // KeyValues.
        for tomb in &tombstones {
            state.emit_buf.push(WatchEvent {
                kind: WatchEventKind::Delete,
                key: Bytes::copy_from_slice(&tomb.key),
                value: Bytes::new(),
                prev: Some(tomb.prev.clone()),
                revision: tomb.rev,
            });
        }

        // L846: publish the new snapshot under the writer mutex.
        // `compacted` carries forward unchanged — DeleteRange
        // does not advance the floor.
        let prev = self.snapshot.load_full();
        self.snapshot.store(Arc::new(Snapshot {
            rev: rev.main(),
            compacted: prev.compacted,
        }));

        // Phase 3 plan §4.2: dispatch AFTER snapshot.store, still
        // under writer mutex.
        self.dispatch_observer(&mut state.emit_buf, rev.main());

        Ok((deleted, rev))
    }

    /// Multi-op transaction.
    ///
    /// Per the L844 plan §5.5: evaluates `req.compare` against
    /// the head; if all pass, executes `req.success`, else
    /// `req.failure`. The writer lock is held for the entire
    /// duration so the chosen branch sees the same `current`
    /// the compares saw.
    ///
    /// # Branch dispatch
    ///
    /// The chosen branch may interleave [`RequestOp::Range`],
    /// [`RequestOp::Put`], and [`RequestOp::DeleteRange`].
    ///
    /// - **Zero physical writes** (`Range`-only branch, or a
    ///   branch whose every `DeleteRange` matches no live keys):
    ///   no main rev is allocated; `header_revision` reflects
    ///   the pre-txn `current` (etcd parity for `storeTxnRead`).
    /// - **Mutating branch**: a single `main` rev is allocated;
    ///   subs increment per **physical write** (one per `Put`,
    ///   one per matched key in `DeleteRange`) starting at `0`,
    ///   in branch order (review item S3 / M1).
    ///
    /// `Range` responses inside a mutating branch see the
    /// **post-commit** state — they are evaluated after the
    /// batch commits and the snapshot is republished with the
    /// new `rev` (plan §5.5 step 11).
    ///
    /// # Intra-branch visibility caveat
    ///
    /// `DeleteRange`'s match set is computed against the
    /// pre-txn `keys_in_order` snapshot — a `Put` earlier in the
    /// same branch does **not** make the just-put key visible to
    /// a later `DeleteRange` in the same txn. Symmetrically, a
    /// `Put` after a `DeleteRange` of the same key still records
    /// a fresh value (its sub allocates after the delete's). This
    /// matches the L844 plan §5.5 step 6 phrasing ("compute under
    /// the same writer lock") and is the simplest correct
    /// behaviour at this scope; intra-branch reorder semantics
    /// (`storeTxnWrite`-precise parity) is L851 model-test
    /// territory.
    ///
    /// Empty compare list always succeeds (review item M1).
    /// Responses are index-aligned with the chosen branch slice.
    ///
    /// # Errors
    ///
    /// - [`MvccError::Backend`] from compare evaluation,
    ///   `begin_batch` / `commit_batch`, or post-commit `Range`
    ///   value fetch.
    /// - [`MvccError::KeyIndex`] / [`MvccError::KeyDecode`]
    ///   propagated from compare evaluation or response
    ///   construction.
    /// - [`MvccError::Internal`] if `next_main` overflows, the
    ///   per-txn sub allocator overflows, or the in-mem index
    ///   rejects an op that the writer-lock invariant says it
    ///   must accept.
    pub async fn txn(&self, req: TxnRequest) -> Result<TxnResponse, MvccError> {
        // Hold the writer lock for the entire txn so compare
        // evaluation and branch execution see the same `current`.
        // Read-only txns still take the writer lock — the
        // alternative (RwLock-style upgrade) would need a
        // different primitive and the cost of the async lock
        // acquisition is dwarfed by the work the txn does.
        let mut state = self.writer.lock().await;
        // L846: one rev read for the whole txn — compares,
        // branch evaluation, and any nested Range ops all see
        // this. Republished at the end if any writes happened.
        let current = self.snapshot.load().rev;

        let outcomes = self.evaluate_compares(&req.compare)?;
        let succeeded = outcomes.iter().all(|&b| b);
        let branch = if succeeded {
            &req.success
        } else {
            &req.failure
        };

        let txn_main = state.next_main;
        let (plan, total_physical_writes) = self.build_txn_plan(branch, current, txn_main)?;

        // No physical writes → don't allocate a main rev; build
        // responses against pre-txn state and return.
        if total_physical_writes == 0 {
            return self.build_txn_response_readonly(succeeded, branch, current);
        }

        // Mutating: allocate the single main, commit the batch,
        // apply in-mem updates, then republish the snapshot.
        let head_rev = Revision::new(txn_main, 0);
        self.commit_txn_batch(&plan).await?;
        self.apply_txn_in_mem(&plan)?;

        // Phase 3 plan §4.2 (ROADMAP.md:862): walk the OpPlan and
        // emit one event per physical write (Put → one Put event;
        // Delete → one Delete event per tombstoned key; Read
        // contributes nothing). Sub-revisions match the on-disk
        // assignments inside `OpPlan::Put.rev` / the `tombs`
        // pairs, so commit-revision ordering is preserved.
        //
        // INVARIANT X (Phase 3 plan §4.6 / §6, ROADMAP.md:863):
        // every WatchEvent appended here carries the prev-kv that
        // was captured by `delete_range` / `build_txn_plan` BEFORE
        // any index mutation ran. The emit path performs no second
        // backend or index lookup — `prev` is read straight off the
        // OpPlan / Tombstone. Holds because `apply_txn_in_mem` (the
        // step that mutates the index and `keys_in_order`) ran
        // above on this committed plan; the captured prevs were
        // resolved before that, against the snapshot taken inside
        // `build_txn_plan`'s single `Backend::snapshot` call (or
        // the in-flight `TentativeState` for same-branch
        // overwrites). No call site between capture and emit
        // mutates the captured KeyValues.
        emit_txn_events(&plan, &mut state.emit_buf);

        let next = state.next_main.checked_add(1).ok_or(MvccError::Internal {
            context: "next_main overflow",
        })?;
        state.next_main = next;
        // L846: publish the new snapshot under the writer mutex.
        // Range ops queued after this txn see the post-commit
        // rev. `compacted` carries forward unchanged.
        let prev = self.snapshot.load_full();
        self.snapshot.store(Arc::new(Snapshot {
            rev: head_rev.main(),
            compacted: prev.compacted,
        }));

        // Phase 3 plan §4.2: dispatch AFTER snapshot.store, still
        // under writer mutex.
        self.dispatch_observer(&mut state.emit_buf, head_rev.main());

        let responses = self.build_txn_responses_mutating(branch, &plan)?;

        Ok(TxnResponse {
            succeeded,
            responses,
            header_revision: head_rev.main(),
        })
    }

    /// First pass over the chosen branch: build the per-op
    /// physical-write plan and total physical-write count. Subs
    /// allocate from `0`, incrementing per physical write
    /// (`Put` = one; `DeleteRange` = one per matched live key).
    /// `DeleteRange`'s match set is computed against the pre-txn
    /// `keys_in_order` snapshot (see `txn` doc, intra-branch
    /// visibility caveat).
    ///
    /// Phase 3 plan §4.6 (ROADMAP.md:863): the pass also captures
    /// `prev_kv` for each physical write into the [`OpPlan`]
    /// entries. A [`TentativeState`] map threads in-flight
    /// updates across ops within the same branch so a `Put`
    /// following an earlier `Put` of the same key sees the
    /// in-flight value (NOT the post-mutation index) as its
    /// `prev`. The map is consulted **only** for prev-kv
    /// resolution; the `DeleteRange` match-set continues to be
    /// computed against pre-txn `keys_in_order` exactly as
    /// before. ONE [`Backend::snapshot`] per call amortises
    /// snapshot cost across N matched keys.
    ///
    /// Captured `prev`s are plumbed onto [`OpPlan::Put`] /
    /// [`Tombstone`] but not yet wired into emitted
    /// [`WatchEvent`]s — that ride lands in commit 3 of the L863
    /// series.
    fn build_txn_plan(
        &self,
        branch: &[RequestOp],
        current: i64,
        txn_main: i64,
    ) -> Result<(Vec<OpPlan>, usize), MvccError> {
        let mut plan: Vec<OpPlan> = Vec::with_capacity(branch.len());
        let mut sub: i64 = 0;
        let mut total: usize = 0;
        let mut tentative = TentativeState {
            live: HashMap::with_capacity(branch.len()),
        };
        // Acquire the prev-kv snapshot lazily so the read-only
        // path (no Puts, no live DeleteRange matches) does not pay
        // the snapshot cost. Constructed on first need.
        let mut snap_be: Option<B::Snapshot> = None;
        for op in branch {
            match op {
                RequestOp::Range(_) => plan.push(OpPlan::Read),
                RequestOp::Put { key, value } => {
                    let rev = Revision::new(txn_main, sub);
                    let prev =
                        self.compute_txn_op_prev_for_put(key, rev, &tentative, &mut snap_be)?;
                    // Tentative state: the just-pushed Put makes
                    // the key live with `value` (the in-flight
                    // version). Build a synthetic `KeyValue` so a
                    // later op in the same txn that wants `prev`
                    // resolves against this in-flight write
                    // instead of the post-mutation index.
                    let new_kv = KeyValue {
                        key: Bytes::copy_from_slice(key),
                        create_revision: prev.as_ref().map_or(rev, |p| p.create_revision),
                        mod_revision: rev,
                        version: prev.as_ref().map_or(1, |p| p.version.saturating_add(1)),
                        value: Bytes::copy_from_slice(value),
                        lease: None,
                    };
                    let _ = tentative.live.insert(key.as_slice().into(), Some(new_kv));
                    sub = checked_add_sub(sub)?;
                    total = checked_add_total(total)?;
                    plan.push(OpPlan::Put {
                        key: key.clone(),
                        value: value.clone(),
                        rev,
                        prev,
                    });
                }
                RequestOp::DeleteRange { key, end } => {
                    let tombs = self.plan_delete_range(
                        key,
                        end,
                        current,
                        txn_main,
                        &mut sub,
                        &mut total,
                        &mut tentative,
                        &mut snap_be,
                    )?;
                    plan.push(OpPlan::Delete { tombs });
                }
            }
        }
        Ok((plan, total))
    }

    /// Resolve `prev` for a single `Put` op inside
    /// [`Self::build_txn_plan`]: prefer the in-flight tentative
    /// state, fall back to the pre-txn index via
    /// [`Self::compute_prev_kv_strict`]. Lazily builds `snap_be`
    /// on first use so a Range-only branch never acquires a
    /// backend snapshot.
    fn compute_txn_op_prev_for_put(
        &self,
        key: &[u8],
        at_rev: Revision,
        tentative: &TentativeState,
        snap_be: &mut Option<B::Snapshot>,
    ) -> Result<Option<KeyValue>, MvccError> {
        match tentative.live.get(key) {
            Some(Some(prior)) => Ok(Some(prior.clone())),
            Some(None) => Ok(None),
            None => {
                if snap_be.is_none() {
                    *snap_be = Some(self.backend.snapshot()?);
                }
                let snap_ref = snap_be.as_ref().ok_or(MvccError::Internal {
                    context: "txn snap_be construction logic inconsistency",
                })?;
                self.compute_prev_kv_strict(key, at_rev, snap_ref)
            }
        }
    }

    /// Compute the matched-key/sub list for a single
    /// [`RequestOp::DeleteRange`] under the writer lock. Filters
    /// already-tombstoned keys (review item S3); allocates one
    /// sub per surviving match and bumps the running sub /
    /// total counters.
    ///
    /// Phase 3 plan §4.6 (ROADMAP.md:863): also resolves `prev`
    /// per match via the supplied [`TentativeState`] (in-flight
    /// state from earlier ops in the same txn) and falls back to
    /// the pre-txn index when a match was not touched. A match
    /// whose tentative state is `Some(None)` (already deleted
    /// in-flight by a prior op in this txn) is skipped — the
    /// second tombstone of an already-tombstoned key would
    /// previously have been rejected by `index.tombstone` at
    /// apply time as `TombstoneOnEmpty`, so the new path also
    /// fixes that latent error case.
    #[allow(clippy::too_many_arguments)]
    fn plan_delete_range(
        &self,
        key: &[u8],
        end: &[u8],
        current: i64,
        txn_main: i64,
        sub: &mut i64,
        total: &mut usize,
        tentative: &mut TentativeState,
        snap_be: &mut Option<B::Snapshot>,
    ) -> Result<Vec<Tombstone>, MvccError> {
        let candidates: Vec<Box<[u8]>> = {
            let keys = self.keys_in_order.read();
            if end.is_empty() {
                if keys.contains_key(key) {
                    vec![key.into()]
                } else {
                    Vec::new()
                }
            } else {
                keys.range::<[u8], _>((Bound::Included(key), Bound::Excluded(end)))
                    .map(|(k, ())| k.clone())
                    .collect()
            }
        };
        let mut tombs: Vec<Tombstone> = Vec::with_capacity(candidates.len());
        for k in candidates {
            match self.index.get(&k, current) {
                Ok(at) => {
                    let prev = match tentative.live.get(k.as_ref()) {
                        Some(Some(prior)) => prior.clone(),
                        Some(None) => {
                            // Already deleted in-flight earlier
                            // in this txn — skip the second
                            // tombstone (would otherwise hit
                            // `KeyHistory::TombstoneOnEmpty` at
                            // apply time).
                            continue;
                        }
                        None => {
                            // Resolve from pre-txn state via the
                            // shared backend snapshot. Lazy on
                            // first need.
                            if snap_be.is_none() {
                                *snap_be = Some(self.backend.snapshot()?);
                            }
                            let snap_ref = snap_be.as_ref().ok_or(MvccError::Internal {
                                context: "txn snap_be construction logic inconsistency",
                            })?;
                            let enc = encode_key(at.modified, KeyKind::Put);
                            let value = snap_ref
                                .get(KEY_BUCKET_ID, enc.as_bytes())?
                                .unwrap_or_default();
                            KeyValue {
                                key: Bytes::copy_from_slice(&k),
                                create_revision: at.created,
                                mod_revision: at.modified,
                                version: at.version,
                                value,
                                lease: None,
                            }
                        }
                    };
                    let rev = Revision::new(txn_main, *sub);
                    *sub = checked_add_sub(*sub)?;
                    *total = checked_add_total(*total)?;
                    let _ = tentative.live.insert(k.clone(), None);
                    tombs.push(Tombstone { key: k, rev, prev });
                }
                Err(KeyIndexError::History(KeyHistoryError::RevisionNotFound)) => {
                    // Already tombstoned at `current` (or before)
                    // by some PRIOR txn — silently skip exactly
                    // as `delete_range` does. We do NOT update
                    // tentative state: the key was never live in
                    // this txn's branch.
                }
                Err(KeyIndexError::KeyNotFound) => {
                    return Err(MvccError::Internal {
                        context: "index/keys_in_order disagree on key presence",
                    });
                }
                Err(e) => return Err(MvccError::KeyIndex(e)),
            }
        }
        Ok(tombs)
    }

    /// Issue the on-disk batch covering every physical write in
    /// `plan`. No fsync — Raft's WAL above us owns durability.
    async fn commit_txn_batch(&self, plan: &[OpPlan]) -> Result<(), MvccError> {
        let mut batch = self.backend.begin_batch()?;
        for entry in plan {
            match entry {
                OpPlan::Read => {}
                OpPlan::Put { value, rev, .. } => {
                    let encoded = encode_key(*rev, KeyKind::Put);
                    batch.put(KEY_BUCKET_ID, encoded.as_bytes(), value)?;
                }
                OpPlan::Delete { tombs } => {
                    for tomb in tombs {
                        let encoded = encode_key(tomb.rev, KeyKind::Tombstone);
                        batch.put(KEY_BUCKET_ID, encoded.as_bytes(), TOMBSTONE_VALUE)?;
                    }
                }
            }
        }
        let _ = self.backend.commit_batch(batch, false).await?;
        Ok(())
    }

    /// Apply in-memory updates after the on-disk commit
    /// succeeds: `keys_in_order` insert + `index.put` for each
    /// `OpPlan::Put`; `index.tombstone` for each entry in each
    /// `OpPlan::Delete` (`keys_in_order` unchanged — review item
    /// R3).
    fn apply_txn_in_mem(&self, plan: &[OpPlan]) -> Result<(), MvccError> {
        for entry in plan {
            match entry {
                OpPlan::Read => {}
                OpPlan::Put { key, rev, .. } => {
                    // Index first, then `keys_in_order`
                    // (rust-expert PR #75 review R1) — see
                    // `Self::put` for the full ordering rationale.
                    if let Err(_e) = self.index.put(key, *rev) {
                        return Err(MvccError::Internal {
                            context: "index.put failed under txn writer-lock invariant",
                        });
                    }
                    {
                        let mut keys = self.keys_in_order.write();
                        let _ = keys.insert(key.as_slice().into(), ());
                    }
                }
                OpPlan::Delete { tombs } => {
                    for tomb in tombs {
                        if let Err(_e) = self.index.tombstone(&tomb.key, tomb.rev) {
                            return Err(MvccError::Internal {
                                context: "index.tombstone failed under txn writer-lock invariant",
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Third pass: build responses index-aligned with `branch`,
    /// against post-commit state. `Range` ops re-evaluate via
    /// `self.range(...)` so they see the just-committed Puts.
    fn build_txn_responses_mutating(
        &self,
        branch: &[RequestOp],
        plan: &[OpPlan],
    ) -> Result<Vec<ResponseOp>, MvccError> {
        let mut responses: Vec<ResponseOp> = Vec::with_capacity(branch.len());
        for (op, entry) in branch.iter().zip(plan.iter()) {
            match (op, entry) {
                (RequestOp::Range(range_req), OpPlan::Read) => {
                    let result = self.range(range_req.clone())?;
                    responses.push(ResponseOp::Range(result));
                }
                (RequestOp::Put { .. }, OpPlan::Put { .. }) => {
                    // L844 always reports `prev_revision: None`
                    // (etcd populates this only when
                    // `prev_kv = true`; Phase 6 wires that
                    // through).
                    responses.push(ResponseOp::Put {
                        prev_revision: None,
                    });
                }
                (RequestOp::DeleteRange { .. }, OpPlan::Delete { tombs }) => {
                    let deleted = u64::try_from(tombs.len()).map_err(|_| MvccError::Internal {
                        context: "txn DeleteRange count exceeds u64",
                    })?;
                    responses.push(ResponseOp::DeleteRange { deleted });
                }
                _ => {
                    return Err(MvccError::Internal {
                        context: "txn plan/branch op kind mismatch",
                    });
                }
            }
        }
        Ok(responses)
    }

    /// Read-only txn response builder. Splits the no-physical-
    /// writes path out of `txn` so the mutating path doesn't
    /// pay for branching on every `OpPlan::Read` entry.
    fn build_txn_response_readonly(
        &self,
        succeeded: bool,
        branch: &[RequestOp],
        current: i64,
    ) -> Result<TxnResponse, MvccError> {
        let mut responses: Vec<ResponseOp> = Vec::with_capacity(branch.len());
        for op in branch {
            match op {
                RequestOp::Range(range_req) => {
                    let result = self.range(range_req.clone())?;
                    responses.push(ResponseOp::Range(result));
                }
                RequestOp::Put { .. } => {
                    // Reachable only via the
                    // `total_physical_writes == 0` path, which
                    // means the branch contains no `Put` (Put is
                    // always a physical write). This arm exists
                    // for exhaustiveness — surface as Internal
                    // rather than panic if reached (workspace
                    // `clippy::panic` is deny).
                    return Err(MvccError::Internal {
                        context: "txn read-only path saw RequestOp::Put",
                    });
                }
                RequestOp::DeleteRange { .. } => {
                    // Zero-match DeleteRange in a no-write branch:
                    // the response is `deleted = 0` (etcd parity).
                    responses.push(ResponseOp::DeleteRange { deleted: 0 });
                }
            }
        }
        Ok(TxnResponse {
            succeeded,
            responses,
            header_revision: current,
        })
    }

    /// Compact every revision strictly below `rev`.
    ///
    /// Per the L844 plan §5.6: physically removes on-disk entries
    /// whose `main <= rev` and which are not in the per-key
    /// "available" set (the set of revs each key needs to retain
    /// so a `Range` at `rev` itself still sees the right value).
    /// Then advances the in-memory `compacted` floor and runs the
    /// in-memory index compaction.
    ///
    /// **Order: on-disk first, then floor advance, then in-mem.**
    /// Crash between the on-disk commit and the floor advance is
    /// safe — the on-disk state is the persistent floor, and L852
    /// recovery infers the floor from "min on-disk rev minus 1"
    /// (review item B3).
    ///
    /// Compaction is **synchronous** at L844: the writer is
    /// blocked for the full duration. L850 backgrounds it.
    ///
    /// `force_fsync = true` on the on-disk delete commit —
    /// compaction durability is the user-visible promise (plan
    /// §5.6 step 8).
    ///
    /// Idempotent for `rev <= compacted_floor` (returns
    /// `Ok(())` with no work).
    ///
    /// # Errors
    ///
    /// - [`MvccError::FutureRevision`] if `rev > snap.rev`.
    /// - [`MvccError::Backend`] from snapshot acquisition,
    ///   `begin_batch`, `commit_batch`, or range iteration.
    /// - [`MvccError::KeyDecode`] if an on-disk encoded key
    ///   fails to decode (indicates backend corruption).
    pub async fn compact(&self, rev: i64) -> Result<(), MvccError> {
        let _state = self.writer.lock().await;
        // L846: coherent (rev, compacted) read.
        let snap = self.snapshot.load_full();
        let current = snap.rev;
        let floor = snap.compacted;

        if rev <= floor {
            return Ok(());
        }
        if rev > current {
            return Err(MvccError::FutureRevision {
                requested: rev,
                current,
            });
        }

        let available = self.compute_available(rev);
        self.commit_compaction_deletes(rev, &available).await?;

        // Step 9: publish the new floor as part of an
        // atomically-published Snapshot (L846). Done BEFORE the
        // in-mem compaction so a concurrent `Range` reader
        // observing the new floor will reject `rev < floor`
        // reads — the on-disk state is already physically
        // advanced to match. `rev` (revision head) is unchanged
        // by compact; `current` was captured under the writer
        // mutex above.
        self.snapshot.store(Arc::new(Snapshot {
            rev: current,
            compacted: rev,
        }));

        // Step 10: in-mem compaction. `ShardedKeyIndex::compact`
        // drops entries whose history becomes empty. Walk
        // `keys_in_order` after the index pass and drop any key
        // whose index entry is gone — etcd parity for the post-
        // tombstone-compaction reap (review item R3).
        let mut available_post: HashSet<Revision> = HashSet::new();
        self.index.compact(rev, &mut available_post);
        self.reap_keys_in_order_after_compact(current);

        Ok(())
    }

    /// Compute the per-key "available" set: the union of
    /// surviving revs across every key in the index. Uses
    /// [`ShardedKeyIndex::keep`] (read-only) so concurrent
    /// `Range` readers are not blocked by a write-lock pass.
    fn compute_available(&self, rev: i64) -> HashSet<Revision> {
        let mut available: HashSet<Revision> = HashSet::new();
        self.index.keep(rev, &mut available);
        available
    }

    /// Build the on-disk delete batch and commit it with
    /// `force_fsync = true` (compaction durability promise).
    /// Iterates `KEY_BUCKET_ID` over `[(0, 0, Put), (rev,
    /// i64::MAX, Tombstone))`; deletes any entry whose decoded
    /// rev has `main <= rev` and is not in `available`.
    async fn commit_compaction_deletes(
        &self,
        rev: i64,
        available: &HashSet<Revision>,
    ) -> Result<(), MvccError> {
        let snap = self.backend.snapshot()?;
        let start = encode_key(Revision::new(0, 0), KeyKind::Put);
        let end = encode_key(Revision::new(rev, i64::MAX), KeyKind::Tombstone);

        let mut batch = self.backend.begin_batch()?;
        let iter = snap.range(KEY_BUCKET_ID, start.as_bytes(), end.as_bytes())?;
        for item in iter {
            let (encoded_key, _value) = item?;
            let (decoded_rev, _kind) = decode_key(&encoded_key)?;
            if decoded_rev.main() <= rev && !available.contains(&decoded_rev) {
                batch.delete(KEY_BUCKET_ID, &encoded_key)?;
            }
        }
        drop(snap);

        // `force_fsync = true` — compaction durability is the
        // user-visible promise (plan §5.6 step 8).
        let _ = self.backend.commit_batch(batch, true).await?;
        Ok(())
    }

    /// Reap keys from `keys_in_order` whose index entry was
    /// dropped by [`ShardedKeyIndex::compact`]. Detection: the
    /// key has no history visible at `current` AND no entry at
    /// all (post-compact).
    ///
    /// Two arms reap (review item R3, expanded to fix the
    /// rust-expert S1 finding on PR #75):
    ///
    /// - `KeyNotFound` — `ShardedKeyIndex::compact` dropped the
    ///   entry entirely (last live generation was reaped).
    /// - `History(RevisionNotFound)` — the index retained a
    ///   tombstone-only history; `index.get(k, current)` walks
    ///   into `KeyHistory::get`, sees the tombstone is the only
    ///   visible generation at `<= current`, and reports
    ///   `RevisionNotFound`. `Range` skips on this arm, so the
    ///   key is invisible at HEAD and must be reaped here too —
    ///   otherwise `keys_in_order` leaks dead entries on every
    ///   put-then-delete-then-compact loop (etcd parity for
    ///   `mvcc/kvstore_compaction.go::scheduleCompaction` which
    ///   discards `KeyIndex` entries whose generations are all
    ///   compacted away).
    fn reap_keys_in_order_after_compact(&self, current: i64) {
        let snapshot: Vec<Box<[u8]>> = {
            let keys = self.keys_in_order.read();
            keys.keys().cloned().collect()
        };
        let mut keys = self.keys_in_order.write();
        for k in &snapshot {
            if let Err(
                KeyIndexError::KeyNotFound
                | KeyIndexError::History(KeyHistoryError::RevisionNotFound),
            ) = self.index.get(k, current)
            {
                let _ = keys.remove(k.as_ref());
            }
        }
    }

    /// Evaluate a list of [`Compare`] preconditions against the
    /// store's head revision.
    ///
    /// Per the L844 plan §5.5: each compare's target field is
    /// loaded from the index at `current` (the head main rev when
    /// this method is called); an absent key (no entry in the
    /// index, or tombstoned at `current`) defaults to `version =
    /// 0`, `create_revision.main = 0`, `mod_revision.main = 0`,
    /// `value = b""` (review item B4 — etcd
    /// `mvcc/kvstore_txn.go::checkCompare` zero-value path).
    ///
    /// Returns the per-compare outcomes (index-aligned with the
    /// input slice). Callers compute `all_passed` as
    /// `outcomes.iter().all(|&b| b)`. **Empty `compares` returns
    /// an empty `Vec` whose `all` is `true`** (review item M1 —
    /// etcd parity for an empty compare list).
    ///
    /// This is a pure read function; it acquires no locks beyond
    /// `index` shard reads + an optional backend snapshot (only
    /// when at least one [`Compare::Value`] is present, to avoid
    /// the snapshot cost on the common compare-by-revision path).
    ///
    /// # Errors
    ///
    /// - [`MvccError::Backend`] if a [`Compare::Value`] needs the
    ///   on-disk value and the snapshot/get fails.
    /// - [`MvccError::KeyIndex`] for non-`KeyNotFound` /
    ///   non-`RevisionNotFound` index errors (those two are the
    ///   "absent" path — not errors).
    pub(super) fn evaluate_compares(&self, compares: &[Compare]) -> Result<Vec<bool>, MvccError> {
        // Caller (`txn`) holds the writer mutex, so the snapshot
        // value is stable across this call. L846: read rev via
        // the published Snapshot, not a separate atomic.
        let current = self.snapshot.load().rev;
        // Backend snapshot is only needed for `Compare::Value`;
        // skip snapshot acquisition if no value compares appear.
        let snap = if compares.iter().any(|c| matches!(c, Compare::Value { .. })) {
            Some(self.backend.snapshot()?)
        } else {
            None
        };
        let mut out = Vec::with_capacity(compares.len());
        for compare in compares {
            out.push(self.evaluate_compare(compare, current, snap.as_ref())?);
        }
        Ok(out)
    }

    /// Evaluate a single [`Compare`] against the store. Helper
    /// for [`Self::evaluate_compares`]; split out so each variant
    /// has a single focused branch.
    fn evaluate_compare(
        &self,
        compare: &Compare,
        current: i64,
        snap: Option<&B::Snapshot>,
    ) -> Result<bool, MvccError> {
        let key: &[u8] = match compare {
            Compare::Version { key, .. }
            | Compare::CreateRevision { key, .. }
            | Compare::ModRevision { key, .. }
            | Compare::Value { key, .. } => key,
        };

        let at: Option<crate::key_history::KeyAtRev> = match self.index.get(key, current) {
            Ok(at) => Some(at),
            // Both "no entry" and "tombstoned at current" are the
            // absent path (B4 — defaults).
            Err(
                KeyIndexError::KeyNotFound
                | KeyIndexError::History(KeyHistoryError::RevisionNotFound),
            ) => None,
            Err(e) => return Err(MvccError::KeyIndex(e)),
        };

        match compare {
            Compare::Version { op, target, .. } => {
                let actual = at.map_or(0_i64, |a| a.version);
                Ok(apply_compare_ord(&actual, target, *op))
            }
            Compare::CreateRevision { op, target, .. } => {
                let actual = at.map_or(0_i64, |a| a.created.main());
                Ok(apply_compare_ord(&actual, target, *op))
            }
            Compare::ModRevision { op, target, .. } => {
                let actual = at.map_or(0_i64, |a| a.modified.main());
                Ok(apply_compare_ord(&actual, target, *op))
            }
            Compare::Value { op, target, .. } => {
                let actual: Bytes = match (at, snap) {
                    (Some(found), Some(s)) => {
                        let enc = encode_key(found.modified, KeyKind::Put);
                        s.get(KEY_BUCKET_ID, enc.as_bytes())?.unwrap_or_default()
                    }
                    // Absent key, or no snapshot (caller didn't
                    // request one — structurally only happens when
                    // there are no Value compares, so this arm is
                    // unreachable in practice; default to empty
                    // bytes for safety).
                    _ => Bytes::new(),
                };
                Ok(apply_compare_ord(actual.as_ref(), target.as_slice(), *op))
            }
        }
    }
}

/// Apply a [`CompareOp`] to two `Ord` values (lex order for byte
/// slices, numeric order for `i64`). Pure function so the compare
/// evaluator only differs by which field it loads, not by which
/// operator it implements.
fn apply_compare_ord<T: Ord + ?Sized>(actual: &T, target: &T, op: CompareOp) -> bool {
    match op {
        CompareOp::Equal => actual == target,
        CompareOp::NotEqual => actual != target,
        CompareOp::Greater => actual > target,
        CompareOp::Less => actual < target,
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

    use super::{MvccStore, Snapshot};
    use crate::bucket::{KEY_BUCKET_ID, KEY_INDEX_BUCKET_ID};
    use crate::error::OpenError;
    use mango_storage::{Backend, BackendConfig, InMemBackend, ReadSnapshot};
    use std::sync::Arc;

    /// Compile-time check: `MvccStore<InMemBackend>` is
    /// `Send + Sync`. A future change that breaks this (e.g.
    /// adding a `Cell` field) fails the build at this site.
    const _: () = {
        const fn assert_send<T: Send>() {}
        const fn assert_sync<T: Sync>() {}
        const fn check() {
            assert_send::<MvccStore<InMemBackend>>();
            assert_sync::<MvccStore<InMemBackend>>();
        }
        check();
    };

    /// Compile-time check: writer futures (`put`, `delete_range`,
    /// `compact`, `txn`) are `Send` (rust-expert PR #75 review S2).
    /// A future change that captures a `!Send` local across `await`
    /// (e.g. holds the batch on the stack instead of letting
    /// `commit_batch` consume it in its sync prologue) fails the
    /// build at this site. Required for L854's `tokio::spawn`-per-
    /// gRPC-request handler.
    #[allow(dead_code, reason = "compile-time assertion only")]
    fn _assert_writer_futures_are_send() {
        fn assert_send_fut<F: core::future::Future + Send>(_: F) {}
        fn check(s: &'static MvccStore<InMemBackend>) {
            assert_send_fut(s.put(b"", b"a"));
            assert_send_fut(s.delete_range(b"", b""));
            assert_send_fut(s.compact(0));
            assert_send_fut(s.txn(crate::store::txn::TxnRequest::default()));
        }
        let _ = check;
    }

    fn fresh_backend() -> InMemBackend {
        InMemBackend::open(BackendConfig::new("/unused".into(), false))
            .expect("inmem open never fails")
    }

    #[test]
    fn open_against_fresh_backend_succeeds() {
        let backend = fresh_backend();
        let store = MvccStore::open(backend).expect("fresh open");
        assert_eq!(store.current_revision(), 0);
    }

    #[test]
    fn open_is_idempotent_on_bucket_registration() {
        let backend = fresh_backend();
        // Pre-register the buckets — open() should still succeed
        // because Backend::register_bucket is idempotent.
        backend
            .register_bucket("key", KEY_BUCKET_ID)
            .expect("pre-register key");
        backend
            .register_bucket("key_index", KEY_INDEX_BUCKET_ID)
            .expect("pre-register key_index");
        let store = MvccStore::open(backend).expect("idempotent open");
        assert_eq!(store.current_revision(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_rejects_non_empty_backend() {
        let backend = fresh_backend();
        backend
            .register_bucket("key", KEY_BUCKET_ID)
            .expect("register");
        // Seed one entry into the key bucket.
        let mut batch = backend.begin_batch().expect("begin");
        {
            use mango_storage::WriteBatch as _;
            batch
                .put(KEY_BUCKET_ID, b"\x00\x00\x00\x00\x00\x00\x00\x01", b"v")
                .expect("put");
        }
        let _ = backend.commit_batch(batch, false).await.expect("commit");

        let err = MvccStore::open(backend).expect_err("non-empty rejected");
        match err {
            OpenError::NonEmptyBackend { found_revs } => {
                assert!(found_revs >= 1, "found_revs = {found_revs}");
            }
            other => panic!("expected NonEmptyBackend, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_non_empty_caps_at_1024() {
        let backend = fresh_backend();
        backend
            .register_bucket("key", KEY_BUCKET_ID)
            .expect("register");
        // Seed 2000 entries — cap should clamp the report at 1024.
        let mut batch = backend.begin_batch().expect("begin");
        {
            use mango_storage::WriteBatch as _;
            for i in 0_u64..2000 {
                let key = i.to_be_bytes();
                batch.put(KEY_BUCKET_ID, &key, b"v").expect("put");
            }
        }
        let _ = backend.commit_batch(batch, false).await.expect("commit");

        let err = MvccStore::open(backend).expect_err("non-empty rejected");
        match err {
            OpenError::NonEmptyBackend { found_revs } => {
                assert_eq!(found_revs, 1024, "cap not enforced: {found_revs}");
            }
            other => panic!("expected NonEmptyBackend, got {other:?}"),
        }
    }

    #[test]
    fn current_revision_is_zero_on_fresh_store() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        assert_eq!(store.current_revision(), 0);
        // Multiple loads are stable.
        assert_eq!(store.current_revision(), 0);
    }

    // === Observer slot (Phase 3 plan §4.1, ROADMAP.md:862) ===

    /// Phase 3 plan §11 test #14. The observer slot is single-
    /// occupancy; the second `attach_observer` call returns
    /// `MvccError::Internal` and does NOT replace the existing
    /// observer.
    #[test]
    fn observer_double_attach_rejects() {
        use crate::error::MvccError;
        use crate::watchable_store::{WatchEvent, WriteObserver};

        struct Noop;
        impl WriteObserver for Noop {
            fn on_apply(&self, _events: &[WatchEvent], _at_main: i64) {}
        }

        let store = MvccStore::open(fresh_backend()).expect("open");
        assert!(!store.observer_is_attached(), "fresh store has no observer");

        let first: Arc<dyn WriteObserver> = Arc::new(Noop);
        store.attach_observer(first).expect("first attach succeeds");
        assert!(store.observer_is_attached(), "first attach lit the slot");

        let second: Arc<dyn WriteObserver> = Arc::new(Noop);
        let err = store
            .attach_observer(second)
            .expect_err("second attach must reject");
        match err {
            MvccError::Internal { context } => {
                assert_eq!(context, "observer slot already occupied");
            }
            other => panic!("expected Internal, got {other:?}"),
        }
        // Slot still holds the first observer (not the rejected
        // second one).
        assert!(store.observer_is_attached());
    }

    /// Phase 3 plan §12 commit #2 (ROADMAP.md:862). Asserts that
    /// `put` / `delete_range` / `txn` each dispatch the right
    /// `WatchEvents` to the attached observer, in commit-revision
    /// order, with sub-revisions matching on-disk assignments.
    /// `at_main` matches the published main revision per call.
    #[tokio::test(flavor = "current_thread")]
    async fn observer_records_put_delete_txn_events() {
        use crate::revision::Revision;
        use crate::store::txn::{RequestOp, TxnRequest};
        use crate::watchable_store::{WatchEvent, WatchEventKind, WriteObserver};
        use bytes::Bytes;
        use parking_lot::Mutex;

        struct Recorder {
            calls: Mutex<Vec<(Vec<WatchEvent>, i64)>>,
        }
        impl WriteObserver for Recorder {
            fn on_apply(&self, events: &[WatchEvent], at_main: i64) {
                self.calls.lock().push((events.to_vec(), at_main));
            }
        }

        let store = MvccStore::open(fresh_backend()).expect("open");
        let rec = Arc::new(Recorder {
            calls: Mutex::new(Vec::new()),
        });
        store
            .attach_observer(Arc::clone(&rec) as Arc<dyn WriteObserver>)
            .expect("first attach");

        // 1. Multi-op txn that pre-populates a/b: [Put a, Put b,
        //    Range b]. Two physical writes; subs 0 and 1; main=1.
        //    DeleteRange inside the same txn is intentionally
        //    omitted — txn `DeleteRange` matches against the
        //    pre-txn `keys_in_order` snapshot (see `build_txn_plan`
        //    rustdoc), so deleting a/b in the same txn that puts
        //    them is a no-op. Multi-key Delete is exercised via a
        //    follow-up `delete_range` in step 2.
        let txn_resp = store
            .txn(TxnRequest {
                compare: vec![],
                success: vec![
                    RequestOp::Put {
                        key: b"a".to_vec(),
                        value: b"av".to_vec(),
                    },
                    RequestOp::Put {
                        key: b"b".to_vec(),
                        value: b"bv".to_vec(),
                    },
                    RequestOp::Range(crate::store::range::RangeRequest {
                        key: b"b".to_vec(),
                        ..Default::default()
                    }),
                ],
                failure: vec![],
            })
            .await
            .expect("txn");
        assert!(txn_resp.succeeded);
        assert_eq!(txn_resp.header_revision, 1);

        // 2. Range-DeleteRange [a, c) — tombstones both a and b
        //    in one op. main=2, subs 0..1.
        let (deleted, del_rev) = store.delete_range(b"a", b"c").await.expect("delete_range");
        assert_eq!(deleted, 2);
        assert_eq!(del_rev.main(), 2);

        // 3. Single Put → one Put event at main=3, sub=0.
        let put_rev = store.put(b"k1", b"v1").await.expect("put");
        assert_eq!(put_rev.main(), 3);

        let calls = rec.calls.lock().clone();
        assert_eq!(calls.len(), 3, "one on_apply call per writer op");

        // Call 1 — Txn: Put(a, sub=0), Put(b, sub=1). Range → no
        //              event.
        let (events, at_main) = &calls[0];
        assert_eq!(*at_main, 1);
        assert_eq!(events.len(), 2, "two physical writes from txn");

        assert_eq!(events[0].kind, WatchEventKind::Put);
        assert_eq!(events[0].key, Bytes::from_static(b"a"));
        assert_eq!(events[0].value, Bytes::from_static(b"av"));
        assert_eq!(events[0].revision, Revision::new(1, 0));
        assert!(events[0].prev.is_none());

        assert_eq!(events[1].kind, WatchEventKind::Put);
        assert_eq!(events[1].key, Bytes::from_static(b"b"));
        assert_eq!(events[1].value, Bytes::from_static(b"bv"));
        assert_eq!(events[1].revision, Revision::new(1, 1));

        // Call 2 — DeleteRange tombstones both keys; subs 0 and 1.
        let (events, at_main) = &calls[1];
        assert_eq!(*at_main, 2);
        assert_eq!(events.len(), 2, "two tombstoned keys");

        assert_eq!(events[0].kind, WatchEventKind::Delete);
        assert_eq!(events[0].key, Bytes::from_static(b"a"));
        assert!(events[0].value.is_empty());
        assert_eq!(events[0].revision, Revision::new(2, 0));

        assert_eq!(events[1].kind, WatchEventKind::Delete);
        assert_eq!(events[1].key, Bytes::from_static(b"b"));
        assert!(events[1].value.is_empty());
        assert_eq!(events[1].revision, Revision::new(2, 1));

        // Call 3 — Put.
        let (events, at_main) = &calls[2];
        assert_eq!(*at_main, 3);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, WatchEventKind::Put);
        assert_eq!(events[0].key, Bytes::from_static(b"k1"));
        assert_eq!(events[0].value, Bytes::from_static(b"v1"));
        assert_eq!(events[0].revision, Revision::new(3, 0));
    }

    /// Phase 3 plan §4.2 negative-path assertion. A
    /// `DeleteRange` that matches no live keys must NOT call
    /// `on_apply` (events would be empty) and must not advance
    /// `next_main`. Confirms the no-physical-write fast-path in
    /// `delete_range` (`if matched.is_empty() { return ... }`)
    /// short-circuits before any dispatch attempt.
    #[tokio::test(flavor = "current_thread")]
    async fn observer_skipped_on_zero_match_delete_range() {
        use crate::watchable_store::{WatchEvent, WriteObserver};
        use parking_lot::Mutex;

        struct Recorder {
            calls: Mutex<usize>,
        }
        impl WriteObserver for Recorder {
            fn on_apply(&self, _events: &[WatchEvent], _at_main: i64) {
                *self.calls.lock() += 1;
            }
        }

        let store = MvccStore::open(fresh_backend()).expect("open");
        let rec = Arc::new(Recorder {
            calls: Mutex::new(0),
        });
        store
            .attach_observer(Arc::clone(&rec) as Arc<dyn WriteObserver>)
            .expect("attach");

        // No keys present; a DeleteRange matches nothing.
        let (deleted, _rev) = store
            .delete_range(b"missing", b"")
            .await
            .expect("delete_range");
        assert_eq!(deleted, 0);
        // current_revision must NOT advance — DeleteRange that
        // matched nothing returns early before allocating a main.
        assert_eq!(store.current_revision(), 0);
        assert_eq!(*rec.calls.lock(), 0, "no observer call for zero-match");
    }

    // === Put (plan §5.2) ===

    #[tokio::test(flavor = "current_thread")]
    async fn put_returns_allocated_rev() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let rev = store.put(b"k", b"v").await.expect("put");
        // First put on a fresh store allocates `(1, 0)` — `next_main`
        // starts at 1 per plan §5.1, sub is 0 for any single-op.
        assert_eq!(rev.main(), 1);
        assert_eq!(rev.sub(), 0);
        // `current_revision` reflects the allocation after the
        // writer drops the lock.
        assert_eq!(store.current_revision(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn put_then_put_increments_main() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let r1 = store.put(b"a", b"1").await.expect("put a");
        let r2 = store.put(b"b", b"2").await.expect("put b");
        let r3 = store.put(b"a", b"3").await.expect("re-put a");
        assert_eq!(r1.main(), 1);
        assert_eq!(r2.main(), 2);
        assert_eq!(r3.main(), 3);
        // Sub is always 0 for a single-key Put — sub allocation
        // resets per top-level op (plan §5.1).
        assert_eq!(r1.sub(), 0);
        assert_eq!(r2.sub(), 0);
        assert_eq!(r3.sub(), 0);
        assert_eq!(store.current_revision(), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn put_persists_encoded_key_to_backend() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let rev = store.put(b"k", b"v").await.expect("put");
        // Read back through the backend snapshot directly. The
        // on-disk key is the Put-kind 17-byte encoding of `rev`;
        // the value is the user payload byte-for-byte.
        let snap = store.backend().snapshot().expect("snapshot");
        let enc = crate::encoding::encode_key(rev, crate::encoding::KeyKind::Put);
        let got = snap
            .get(crate::bucket::KEY_BUCKET_ID, enc.as_bytes())
            .expect("get");
        assert_eq!(got.as_deref(), Some(&b"v"[..]));
    }

    /// S2 of the plan: a writer-invariant violation surfaces as
    /// `MvccError::Internal`, not `panic!()`. We force the
    /// impossible state by rewinding `next_main` after a successful
    /// put — the next put then re-allocates the same rev for the
    /// same user key, which the index's monotonicity check rejects.
    #[tokio::test(flavor = "current_thread")]
    async fn put_index_invariant_violation_returns_internal_not_panic() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v1").await.expect("first put");

        // Force the impossible: rewind `next_main` to the same
        // value the first put consumed. The structural invariant
        // (plan §5.1) is broken by this test hook only.
        store.set_next_main_for_test(1).await;

        let err = store
            .put(b"k", b"v2")
            .await
            .expect_err("must surface invariant violation");
        match err {
            crate::error::MvccError::Internal { context } => {
                assert!(
                    context.contains("monotonic"),
                    "unexpected context: {context}"
                );
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    // === Range (plan §5.3) ===

    use crate::error::MvccError;
    use crate::store::range::RangeRequest;

    fn req_point(key: &[u8]) -> RangeRequest {
        RangeRequest {
            key: key.to_vec(),
            ..RangeRequest::default()
        }
    }

    fn req_range(start: &[u8], end: &[u8]) -> RangeRequest {
        RangeRequest {
            key: start.to_vec(),
            end: end.to_vec(),
            ..RangeRequest::default()
        }
    }

    #[test]
    fn range_empty_store_returns_empty() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let r = store.range(req_point(b"k")).expect("range");
        assert!(r.kvs.is_empty());
        assert_eq!(r.count, 0);
        assert!(!r.more);
        assert_eq!(r.header_revision, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn range_single_key_returns_one_kv() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let rev = store.put(b"k", b"v").await.expect("put");
        let r = store.range(req_point(b"k")).expect("range");
        assert_eq!(r.kvs.len(), 1);
        let kv = &r.kvs[0];
        assert_eq!(kv.key.as_ref(), b"k");
        assert_eq!(kv.value.as_ref(), b"v");
        assert_eq!(kv.create_revision, rev);
        assert_eq!(kv.mod_revision, rev);
        assert_eq!(kv.version, 1);
        assert!(kv.lease.is_none());
        assert_eq!(r.count, 1);
        assert!(!r.more);
        assert_eq!(r.header_revision, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn range_half_open_excludes_end() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"a", b"1").await.expect("a");
        let _ = store.put(b"b", b"2").await.expect("b");
        let _ = store.put(b"c", b"3").await.expect("c");

        // [a, c) — excludes c.
        let r = store.range(req_range(b"a", b"c")).expect("range");
        let keys: Vec<&[u8]> = r.kvs.iter().map(|kv| kv.key.as_ref()).collect();
        assert_eq!(keys, vec![&b"a"[..], &b"b"[..]]);
        assert_eq!(r.count, 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn range_with_limit_sets_more() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        for k in [b"a", b"b", b"c", b"d"] {
            let _ = store.put(k.as_slice(), b"v").await.expect("put");
        }
        let req = RangeRequest {
            key: b"a".to_vec(),
            end: b"e".to_vec(),
            limit: Some(2),
            ..RangeRequest::default()
        };
        let r = store.range(req).expect("range");
        assert_eq!(r.kvs.len(), 2);
        assert!(r.more, "limit hit must set more = true");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn range_count_is_total_matches_not_returned_kvs() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        for k in [b"a", b"b", b"c", b"d"] {
            let _ = store.put(k.as_slice(), b"v").await.expect("put");
        }
        let req = RangeRequest {
            key: b"a".to_vec(),
            end: b"e".to_vec(),
            limit: Some(2),
            ..RangeRequest::default()
        };
        let r = store.range(req).expect("range");
        // M4: count reports the total, ignoring limit.
        assert_eq!(r.kvs.len(), 2);
        assert_eq!(r.count, 4);
        assert!(r.more);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn range_keys_only_omits_value() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let req = RangeRequest {
            key: b"k".to_vec(),
            keys_only: true,
            ..RangeRequest::default()
        };
        let r = store.range(req).expect("range");
        assert_eq!(r.kvs.len(), 1);
        let kv = &r.kvs[0];
        assert_eq!(kv.key.as_ref(), b"k");
        assert!(kv.value.is_empty(), "keys_only must zero the value");
        assert_eq!(kv.version, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn range_count_only_returns_count_no_kvs() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        for k in [b"a", b"b", b"c"] {
            let _ = store.put(k.as_slice(), b"v").await.expect("put");
        }
        let req = RangeRequest {
            key: b"a".to_vec(),
            end: b"d".to_vec(),
            count_only: true,
            ..RangeRequest::default()
        };
        let r = store.range(req).expect("range");
        assert!(r.kvs.is_empty(), "count_only must return no kvs");
        assert_eq!(r.count, 3);
        assert!(!r.more);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn range_at_past_rev_returns_value_at_that_rev() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let r1 = store.put(b"k", b"v1").await.expect("put 1");
        let _ = store.put(b"k", b"v2").await.expect("put 2");
        // Read at r1 — must return v1.
        let req = RangeRequest {
            key: b"k".to_vec(),
            revision: Some(r1.main()),
            ..RangeRequest::default()
        };
        let r = store.range(req).expect("range");
        assert_eq!(r.kvs.len(), 1);
        assert_eq!(r.kvs[0].value.as_ref(), b"v1");
        // Read at head — must return v2.
        let r2 = store.range(req_point(b"k")).expect("range head");
        assert_eq!(r2.kvs[0].value.as_ref(), b"v2");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn range_at_future_rev_returns_future_err() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        // Current is 1; rev=99 is the future.
        let req = RangeRequest {
            key: b"k".to_vec(),
            revision: Some(99),
            ..RangeRequest::default()
        };
        let err = store.range(req).expect_err("must reject future");
        match err {
            MvccError::FutureRevision {
                requested: 99,
                current: 1,
            } => {}
            other => panic!("expected FutureRevision, got {other:?}"),
        }
    }

    #[test]
    fn range_invalid_start_gt_end_returns_invalid_range() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let req = RangeRequest {
            key: b"z".to_vec(),
            end: b"a".to_vec(),
            ..RangeRequest::default()
        };
        let err = store.range(req).expect_err("must reject");
        assert!(matches!(err, MvccError::InvalidRange), "got {err:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn range_equal_start_end_yields_empty_not_invalid() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"a", b"v").await.expect("put");
        // Half-open [a, a) is empty — must NOT be InvalidRange.
        let r = store.range(req_range(b"a", b"a")).expect("range");
        assert!(r.kvs.is_empty());
        assert_eq!(r.count, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn range_skips_tombstoned_at_rev() {
        // Forward-looking: even though DeleteRange isn't shipped
        // until commit 5, we can synthesize the tombstone state
        // through the index directly to confirm the Range path
        // skips a key whose visible version is tombstoned at rev.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        // Tombstone via direct index call — same writer-side state
        // a future DeleteRange will produce.
        store
            .index
            .tombstone(b"k", crate::Revision::new(2, 0))
            .expect("tombstone");
        // Range at the tombstone main: key is not visible.
        let req = RangeRequest {
            key: b"k".to_vec(),
            revision: Some(2),
            ..RangeRequest::default()
        };
        // Snapshot.rev is still 1 (we tombstoned through the
        // index only); rev=2 would be FutureRevision. Bump the
        // published snapshot directly to avoid the future-rev
        // path. L846: writes go through ArcSwap, not an atomic.
        let prev = store.snapshot.load_full();
        store.snapshot.store(Arc::new(Snapshot {
            rev: 2,
            compacted: prev.compacted,
        }));
        let r = store.range(req).expect("range");
        assert!(r.kvs.is_empty(), "tombstoned key must not appear");
        assert_eq!(r.count, 0);
    }

    // === DeleteRange (plan §5.4) ===

    #[tokio::test(flavor = "current_thread")]
    async fn delete_range_returns_count_and_rev() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"a", b"1").await.expect("a");
        let _ = store.put(b"b", b"2").await.expect("b");
        let _ = store.put(b"c", b"3").await.expect("c");
        // Delete [a, c) — covers a and b.
        let (n, rev) = store.delete_range(b"a", b"c").await.expect("delete");
        assert_eq!(n, 2);
        // current was 3 before delete; one main allocated -> 4.
        assert_eq!(rev.main(), 4);
        assert_eq!(rev.sub(), 0);
        assert_eq!(store.current_revision(), 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_range_tombstones_each_key_with_ascending_sub() {
        // S3 of plan §5.4: each tombstoned key consumes one sub,
        // assigned in `keys_in_order` traversal order starting at 0.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"a", b"1").await.expect("a");
        let _ = store.put(b"b", b"2").await.expect("b");
        let _ = store.put(b"c", b"3").await.expect("c");
        let (n, rev) = store.delete_range(b"a", b"d").await.expect("delete");
        assert_eq!(n, 3);
        // Confirm each tombstone is present on disk at
        // (rev.main, sub) for sub in 0..3.
        let snap = store.backend().snapshot().expect("snapshot");
        for sub in 0_i64..3 {
            let key_rev = crate::Revision::new(rev.main(), sub);
            let enc = crate::encoding::encode_key(key_rev, crate::encoding::KeyKind::Tombstone);
            let got = snap
                .get(crate::bucket::KEY_BUCKET_ID, enc.as_bytes())
                .expect("get");
            assert_eq!(
                got.as_deref(),
                Some(&[0u8][..]),
                "missing tombstone sentinel at sub {sub}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_range_then_range_at_post_rev_returns_empty() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let (n, _) = store.delete_range(b"k", b"").await.expect("delete");
        assert_eq!(n, 1);
        // At head (post-tombstone) the key is gone.
        let r = store.range(req_point(b"k")).expect("range head");
        assert!(r.kvs.is_empty());
        assert_eq!(r.count, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_range_then_range_at_pre_rev_still_returns_value() {
        // MVCC invariant: a Range at the pre-delete revision still
        // sees the key. The tombstone applies only at-and-after
        // the delete's main rev.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let put_rev = store.put(b"k", b"v").await.expect("put");
        let (_, _) = store.delete_range(b"k", b"").await.expect("delete");
        let req = RangeRequest {
            key: b"k".to_vec(),
            revision: Some(put_rev.main()),
            ..RangeRequest::default()
        };
        let r = store.range(req).expect("range pre");
        assert_eq!(r.kvs.len(), 1);
        assert_eq!(r.kvs[0].value.as_ref(), b"v");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_range_already_tombstoned_key_excluded_from_count() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let (n1, r1) = store.delete_range(b"k", b"").await.expect("delete 1");
        assert_eq!(n1, 1);
        // Second delete on the now-tombstoned key matches nothing.
        let (n2, r2) = store.delete_range(b"k", b"").await.expect("delete 2");
        assert_eq!(n2, 0);
        // Zero-match path returns the current head, no main advance.
        assert_eq!(r2.main(), r1.main());
        assert_eq!(store.current_revision(), r1.main());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_range_with_empty_end_treats_as_single_key() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"a", b"1").await.expect("a");
        let _ = store.put(b"b", b"2").await.expect("b");
        // Empty `end` -> single-key delete of "a" only.
        let (n, _) = store.delete_range(b"a", b"").await.expect("delete");
        assert_eq!(n, 1);
        // "b" is still live.
        let r = store.range(req_point(b"b")).expect("range b");
        assert_eq!(r.kvs.len(), 1);
        assert_eq!(r.kvs[0].value.as_ref(), b"2");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_range_no_match_returns_zero_and_does_not_advance_main() {
        // S3: empty match set must not allocate a revision.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let put_rev = store.put(b"x", b"v").await.expect("put");
        // Delete [a, c) — no overlap with "x".
        let (n, rev) = store.delete_range(b"a", b"c").await.expect("delete");
        assert_eq!(n, 0);
        assert_eq!(rev.main(), put_rev.main(), "no main advance on zero match");
        assert_eq!(rev.sub(), 0);
        assert_eq!(store.current_revision(), put_rev.main());
    }

    // === Txn compare evaluator (plan §5.5) ===

    use crate::store::txn::{Compare, CompareOp};

    fn all_passed(outcomes: &[bool]) -> bool {
        outcomes.iter().all(|&b| b)
    }

    #[test]
    fn evaluate_compares_empty_list_passes() {
        // M1: empty compare list returns Vec::new(), `all` = true.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let outcomes = store.evaluate_compares(&[]).expect("evaluate");
        assert!(outcomes.is_empty());
        assert!(all_passed(&outcomes));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_compare_version_eq_against_present_key() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        // version is 1 after first put.
        let cmp = Compare::Version {
            key: b"k".to_vec(),
            op: CompareOp::Equal,
            target: 1,
        };
        let outcomes = store.evaluate_compares(&[cmp]).expect("evaluate");
        assert!(all_passed(&outcomes));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_compare_version_eq_zero_against_absent_key_passes() {
        // B4: absent key defaults to version = 0.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let cmp = Compare::Version {
            key: b"absent".to_vec(),
            op: CompareOp::Equal,
            target: 0,
        };
        let outcomes = store.evaluate_compares(&[cmp]).expect("evaluate");
        assert!(all_passed(&outcomes));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_compare_create_revision_zero_against_absent_passes() {
        // B4: absent key defaults to create_revision = 0.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let cmp = Compare::CreateRevision {
            key: b"absent".to_vec(),
            op: CompareOp::Equal,
            target: 0,
        };
        let outcomes = store.evaluate_compares(&[cmp]).expect("evaluate");
        assert!(all_passed(&outcomes));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_compare_mod_revision_against_recently_tombstoned_returns_zero() {
        // M2: a key tombstoned at current is "absent" — its
        // mod_revision compares to 0.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let _ = store.delete_range(b"k", b"").await.expect("delete");
        let cmp_eq_zero = Compare::ModRevision {
            key: b"k".to_vec(),
            op: CompareOp::Equal,
            target: 0,
        };
        let outcomes = store.evaluate_compares(&[cmp_eq_zero]).expect("evaluate");
        assert!(all_passed(&outcomes));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_compare_value_eq_uses_current_value() {
        // M1: value compare reads from the live snapshot, not
        // from any branch RequestOp.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"hello").await.expect("put");
        let cmp = Compare::Value {
            key: b"k".to_vec(),
            op: CompareOp::Equal,
            target: b"hello".to_vec(),
        };
        let outcomes = store.evaluate_compares(&[cmp]).expect("evaluate");
        assert!(all_passed(&outcomes));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_compare_value_eq_empty_against_absent_passes() {
        // B4: absent key defaults to value = b"".
        let store = MvccStore::open(fresh_backend()).expect("open");
        let cmp = Compare::Value {
            key: b"absent".to_vec(),
            op: CompareOp::Equal,
            target: b"".to_vec(),
        };
        let outcomes = store.evaluate_compares(&[cmp]).expect("evaluate");
        assert!(all_passed(&outcomes));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_compare_value_eq_nonempty_against_absent_fails() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let cmp = Compare::Value {
            key: b"absent".to_vec(),
            op: CompareOp::Equal,
            target: b"v".to_vec(),
        };
        let outcomes = store.evaluate_compares(&[cmp]).expect("evaluate");
        assert!(!all_passed(&outcomes));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_compare_value_lex_greater() {
        // Value compares apply lex order for Greater/Less.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"hello").await.expect("put");
        let greater_than_alpha = Compare::Value {
            key: b"k".to_vec(),
            op: CompareOp::Greater,
            target: b"alpha".to_vec(),
        };
        let less_than_zebra = Compare::Value {
            key: b"k".to_vec(),
            op: CompareOp::Less,
            target: b"zebra".to_vec(),
        };
        let outcomes = store
            .evaluate_compares(&[greater_than_alpha, less_than_zebra])
            .expect("evaluate");
        assert!(all_passed(&outcomes));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_compares_short_circuits_on_first_failure_in_outcomes() {
        // Per-compare outcomes are returned index-aligned; a
        // failing compare leaves a `false` at its slot but later
        // compares are still evaluated. (Caller can inspect.)
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let cmp_pass = Compare::Version {
            key: b"k".to_vec(),
            op: CompareOp::Equal,
            target: 1,
        };
        let cmp_fail = Compare::Version {
            key: b"k".to_vec(),
            op: CompareOp::Equal,
            target: 99,
        };
        let cmp_pass2 = Compare::CreateRevision {
            key: b"k".to_vec(),
            op: CompareOp::Greater,
            target: 0,
        };
        let outcomes = store
            .evaluate_compares(&[cmp_pass, cmp_fail, cmp_pass2])
            .expect("evaluate");
        assert_eq!(outcomes, vec![true, false, true]);
        assert!(!all_passed(&outcomes));
    }

    // === Txn read-only branch dispatch (plan §5.5 / §8 commit 7) ===

    use crate::store::txn::{RequestOp, ResponseOp, TxnRequest};

    #[tokio::test(flavor = "current_thread")]
    async fn txn_empty_compare_list_uses_success_branch() {
        // M1: empty compare list succeeds — etcd parity.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let req = TxnRequest {
            compare: vec![],
            success: vec![RequestOp::Range(req_point(b"k"))],
            failure: vec![],
            ..TxnRequest::default()
        };
        let resp = store.txn(req).await.expect("txn");
        assert!(resp.succeeded);
        assert_eq!(resp.responses.len(), 1);
        // Read-only txn does not advance main; header is the
        // pre-txn current.
        assert_eq!(resp.header_revision, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn txn_readonly_does_not_advance_revision() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let pre = store.current_revision();
        let req = TxnRequest {
            compare: vec![],
            success: vec![RequestOp::Range(req_point(b"k"))],
            failure: vec![],
            ..TxnRequest::default()
        };
        let _ = store.txn(req).await.expect("txn");
        assert_eq!(
            store.current_revision(),
            pre,
            "read-only Txn must not advance head"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn txn_compare_version_eq_picks_success_branch() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let cmp = Compare::Version {
            key: b"k".to_vec(),
            op: CompareOp::Equal,
            target: 1,
        };
        let req = TxnRequest {
            compare: vec![cmp],
            success: vec![RequestOp::Range(req_point(b"k"))],
            failure: vec![RequestOp::Range(req_point(b"absent"))],
            ..TxnRequest::default()
        };
        let resp = store.txn(req).await.expect("txn");
        assert!(resp.succeeded);
        match &resp.responses[0] {
            ResponseOp::Range(r) => assert_eq!(r.kvs.len(), 1),
            other => panic!("expected Range, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn txn_compare_failure_picks_failure_branch() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        // Compare fails: version is 1, target is 99.
        let cmp = Compare::Version {
            key: b"k".to_vec(),
            op: CompareOp::Equal,
            target: 99,
        };
        let req = TxnRequest {
            compare: vec![cmp],
            success: vec![RequestOp::Range(req_point(b"k"))],
            failure: vec![RequestOp::Range(req_point(b"k"))],
            ..TxnRequest::default()
        };
        let resp = store.txn(req).await.expect("txn");
        assert!(!resp.succeeded, "compare must fail");
        // Failure branch ran; same Range result content.
        assert_eq!(resp.responses.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn txn_response_ops_align_with_request_ops_readonly() {
        // M1: index alignment between RequestOp and ResponseOp slices.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"a", b"1").await.expect("a");
        let _ = store.put(b"b", b"2").await.expect("b");
        let req = TxnRequest {
            compare: vec![],
            success: vec![
                RequestOp::Range(req_point(b"a")),
                RequestOp::Range(req_point(b"b")),
                RequestOp::Range(req_point(b"absent")),
            ],
            failure: vec![],
            ..TxnRequest::default()
        };
        let resp = store.txn(req).await.expect("txn");
        assert_eq!(resp.responses.len(), 3);
        for (idx, op) in resp.responses.iter().enumerate() {
            match op {
                ResponseOp::Range(r) => {
                    if idx == 2 {
                        assert!(r.kvs.is_empty(), "absent must yield 0 kvs");
                    } else {
                        assert_eq!(r.kvs.len(), 1, "slot {idx} expected 1 kv");
                    }
                }
                other => panic!("slot {idx} expected Range, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn txn_with_only_range_does_not_advance() {
        // Same invariant as `txn_readonly_does_not_advance_revision`
        // but with multiple Range ops in the success branch.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let pre = store.current_revision();
        let req = TxnRequest {
            compare: vec![],
            success: vec![
                RequestOp::Range(req_point(b"k")),
                RequestOp::Range(req_point(b"k")),
            ],
            failure: vec![],
            ..TxnRequest::default()
        };
        let _ = store.txn(req).await.expect("txn");
        assert_eq!(store.current_revision(), pre);
    }

    // === Txn mutating branch dispatch (plan §5.5 / §8 commit 8) ===

    #[tokio::test(flavor = "current_thread")]
    async fn txn_with_one_put_advances_by_1_subs_start_at_0() {
        // Single Put inside a txn advances main by exactly 1 and
        // allocates sub = 0 (etcd parity for a 1-write txn).
        let store = MvccStore::open(fresh_backend()).expect("open");
        let pre = store.current_revision();
        let req = TxnRequest {
            compare: vec![],
            success: vec![RequestOp::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            }],
            failure: vec![],
            ..TxnRequest::default()
        };
        let resp = store.txn(req).await.expect("txn");
        assert!(resp.succeeded);
        assert_eq!(resp.responses.len(), 1);
        match &resp.responses[0] {
            ResponseOp::Put { prev_revision } => assert!(prev_revision.is_none()),
            other => panic!("expected Put, got {other:?}"),
        }
        assert_eq!(
            store.current_revision(),
            pre.checked_add(1).expect("pre+1"),
            "txn with one Put must advance main by 1"
        );
        assert_eq!(resp.header_revision, store.current_revision());
        // Sub allocation: read back at the new rev and confirm
        // the value is visible.
        let r = store.range(req_point(b"k")).expect("range");
        assert_eq!(r.kvs.len(), 1);
        assert_eq!(r.kvs[0].mod_revision.sub(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn txn_put_deleterange_put_subs_increment_per_physical_write() {
        // M1 / S3: subs increment per physical write across the
        // mixed branch — Put(0), DeleteRange-2-keys(1, 2), Put(3).
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"a", b"1").await.expect("a");
        let _ = store.put(b"b", b"2").await.expect("b");
        // current = 2.
        let req = TxnRequest {
            compare: vec![],
            success: vec![
                RequestOp::Put {
                    key: b"x".to_vec(),
                    value: b"x_val".to_vec(),
                },
                RequestOp::DeleteRange {
                    key: b"a".to_vec(),
                    end: b"c".to_vec(),
                },
                RequestOp::Put {
                    key: b"y".to_vec(),
                    value: b"y_val".to_vec(),
                },
            ],
            failure: vec![],
            ..TxnRequest::default()
        };
        let resp = store.txn(req).await.expect("txn");
        assert!(resp.succeeded);
        assert_eq!(resp.responses.len(), 3);
        // Slot 1: DeleteRange counted 2.
        match &resp.responses[1] {
            ResponseOp::DeleteRange { deleted } => assert_eq!(*deleted, 2),
            other => panic!("expected DeleteRange, got {other:?}"),
        }
        // Main advanced by exactly 1 (single txn allocates 1 main).
        assert_eq!(store.current_revision(), 3);
        // x at sub 0; y at sub 3 (after the two tombstones).
        let rx = store.range(req_point(b"x")).expect("range x");
        assert_eq!(rx.kvs[0].mod_revision.main(), 3);
        assert_eq!(rx.kvs[0].mod_revision.sub(), 0);
        let ry = store.range(req_point(b"y")).expect("range y");
        assert_eq!(ry.kvs[0].mod_revision.main(), 3);
        assert_eq!(ry.kvs[0].mod_revision.sub(), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn txn_subs_increment_per_mutating_op() {
        // S3: each mutating op gets its own sub (Put = 1 sub;
        // DeleteRange-N = N subs).
        let store = MvccStore::open(fresh_backend()).expect("open");
        let req = TxnRequest {
            compare: vec![],
            success: vec![
                RequestOp::Put {
                    key: b"k1".to_vec(),
                    value: b"v1".to_vec(),
                },
                RequestOp::Put {
                    key: b"k2".to_vec(),
                    value: b"v2".to_vec(),
                },
            ],
            failure: vec![],
            ..TxnRequest::default()
        };
        let _ = store.txn(req).await.expect("txn");
        let r1 = store.range(req_point(b"k1")).expect("k1");
        let r2 = store.range(req_point(b"k2")).expect("k2");
        assert_eq!(r1.kvs[0].mod_revision.sub(), 0);
        assert_eq!(r2.kvs[0].mod_revision.sub(), 1);
        assert_eq!(r1.kvs[0].mod_revision.main(), r2.kvs[0].mod_revision.main());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn txn_with_zero_match_deleterange_only_does_not_advance_main() {
        // M1 / S3: a branch whose only mutating op is a
        // DeleteRange that matches no live keys is read-only
        // (no main allocation).
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let pre = store.current_revision();
        let req = TxnRequest {
            compare: vec![],
            success: vec![RequestOp::DeleteRange {
                key: b"absent_a".to_vec(),
                end: b"absent_z".to_vec(),
            }],
            failure: vec![],
            ..TxnRequest::default()
        };
        let resp = store.txn(req).await.expect("txn");
        assert!(resp.succeeded);
        match &resp.responses[0] {
            ResponseOp::DeleteRange { deleted } => assert_eq!(*deleted, 0),
            other => panic!("expected DeleteRange, got {other:?}"),
        }
        assert_eq!(store.current_revision(), pre);
        assert_eq!(resp.header_revision, pre);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn txn_success_branch_executes_in_order_with_one_main() {
        // Sanity: two Puts and a Range in the success branch share
        // one main, and the trailing Range sees the just-committed
        // Puts (post-commit visibility, plan §5.5 step 11).
        let store = MvccStore::open(fresh_backend()).expect("open");
        let req = TxnRequest {
            compare: vec![],
            success: vec![
                RequestOp::Put {
                    key: b"a".to_vec(),
                    value: b"1".to_vec(),
                },
                RequestOp::Put {
                    key: b"b".to_vec(),
                    value: b"2".to_vec(),
                },
                RequestOp::Range(RangeRequest {
                    key: b"a".to_vec(),
                    end: b"c".to_vec(),
                    ..RangeRequest::default()
                }),
            ],
            failure: vec![],
            ..TxnRequest::default()
        };
        let resp = store.txn(req).await.expect("txn");
        assert!(resp.succeeded);
        match &resp.responses[2] {
            ResponseOp::Range(r) => {
                assert_eq!(r.kvs.len(), 2, "post-commit Range sees both Puts");
                assert_eq!(r.kvs[0].key.as_ref(), b"a");
                assert_eq!(r.kvs[1].key.as_ref(), b"b");
            }
            other => panic!("expected Range, got {other:?}"),
        }
        assert_eq!(store.current_revision(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn txn_failure_branch_runs_when_any_compare_fails() {
        // Mutating branch on the failure path: compare fails →
        // failure branch's Put executes and main advances.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let cmp = Compare::Version {
            key: b"absent".to_vec(),
            op: CompareOp::Greater,
            target: 0,
        };
        let req = TxnRequest {
            compare: vec![cmp],
            success: vec![],
            failure: vec![RequestOp::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            }],
            ..TxnRequest::default()
        };
        let resp = store.txn(req).await.expect("txn");
        assert!(!resp.succeeded);
        assert_eq!(store.current_revision(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn txn_response_ops_align_with_request_ops_mutating() {
        // Index alignment for a mixed mutating branch.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"a", b"1").await.expect("put a");
        let req = TxnRequest {
            compare: vec![],
            success: vec![
                RequestOp::Range(req_point(b"a")),
                RequestOp::Put {
                    key: b"b".to_vec(),
                    value: b"2".to_vec(),
                },
                RequestOp::DeleteRange {
                    key: b"a".to_vec(),
                    end: b"b".to_vec(),
                },
            ],
            failure: vec![],
            ..TxnRequest::default()
        };
        let resp = store.txn(req).await.expect("txn");
        assert_eq!(resp.responses.len(), 3);
        match &resp.responses[0] {
            ResponseOp::Range(_) => {}
            other => panic!("slot 0 expected Range, got {other:?}"),
        }
        match &resp.responses[1] {
            ResponseOp::Put { .. } => {}
            other => panic!("slot 1 expected Put, got {other:?}"),
        }
        match &resp.responses[2] {
            ResponseOp::DeleteRange { deleted } => assert_eq!(*deleted, 1),
            other => panic!("slot 2 expected DeleteRange, got {other:?}"),
        }
    }

    // === Compact (plan §5.6 / §8 commit 9) ===

    #[tokio::test(flavor = "current_thread")]
    async fn compact_advances_floor() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v1").await.expect("put1");
        let _ = store.put(b"k", b"v2").await.expect("put2");
        store.compact(1).await.expect("compact");
        // Floor advance: a Range at rev 1 still works (B1: floor
        // itself remains readable), but rev 0 returns Compacted.
        let r0 = store.range(RangeRequest {
            key: b"k".to_vec(),
            end: vec![],
            revision: Some(0),
            ..RangeRequest::default()
        });
        match r0 {
            Err(MvccError::Compacted { .. }) => {}
            other => panic!("expected Compacted, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compact_at_or_below_floor_is_noop() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v1").await.expect("put1");
        let _ = store.put(b"k", b"v2").await.expect("put2");
        store.compact(1).await.expect("compact 1");
        // Idempotent: a second compact at the same rev (or lower)
        // is a no-op.
        store.compact(1).await.expect("compact 1 again");
        store.compact(0).await.expect("compact 0");
        // Range at the floor still works (B1).
        let r1 = store
            .range(RangeRequest {
                key: b"k".to_vec(),
                end: vec![],
                revision: Some(1),
                ..RangeRequest::default()
            })
            .expect("range at floor");
        assert_eq!(r1.kvs.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compact_at_future_rev_returns_future_err() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let err = store.compact(99).await.expect_err("future rev");
        match err {
            MvccError::FutureRevision { requested, current } => {
                assert_eq!(requested, 99);
                assert_eq!(current, 1);
            }
            other => panic!("expected FutureRevision, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compact_then_range_at_compacted_rev_succeeds() {
        // B1: Range at the floor itself returns the value visible
        // at that rev.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v1").await.expect("put1");
        let _ = store.put(b"k", b"v2").await.expect("put2");
        store.compact(2).await.expect("compact");
        let r = store
            .range(RangeRequest {
                key: b"k".to_vec(),
                end: vec![],
                revision: Some(2),
                ..RangeRequest::default()
            })
            .expect("range at floor");
        assert_eq!(r.kvs.len(), 1);
        assert_eq!(r.kvs[0].value.as_ref(), b"v2");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compact_then_range_below_floor_returns_compacted_err() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v1").await.expect("put1");
        let _ = store.put(b"k", b"v2").await.expect("put2");
        store.compact(2).await.expect("compact");
        let err = store
            .range(RangeRequest {
                key: b"k".to_vec(),
                end: vec![],
                revision: Some(1),
                ..RangeRequest::default()
            })
            .expect_err("below floor");
        match err {
            MvccError::Compacted { requested, floor } => {
                assert_eq!(requested, 1);
                assert_eq!(floor, 2);
            }
            other => panic!("expected Compacted, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compact_at_current_rev_drops_old_revs() {
        // After compact at current, the on-disk old revisions
        // are physically gone. Probe the backend's emptiness:
        // if the only surviving keys are at >= compacted floor,
        // the floor is enforced.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v1").await.expect("put1");
        let _ = store.put(b"k", b"v2").await.expect("put2");
        let _ = store.put(b"k", b"v3").await.expect("put3");
        store.compact(3).await.expect("compact");
        // Range at current still returns the latest value.
        let r = store.range(req_point(b"k")).expect("range at current");
        assert_eq!(r.kvs.len(), 1);
        assert_eq!(r.kvs[0].value.as_ref(), b"v3");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compact_removes_fully_tombstoned_keys_from_in_mem_set() {
        // R3: a key whose only revs are pre-compaction-tombstoned
        // is reaped from `keys_in_order` after compact, so a
        // post-compaction Range over the wider range no longer
        // sees it.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let _ = store.delete_range(b"k", &[]).await.expect("del");
        // Now compact at the tombstone's main.
        let post_del = store.current_revision();
        store.compact(post_del).await.expect("compact");
        // Range at current must not see the key.
        let r = store
            .range(RangeRequest {
                key: b"a".to_vec(),
                end: b"z".to_vec(),
                ..RangeRequest::default()
            })
            .expect("range");
        assert!(r.kvs.is_empty(), "tombstoned key must be reaped");
        // Direct probe (rust-expert PR #75 review S1/B1): Range
        // silently skips on `RevisionNotFound`, so the assertion
        // above passes whether or not `keys_in_order` was reaped.
        // Probe the in-memory set directly to pin the post-
        // compact invariant.
        assert!(
            !store.keys_in_order.read().contains_key(&b"k"[..]),
            "fully-tombstoned key must be reaped from keys_in_order"
        );
    }

    // === L849 compaction physical-removal byte-level identity ===
    //
    // The three tests above verify post-compact reads, but every
    // assertion goes through the in-memory index. A regression that
    // removed `commit_compaction_deletes` (`store/mod.rs:1144-1168`)
    // entirely would still pass them — old on-disk bytes would just
    // leak silently. The three tests below probe the backend
    // directly, asserting the exact decoded survivor set: rev, kind,
    // and value bytes.

    /// Decoded snapshot of every record in `KEY_BUCKET_ID`, in
    /// backend iteration order (lex over encoded keys).
    ///
    /// Returns `(Revision, KeyKind, value bytes)` per record. The
    /// helper exists for L849 byte-level identity checks; counting
    /// alone is too weak (a regression that deletes the right rev
    /// and writes a different one preserves the count).
    fn key_bucket_survivors<B: Backend>(
        b: &B,
    ) -> Result<
        Vec<(crate::revision::Revision, crate::encoding::KeyKind, Vec<u8>)>,
        mango_storage::BackendError,
    > {
        let snap = b.snapshot()?;
        let iter = snap.range(KEY_BUCKET_ID, &[], super::NON_EMPTY_PROBE_END)?;
        let mut out = Vec::new();
        for item in iter {
            let (k, v) = item?;
            let (rev, kind) = crate::encoding::decode_key(&k).expect("on-disk key must decode");
            out.push((rev, kind, v.to_vec()));
        }
        Ok(out)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compact_physically_removes_old_revs_from_backend() {
        let store = MvccStore::open(fresh_backend()).expect("open");

        let r1 = store.put(b"k", b"v1").await.expect("put1");
        let r2 = store.put(b"k", b"v2").await.expect("put2");
        let r3 = store.put(b"k", b"v3").await.expect("put3");
        assert_eq!((r1.main(), r2.main(), r3.main()), (1, 2, 3));

        let before = key_bucket_survivors(store.backend()).expect("probe");
        assert_eq!(before.len(), 3, "3 puts → 3 backend records");

        store.compact(3).await.expect("compact");

        let after = key_bucket_survivors(store.backend()).expect("probe");
        assert_eq!(
            after,
            vec![(
                crate::revision::Revision::new(3, 0),
                crate::encoding::KeyKind::Put,
                b"v3".to_vec(),
            )],
            "compact at HEAD must keep exactly the latest Put on-disk, byte-for-byte",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compact_at_head_keeps_only_latest_per_key() {
        let store = MvccStore::open(fresh_backend()).expect("open");
        for round in 0..3 {
            for k in [b"a".as_slice(), b"b", b"c"] {
                let _ = store
                    .put(k, format!("v{round}").as_bytes())
                    .await
                    .expect("put");
            }
        }
        let before = key_bucket_survivors(store.backend()).expect("probe");
        assert_eq!(before.len(), 9, "3 keys × 3 rounds → 9 records");

        let head = store.current_revision();
        assert_eq!(head, 9);
        store.compact(head).await.expect("compact");

        // Round 2 wrote v2 to all three keys at revs 7, 8, 9.
        let after = key_bucket_survivors(store.backend()).expect("probe");
        assert_eq!(
            after,
            vec![
                (
                    crate::revision::Revision::new(7, 0),
                    crate::encoding::KeyKind::Put,
                    b"v2".to_vec(),
                ),
                (
                    crate::revision::Revision::new(8, 0),
                    crate::encoding::KeyKind::Put,
                    b"v2".to_vec(),
                ),
                (
                    crate::revision::Revision::new(9, 0),
                    crate::encoding::KeyKind::Put,
                    b"v2".to_vec(),
                ),
            ],
            "compact at HEAD keeps exactly the live KV per key, by rev",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compact_removes_tombstone_bytes_when_keep_drops_them() {
        // The strongest of the three: pins the asymmetry between
        // `KeyHistory::keep` (drops trailing-tombstone-of-non-final-
        // generation) and per-key `KeyHistory::compact` (retains
        // it). `compute_available` uses `keep`, so the on-disk
        // tombstone IS deleted even though the per-key compact
        // would have retained it. If `commit_compaction_deletes`
        // regresses to a no-op this test fails loudest.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let _ = store.put(b"k", b"v").await.expect("put");
        let _ = store.delete_range(b"k", &[]).await.expect("del");
        let head = store.current_revision();
        assert_eq!(head, 2);

        let before = key_bucket_survivors(store.backend()).expect("probe");
        assert_eq!(before.len(), 2, "1 put + 1 tombstone = 2 backend records");
        assert_eq!(before[0].1, crate::encoding::KeyKind::Put);
        assert_eq!(before[1].1, crate::encoding::KeyKind::Tombstone);

        store.compact(head).await.expect("compact");

        let after = key_bucket_survivors(store.backend()).expect("probe");
        assert!(
            after.is_empty(),
            "tombstone-only key compacted at head leaves zero bytes (got {after:?})",
        );
    }

    // === L846 snapshot publication coherence (plan §"New tokio test") ===

    /// Concurrent writer + readers — every observed snapshot
    /// satisfies `compacted <= rev`, and both fields are
    /// monotonically non-decreasing across reader iterations.
    ///
    /// Iteration-counted (not wall-clock) so the same work runs
    /// on slow CI runners and fast laptops; readers run on
    /// `spawn_blocking` so they sit on dedicated threads instead
    /// of being multiplexed onto the writer's worker. The
    /// readers' tight `spin_loop()` loop maximises the chance of
    /// observing a torn pair if one were possible.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn snapshot_publish_is_coherent_pair() {
        use std::sync::atomic::{AtomicBool, Ordering as Ord};

        let store = Arc::new(MvccStore::open(fresh_backend()).expect("open"));
        let stop = Arc::new(AtomicBool::new(false));

        // Writer: 5_000 puts with periodic compact at rev/2.
        // 5_000 (not 50_000 from the plan draft) keeps the test
        // under nextest's 60s default and still gives readers
        // tens of millions of snapshot loads under the
        // multi-thread runtime.
        let writer = {
            let s = Arc::clone(&store);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                for i in 0_i64..5_000 {
                    let key = format!("k{i}");
                    s.put(key.as_bytes(), b"v").await.expect("put");
                    if i % 16 == 15 {
                        let target = s.current_revision() / 2;
                        if target > 0 {
                            s.compact(target).await.expect("compact");
                        }
                    }
                }
                stop.store(true, Ord::Relaxed);
            })
        };

        // Readers: spawn_blocking so they sit on dedicated
        // threads — under the multi-thread runtime each reader
        // races the writer concurrently.
        let mut readers = Vec::new();
        for _ in 0..3 {
            let s = Arc::clone(&store);
            let stop = Arc::clone(&stop);
            readers.push(tokio::task::spawn_blocking(move || {
                let mut prev_rev: i64 = 0;
                let mut prev_compacted: i64 = 0;
                while !stop.load(Ord::Relaxed) {
                    let snap = s.current_snapshot();
                    assert!(
                        snap.compacted <= snap.rev,
                        "torn pair: compacted={} > rev={}",
                        snap.compacted,
                        snap.rev,
                    );
                    assert!(
                        snap.rev >= prev_rev,
                        "rev went backwards: {} -> {}",
                        prev_rev,
                        snap.rev,
                    );
                    assert!(
                        snap.compacted >= prev_compacted,
                        "compacted went backwards: {} -> {}",
                        prev_compacted,
                        snap.compacted,
                    );
                    prev_rev = snap.rev;
                    prev_compacted = snap.compacted;
                    std::hint::spin_loop();
                }
            }));
        }

        writer.await.expect("writer task");
        for r in readers {
            r.await.expect("reader task");
        }

        // Sanity: writer ran to completion; final snapshot has
        // a non-trivial rev and a compacted floor strictly less
        // than rev (the writer compacted at rev/2 throughout).
        let final_snap = store.current_snapshot();
        assert!(final_snap.rev > 0, "writer did at least one put");
        assert!(
            final_snap.compacted < final_snap.rev,
            "compaction floor strictly below head: rev={}, compacted={}",
            final_snap.rev,
            final_snap.compacted,
        );
    }
}
