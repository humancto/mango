//! Watch surface — Phase 3, ROADMAP.md:862.
//!
//! [`WatchableStore`] wraps an [`MvccStore`] and dispatches every applied
//! write to registered watchers whose range covers the event key **and**
//! whose `start_rev` permits the event. Watchers receive events in commit
//! order via a [`WatchStream`] (a `futures_core::Stream` of
//! `Result<WatchEvent, WatchError>`).
//!
//! # Layering
//!
//! - [`WatchEvent`] / [`WatchEventKind`] — pure data, no I/O, no
//!   locks. Cloned per-watcher in the dispatch path.
//! - [`WriteObserver`] — the trait the writer hot path
//!   ([`crate::store::MvccStore::put`] / `delete_range` / `txn`)
//!   calls under its writer mutex AFTER `snapshot.store()`. The slot
//!   on `MvccStore` is one `arc_swap::ArcSwap<Option<Arc<dyn WriteObserver>>>`
//!   field (see [`crate::store::MvccStore::attach_observer`]).
//! - [`WatchableStore`] — the registry + the `WriteObserver` impl.
//!   Single-tenant per `MvccStore` (the observer slot is single-occupancy).
//! - [`WatchStream`] — the per-watcher stream returned by `watch()`.
//!
//! # Etcd parity
//!
//! `WatchEventKind` discriminants match
//! `etcdserver/api/mvcc/mvccpb.Event_EventType` (`PUT = 0`,
//! `DELETE = 1`) at tag `v3.5.16`. Pinned by the
//! `watch_event_kind_discriminants_match_etcd` test below.
//!
//! Wire-format byte equality lives at Phase 7 (gRPC service). This
//! module is the in-memory shape only.
//!
//! # Race-free `start_rev` resolution
//!
//! `watch()` reads `current_revision()` **inside** the registry write-lock,
//! which is mutex-exclusive with the dispatch path's registry read-lock.
//! Plus the writer mutex serializes `snapshot.store()` with dispatch's
//! registry-read acquisition. Together the two locks pin the
//! "registration vs dispatch" interleaving: every event with `revision >=
//! resolved_start` is delivered exactly once, and no event with
//! `revision < resolved_start` is delivered. See Phase 3 plan §4.4 for
//! the full proof.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use mango_storage::Backend;
use parking_lot::{Mutex as PlMutex, RwLock};
use pin_project_lite::pin_project;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, OwnedPermit};
use tokio::task::AbortHandle;

use crate::error::MvccError;
use crate::revision::Revision;
use crate::store::{KeyValue, MvccStore};

/// Watch event kind. Discriminant values match etcd
/// `mvccpb.Event_EventType` (`PUT = 0`, `DELETE = 1`); pinned by
/// the `watch_event_kind_discriminants_match_etcd` test in this
/// module.
///
/// **Intentionally exhaustive.** Etcd's `mvccpb.Event_EventType`
/// has exactly these two variants at tag `v3.5.16` and adding a
/// third would itself be an etcd-protocol break. The
/// `clippy::exhaustive_enums` deny is suppressed locally — same
/// rationale as the wire-format enums in `mango-proto`
/// (`crates/mango-proto/src/lib.rs`).
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
#[repr(i32)]
#[allow(
    clippy::exhaustive_enums,
    reason = "etcd mvccpb.Event_EventType is exhaustive at v3.5.16"
)]
pub enum WatchEventKind {
    /// `put(key, value) -> rev` produced this event.
    Put = 0,
    /// A `DeleteRange` tombstoned this key at the carried revision.
    Delete = 1,
}

/// One event delivered to a watcher.
///
/// Etcd parity: `mvccpb.Event { type, kv, prev_kv }`. `prev` is
/// wired in this PR but populated as `None`; ROADMAP.md:863
/// populates it from the writer hot path
/// (`index.get(key, rev-1)` + `backend.get(value)`) without
/// re-touching this struct or [`WriteObserver`].
///
/// `#[non_exhaustive]` so future field additions (e.g. `prev`
/// becoming non-`None`) are non-breaking inside the crate.
#[derive(Clone, Eq, PartialEq, Debug)]
#[non_exhaustive]
pub struct WatchEvent {
    /// Whether this event was produced by a `Put` or a `Delete`.
    pub kind: WatchEventKind,
    /// User key. Cheap-clone via `Bytes`.
    pub key: Bytes,
    /// User value. Empty for [`WatchEventKind::Delete`].
    pub value: Bytes,
    /// Previous key/value at `revision - 1`, if available.
    /// Captured by the writer hot path (single-key `put` /
    /// `delete_range` and the txn first pass) BEFORE any index
    /// mutation, against a single `Backend::snapshot` per writer
    /// call. `None` for `Put` when the key was absent or tombstoned
    /// at the moment the writer ran; always `Some` for `Delete`
    /// since tombstones target live keys only. Phase 3 plan §4.6
    /// (ROADMAP.md:863).
    pub prev: Option<KeyValue>,
    /// Revision at which the event was produced. The dispatch
    /// eligibility filter compares
    /// `event.revision.main() >= watcher.start_rev`.
    pub revision: Revision,
}

// Compile-time guarantee that `WatchEvent` is `Send + Sync + Clone`.
// ROADMAP.md:865's progress-notify ticker will run on a `tokio` task
// and serialize events across `.await` points; this static-assert
// regresses to a build error if a future field breaks the bound.
const _: fn() = || {
    fn assert_send_sync_clone<T: Send + Sync + Clone>() {}
    assert_send_sync_clone::<WatchEvent>();
};

/// Writer-side observer hook for the MVCC store.
///
/// The store calls [`Self::on_apply`] **inside its writer mutex,
/// after `snapshot.store()`** (see Phase 3 plan §4.2 — the ordering
/// is load-bearing for the `start_rev` race-free contract). `events`
/// is non-empty and ordered by sub-revision within `at_main`.
///
/// Implementors MUST NOT take locks that any read/writer path on
/// the same store may also hold; the call happens under
/// `MvccStore`'s writer `tokio::sync::Mutex` and a long-running
/// observer stalls every other writer.
///
/// `'static` because the slot on `MvccStore` is
/// [`arc_swap::ArcSwap`]`<Option<Arc<dyn WriteObserver>>>` —
/// `Arc<dyn T>` requires `T: 'static`.
pub trait WriteObserver: Send + Sync + 'static {
    /// Called inside the writer lock, **after** `snapshot.store()`.
    ///
    /// `events` is non-empty (the store does not call `on_apply`
    /// for zero-event ops, e.g. a `DeleteRange` that matched no
    /// keys, or a read-only `Txn`).
    ///
    /// `at_main` is the writer's allocated `main` revision for the
    /// op (every event in `events` has `revision.main() == at_main`).
    /// Carried separately so future progress-notify watermark
    /// callers (ROADMAP.md:865) can read the head without
    /// re-decoding the event slice.
    fn on_apply(&self, events: &[WatchEvent], at_main: i64);
}

/// Reason a watcher was force-dropped from the registry.
///
/// Carried on the [`WatchError::Disconnected`] terminal item so callers can
/// distinguish "your channel filled" from "the store shut down."
/// **Trigger heuristic** for `SlowConsumer` is item 866's call; this PR
/// commits only to the signal shape.
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
#[allow(
    clippy::exhaustive_enums,
    reason = "shape committed in Phase 3 + ROADMAP.md:863; reasons map 1:1 to terminal disconnect causes the watcher-side match must handle exhaustively. Adding a reason should be a deliberate visible API change."
)]
pub enum DisconnectReason {
    /// The per-watcher channel could not accept an event.
    SlowConsumer,
    /// The owning [`WatchableStore`] (and its dispatch path) was dropped.
    StoreDropped,
    /// Catch-up scan tried to replay events from a revision the
    /// store has already compacted away. `floor` is the
    /// post-compaction floor — the watcher cannot recover this range.
    /// Phase 3 plan §4.5 (ROADMAP.md:863).
    Compacted {
        /// Lowest revision still retained on disk.
        floor: i64,
    },
    /// Catch-up driver hit `MAX_CATCHUP_ATTEMPTS` without converging
    /// (sustained writer outpaced the scan rate). Terminal —
    /// caller must re-watch with a fresh `start_rev`. Phase 3 plan §4.3.
    CatchupConvergenceFailed {
        /// Number of catch-up loop iterations attempted.
        attempts: u32,
    },
    /// Internal error inside the catch-up driver (e.g. backend I/O
    /// during the scan). Carries a static context string so the
    /// reason is typed and observable without scraping a side
    /// channel. Terminal.
    Internal {
        /// Short, static description of the failure site.
        context: &'static str,
    },
}

/// Watcher-side error variants. Surfaced as `Stream::Item =
/// Result<WatchEvent, WatchError>` so the watcher can distinguish a
/// closed channel "because we disconnected you" from "because the
/// store dropped" without scraping a side channel.
#[derive(Clone, Copy, Eq, PartialEq, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WatchError {
    /// Watcher was force-dropped from the registry. The terminal item on
    /// the stream; no further events will be delivered.
    #[error("watcher disconnected: {0:?}")]
    Disconnected(DisconnectReason),
}

/// Channel capacity for the per-watcher mpsc.
///
/// 1024 slots are exposed to the event-send hot path (see
/// [`EVENT_CAPACITY`]). One additional slot is reserved at registration
/// time via an [`OwnedPermit`] held on the [`Watcher`] record, so the
/// terminal `Err(WatchError::Disconnected(_))` is **always deliverable**
/// even when the event channel is full.
///
/// True channel capacity is therefore `EVENT_CAPACITY + 1 = 1025`.
const CHANNEL_CAPACITY: usize = 1025;

/// Number of slots in the per-watcher mpsc that are exposed to the
/// event-send path. The `+ 1`-th slot of the channel is reserved for
/// the terminal disconnect signal (see [`CHANNEL_CAPACITY`]).
///
/// Etcd's default is 128; ours is 8× generous for item-1 tests, with
/// item 866 to tune.
#[allow(
    dead_code,
    reason = "documentation constant; the value is implicit in CHANNEL_CAPACITY - 1"
)]
const EVENT_CAPACITY: usize = 1024;

/// Monotonic id for a registered watcher.
type WatcherId = u64;

/// Catch-up driver retry cap. After this many trips through
/// [`catchup_drive_inner`]'s scan loop without converging, the
/// watcher receives a terminal
/// [`DisconnectReason::CatchupConvergenceFailed`].
///
/// Phase 3 plan §4.3 (ROADMAP.md:863). The cap exists because a
/// watcher whose scan rate cannot keep up with the writer rate would
/// otherwise spin forever, holding a registry slot and a backend
/// snapshot per attempt. 256 attempts is etcd-flavored — sustained
/// outpacing of a several-thousand-events scan budget per attempt
/// implies the watcher is structurally unable to catch up; failing
/// fast lets the caller re-watch with a fresh `start_rev`.
const MAX_CATCHUP_ATTEMPTS: u32 = 256;

/// Half-open key range `[start, end)`. `end.is_empty()` denotes a
/// single-key watch (etcd parity — etcd uses `end_key.is_empty()` to
/// signal point-lookup on `key`).
#[derive(Clone, Eq, PartialEq, Debug)]
struct WatcherRange {
    start: Bytes,
    end: Bytes,
}

impl WatcherRange {
    /// Range membership test. `end.is_empty()` means `key == start`.
    fn covers(&self, key: &[u8]) -> bool {
        if self.end.is_empty() {
            self.start.as_ref() == key
        } else {
            self.start.as_ref() <= key && key < self.end.as_ref()
        }
    }
}

/// One registered watcher.
struct Watcher {
    id: WatcherId,
    /// Resolved start revision (no `0` placeholder remains here — the
    /// `0 → current+1` resolution happens in `watch()` under the
    /// registry write-lock; see Phase 3 plan §4.4).
    start_rev: i64,
    /// Range covered by this watch.
    range: WatcherRange,
    /// Per-watcher event channel. Capacity exposed to dispatch is 1024
    /// (one slot is reserved on a sibling `Sender` clone via
    /// `disconnect_permit`).
    tx: mpsc::Sender<Result<WatchEvent, WatchError>>,
    /// Permit reserved at registration time on a clone of `tx`. Holds
    /// one slot of channel capacity for the terminal
    /// `Err(WatchError::Disconnected(_))`. `None` after consumption.
    /// The store-shutdown disconnect path (commit 5 of the Phase 3
    /// plan) consumes this permit during `WatchableStore::drop`.
    disconnect_permit: PlMutex<Option<OwnedPermit<Result<WatchEvent, WatchError>>>>,
    /// `true` once dispatch has tripped a disconnect on this watcher.
    /// Skip-flag for the dispatch inner loop within a single `on_apply`
    /// call — cross-call dedup is enforced by the `registry.write()`
    /// removal at the bottom of `on_apply`. Relaxed ordering is
    /// sufficient because no other state is published through this
    /// flag (see Phase 3 plan §4.3).
    pending_disconnect: AtomicBool,
}

impl Watcher {
    /// Take the reserved permit (if not already taken) and send the
    /// terminal Err. The send is **infallible** because the permit
    /// holds one slot of capacity that is not exposed to the
    /// event-send path. Idempotent.
    fn consume_disconnect_permit(&self, reason: DisconnectReason) {
        if let Some(permit) = self.disconnect_permit.lock().take() {
            // OwnedPermit::send returns the Sender (now with one fewer
            // permit reserved). We drop it; the channel still has the
            // original `tx` for any in-flight cleanup.
            let _sender = permit.send(Err(WatchError::Disconnected(reason)));
        }
    }
}

/// Watcher under active catch-up. Held in [`Registry::unsynced`]
/// while the [`catchup_drive`] task replays historical events from
/// `start_rev` to the writer-locked snapshot revision; promoted to
/// the [`Watcher`] form (and moved to `synced`) when caught up.
///
/// The id is the [`Registry::unsynced`] `HashMap` key; not duplicated
/// here (unlike [`Watcher`], where `id` is needed because the
/// dispatch loop iterates `values()` rather than `iter()`).
///
/// Phase 3 plan §4.1 / §4.3 (ROADMAP.md:863).
struct CatchupWatcher {
    range: WatcherRange,
    /// Producer-side clone the catch-up driver uses for
    /// `tx.send().await`. Constructed via `tx.clone()` at watcher
    /// build time.
    tx: mpsc::Sender<Result<WatchEvent, WatchError>>,
    /// Reserved disconnect slot; same shape and idempotency
    /// contract as [`Watcher::disconnect_permit`].
    disconnect_permit: PlMutex<Option<OwnedPermit<Result<WatchEvent, WatchError>>>>,
    /// Highest `rev.main()` already delivered on this watcher.
    /// Initialised to `start_rev - 1`. Updated by the catch-up
    /// driver after each scan iteration succeeds; read by
    /// [`promote_to_synced`] under `registry.write()` to gate the
    /// retry-vs-promote decision.
    ///
    /// Stored as `Arc<AtomicI64>` (NOT inside the registry RwLock):
    /// the driver task owns writes; `promote_to_synced` reads it
    /// via `load(Acquire)`. `AtomicI64` is the right primitive —
    /// single writer, occasional reader, no need to serialize
    /// against other watchers' updates.
    last_delivered: Arc<AtomicI64>,
    /// `AbortHandle` for the spawned driver task. Drop paths call
    /// `.abort()` to ensure the driver does not outlive the
    /// watcher's registration. `.abort()` is idempotent.
    driver: AbortHandle,
}

impl CatchupWatcher {
    /// Same idempotent permit-consume contract as
    /// [`Watcher::consume_disconnect_permit`].
    fn consume_disconnect_permit(&self, reason: DisconnectReason) {
        if let Some(permit) = self.disconnect_permit.lock().take() {
            let _sender = permit.send(Err(WatchError::Disconnected(reason)));
        }
    }
}

/// Outcome of [`Registry::remove`] — either the watcher was found
/// in the synced or unsynced group, or it was already gone. The
/// `Unsynced` arm carries the [`CatchupWatcher`] so the caller can
/// abort its driver task; the `Synced` arm carries no payload
/// because nothing group-specific is needed (the `Watcher` is
/// dropped inside `Registry::remove` along with its `Sender`,
/// which closes the channel to the consumer).
enum RegistryRemoval {
    Synced,
    Unsynced(CatchupWatcher),
    None,
}

/// Catch-up driver exit reason. The driver returns this from its
/// inner loop (`catchup_drive_inner`); the outer `catchup_drive`
/// fn uses the variant to decide whether to send a terminal
/// disconnect via the reserved permit.
///
/// `Disconnect(_)` is the only variant that produces a terminal
/// item on the watcher's stream — the others are silent exits
/// (the registry entry is already gone or the consumer is gone).
///
/// Phase 3 plan §4.3 (ROADMAP.md:863).
#[derive(Debug)]
enum DriverExit {
    /// `tx.send().await` returned `SendError` — the receiver
    /// (`WatchStream`) was dropped. The `PinnedDrop` already removed
    /// the registry entry; nothing more to do.
    ReceiverGone,
    /// `Weak::upgrade` of the [`WatchableStore`] failed. The store
    /// dropped while we were running; its `Drop` impl already
    /// drained both registries and emitted `StoreDropped` to every
    /// watcher.
    StoreGone,
    /// The watcher's registry entry was removed (e.g. `PinnedDrop`
    /// raced our scan, or the store-drop path drained it). Silent
    /// exit; whoever removed the entry owns the terminal item.
    WatcherGone,
    /// Terminal disconnect — the outer `catchup_drive` consumes
    /// the disconnect permit with this reason before returning.
    Disconnect(DisconnectReason),
}

/// Outcome of `promote_to_synced`. `Done` and `WatcherGone` are
/// terminal for the driver loop; `Retry` means the writer outpaced
/// the catch-up scan in the time between releasing the snapshot-pair
/// mutex and acquiring `registry.write()`, and the driver should
/// run another scan iteration.
enum PromoteResult {
    Done,
    Retry,
    WatcherGone,
}

/// Routing decision made under `registry.write()` in
/// [`WatchableStore::watch`]: either register as synced (resolved
/// `start_rev` ready for the inline-dispatch eligibility filter) or
/// register as unsynced (driver task to be spawned, scan loop to
/// drive [`start_rev`, `current_revision()`]).
enum WatchRoute {
    Synced { resolved_start: i64 },
    Unsynced { start_rev: i64 },
}

/// Watcher registry. Behind a [`parking_lot::RwLock`].
///
/// `next_id` is a monotonic `u64`. At ~1B watch registrations/sec the
/// counter rolls over in ~584 years, so the `checked_add` in
/// [`WatchableStore::watch`] is paranoia-belt-suspenders rather than a
/// realistic guard — the `Internal` arm just makes the failure mode
/// typed instead of a panic.
///
/// # Synced vs unsynced split (Phase 3 plan §4.4 / ROADMAP.md:863)
///
/// Watchers fall into one of two disjoint groups:
///
/// - `synced` — caught up to `current_revision()`; receive every new
///   event from the inline writer dispatch path.
/// - `unsynced` — registered with `start_rev <= current_revision()`
///   at the time of [`WatchableStore::watch`]; the catch-up driver
///   (`catchup_drive`) replays history events
///   `[start_rev, snap_rev]` from the MVCC store before promoting
///   the watcher to `synced` via [`promote_to_synced`].
///
/// A given `WatcherId` appears in **at most one** map at any time.
struct Registry {
    next_id: WatcherId,
    /// Synced watchers — receive events from the inline writer hot path.
    synced: HashMap<WatcherId, Watcher>,
    /// Unsynced watchers — under catch-up. Inline dispatch SKIPS them;
    /// `catchup_drive` is the sole producer.
    unsynced: HashMap<WatcherId, CatchupWatcher>,
}

impl Registry {
    fn new() -> Self {
        Self {
            next_id: 0,
            synced: HashMap::new(),
            unsynced: HashMap::new(),
        }
    }

    /// Total live watchers across both groups. Used by
    /// [`WatchableStore::watcher_count`] so the externally-visible
    /// count is invariant under group transitions.
    fn total_len(&self) -> usize {
        self.synced.len().saturating_add(self.unsynced.len())
    }

    /// Remove watcher `id` from whichever group holds it. Returns
    /// a typed [`RegistryRemoval`] so the caller can dispatch
    /// group-specific cleanup. The id space is shared and disjoint
    /// across groups, so at most one removal succeeds.
    ///
    /// On `Synced` the removed [`Watcher`] (and its `Sender`) is
    /// dropped inside this function — the channel closes for the
    /// consumer immediately. On `Unsynced` the [`CatchupWatcher`] is
    /// returned so the caller can abort its driver task before
    /// dropping (and closing the channel).
    fn remove(&mut self, id: WatcherId) -> RegistryRemoval {
        if let Some(_w) = self.synced.remove(&id) {
            return RegistryRemoval::Synced;
        }
        if let Some(cw) = self.unsynced.remove(&id) {
            return RegistryRemoval::Unsynced(cw);
        }
        RegistryRemoval::None
    }
}

/// Distributed-KV watch surface.
///
/// Wraps an [`MvccStore`] and dispatches every applied write to registered
/// watchers whose range covers the event key **and** whose `start_rev`
/// permits the event. Watchers receive events in commit order, in-order
/// per watcher.
///
/// # Lifetime / drop
///
/// Hold the `Arc<WatchableStore<B>>` for as long as you want events
/// to be dispatched. Dropping the last `Arc<WatchableStore<B>>`
/// sends a terminal
/// [`WatchError::Disconnected`]`(DisconnectReason::StoreDropped)` to
/// each active watcher and removes them from the registry (see
/// [`Drop for WatchableStore`](#impl-Drop-for-WatchableStore<B>)).
/// Subsequent writes against the underlying `MvccStore` invoke the
/// trampoline observer, which finds the [`Weak`] failed to upgrade
/// and is a no-op.
///
/// # Reference shape (no `Arc` cycle)
///
/// `WatchableStore` holds a strong [`Arc`] to its [`MvccStore`], but
/// the observer slot on `MvccStore` holds a strong [`Arc`] to a
/// **trampoline** observer that itself holds only a [`Weak`] back to
/// the `WatchableStore`. Dropping the user's last
/// `Arc<WatchableStore>` therefore causes the `WatchableStore` to
/// drop (Weak ≠ ownership), which cleans up the registry and emits
/// `StoreDropped`. The trampoline lives until the `MvccStore` itself
/// is dropped (or another observer is attached); its only state is a
/// single-pointer [`Weak`] plus the `dyn` vtable.
pub struct WatchableStore<B: Backend> {
    store: Arc<MvccStore<B>>,
    registry: Arc<RwLock<Registry>>,
}

/// Trampoline observer holding a [`Weak`] back to its
/// [`WatchableStore`]. Lives in the [`MvccStore`] observer slot as
/// `Arc<dyn WriteObserver>`; on each `on_apply` it upgrades the
/// `Weak` and delegates to [`WatchableStore::dispatch`]. If the
/// upgrade fails (i.e. the user dropped the last
/// `Arc<WatchableStore>`), the call is a no-op.
///
/// This indirection breaks the would-be `Arc` cycle between
/// `WatchableStore` and `MvccStore`. See the doc on
/// [`WatchableStore`].
struct ObserverHandle<B: Backend> {
    target: Weak<WatchableStore<B>>,
}

impl<B: Backend> WriteObserver for ObserverHandle<B> {
    fn on_apply(&self, events: &[WatchEvent], at_main: i64) {
        if let Some(ws) = self.target.upgrade() {
            ws.dispatch(events, at_main);
        }
        // else: WatchableStore has been dropped. No watchers can be
        // reachable; nothing to do.
    }
}

impl<B: Backend> std::fmt::Debug for WatchableStore<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchableStore")
            .field("watcher_count", &self.watcher_count())
            .finish_non_exhaustive()
    }
}

impl<B: Backend> WatchableStore<B> {
    /// Wrap an existing store. Idempotent **per `MvccStore`**: a single
    /// store can have at most one `WatchableStore` (the observer slot
    /// is single-occupancy, gated by an `AtomicBool` CAS).
    ///
    /// # Errors
    ///
    /// - [`MvccError::Internal`] if the underlying store already has an
    ///   observer attached (`context: "observer slot already occupied"`).
    pub fn new(store: Arc<MvccStore<B>>) -> Result<Arc<Self>, MvccError> {
        let this = Arc::new(Self {
            store,
            registry: Arc::new(RwLock::new(Registry::new())),
        });
        // The observer slot on MvccStore takes a strong Arc<dyn
        // WriteObserver>. To avoid an Arc cycle (WatchableStore →
        // MvccStore → WatchableStore), we install a trampoline that
        // owns only a Weak back to `this`. Dropping the user's last
        // Arc<WatchableStore> therefore drops the WatchableStore
        // (running its Drop impl); subsequent writes upgrade-and-fail
        // on the Weak and become no-ops.
        let handle: Arc<dyn WriteObserver> = Arc::new(ObserverHandle {
            target: Arc::downgrade(&this),
        });
        this.store.attach_observer(handle)?;
        Ok(this)
    }

    /// Register a watcher over the half-open range `[range_start, range_end)`.
    ///
    /// `range_end.is_empty()` denotes a single-key watch (etcd parity).
    ///
    /// # `start_rev` semantics
    ///
    /// The eligibility check at dispatch is `event.revision.main() >=
    /// watcher.start_rev`. Resolution at registration:
    ///
    /// - `start_rev == 0` → resolved under the registry write-lock to
    ///   `current_revision() + 1`. The watcher receives every event strictly
    ///   after the rev observable at registration time. Registered as
    ///   **synced**. **No race.**
    /// - `start_rev > 0` and `start_rev > current_revision()` → registered
    ///   verbatim as **synced**. `current_revision() + 1` (the explicit
    ///   synced-from-now case) lands here unambiguously.
    /// - `start_rev > 0` and `start_rev > compacted_floor` and
    ///   `start_rev <= current_revision()` → registered as **unsynced** and
    ///   a per-watcher catch-up driver task is spawned (Phase 3 plan §4.3 /
    ///   ROADMAP.md:863). The driver replays history events in
    ///   `[start_rev, current_revision()]` against an atomic
    ///   `(mvcc_snap, backend_snap)` pair, then promotes the watcher to
    ///   synced.
    /// - `start_rev > 0` and `start_rev <= compacted_floor` →
    ///   [`MvccError::Compacted`] (the compacted floor is the inclusive
    ///   boundary — etcd parity).
    /// - `start_rev < 0` → [`MvccError::FutureRevision`] (`< 0` is structurally
    ///   future-of-nothing; etcd rejects the same way).
    ///
    /// The `current_revision()` read happens **inside** the registry
    /// write-lock that also inserts the watcher, sequenced with concurrent
    /// dispatches. See Phase 3 plan §4.4 for the proof.
    ///
    /// # Cancellation safety
    ///
    /// `WatchStream::poll_next` is cancel-safe under `tokio::select!`: it
    /// delegates to `tokio::sync::mpsc::Receiver::poll_recv`, which is
    /// documented cancel-safe in tokio 1.x.
    ///
    /// # Errors
    ///
    /// - [`MvccError::FutureRevision`] when `start_rev < 0`.
    /// - [`MvccError::Compacted`] when `start_rev` is at or below the
    ///   compaction floor.
    /// - [`MvccError::InvalidRange`] when `range_end` is non-empty and
    ///   strictly less than `range_start`.
    /// - [`MvccError::Internal`] for `current_revision` / watcher-id
    ///   overflow or `try_reserve_owned` failure (none reachable in
    ///   normal operation).
    pub fn watch(
        self: &Arc<Self>,
        range_start: Bytes,
        range_end: Bytes,
        start_rev: i64,
    ) -> Result<WatchStream, MvccError> {
        if start_rev < 0 {
            return Err(MvccError::FutureRevision {
                requested: start_rev,
                current: self.store.current_revision(),
            });
        }
        if !range_end.is_empty() && range_end.as_ref() < range_start.as_ref() {
            return Err(MvccError::InvalidRange);
        }

        // Acquire registry write-lock. The dispatch path takes registry
        // READ-lock under the writer-tokio-mutex; both cannot run
        // concurrently. Inside this lock we read the published Snapshot
        // exactly once so `(rev, compacted)` are coherent — see Phase 3
        // plan §4.4 race proof.
        let mut reg = self.registry.write();
        let snap = self.store.current_snapshot();
        let current = snap.rev;
        let compacted_floor = snap.compacted;

        // Decide synced vs unsynced.
        let route: WatchRoute = if start_rev == 0 {
            let resolved = current.checked_add(1).ok_or(MvccError::Internal {
                context: "current_revision overflow",
            })?;
            WatchRoute::Synced {
                resolved_start: resolved,
            }
        } else if start_rev > current {
            WatchRoute::Synced {
                resolved_start: start_rev,
            }
        } else if start_rev <= compacted_floor {
            return Err(MvccError::Compacted {
                requested: start_rev,
                floor: compacted_floor,
            });
        } else {
            // compacted_floor < start_rev <= current — unsynced.
            WatchRoute::Unsynced { start_rev }
        };

        let id = reg.next_id;
        reg.next_id = reg.next_id.checked_add(1).ok_or(MvccError::Internal {
            context: "watcher id overflow",
        })?;

        // Channel cap=1025: 1024 slots exposed for events, 1 slot reserved
        // for the terminal Err. We reserve the disconnect slot via
        // `try_reserve_owned()` on a clone of the sender, so:
        //   - the original `tx` retains its full Sender capability for events,
        //   - the OwnedPermit holds 1 slot of capacity (channel sees 1024
        //     remaining for events),
        //   - on disconnect, dispatch consumes the permit; send is infallible.
        // The channel is empty here, so try_reserve_owned trivially succeeds.
        let (tx, rx) = mpsc::channel::<Result<WatchEvent, WatchError>>(CHANNEL_CAPACITY);
        let disconnect_permit =
            tx.clone()
                .try_reserve_owned()
                .map_err(|_| MvccError::Internal {
                    context: "reserve disconnect permit",
                })?;

        let range = WatcherRange {
            start: range_start,
            end: range_end,
        };

        match route {
            WatchRoute::Synced { resolved_start } => {
                reg.synced.insert(
                    id,
                    Watcher {
                        id,
                        start_rev: resolved_start,
                        range,
                        tx,
                        disconnect_permit: PlMutex::new(Some(disconnect_permit)),
                        pending_disconnect: AtomicBool::new(false),
                    },
                );
                drop(reg);
            }
            WatchRoute::Unsynced { start_rev } => {
                // Spawn the driver task BEFORE inserting into the
                // registry — `tokio::spawn` returns a JoinHandle whose
                // AbortHandle is what we store on the CatchupWatcher.
                // The driver will quickly take `registry.read()` for
                // its first iteration; that's safe because we still
                // hold `registry.write()` here, so the driver's read
                // queues behind us.
                //
                // Initial last_delivered = start_rev - 1 so the first
                // scan iteration covers [start_rev, snap_rev].
                let last_delivered = Arc::new(AtomicI64::new(start_rev.saturating_sub(1)));
                let weak_self = Arc::downgrade(self);
                let driver_handle = tokio::spawn(catchup_drive(weak_self, id));
                let driver_abort = driver_handle.abort_handle();

                reg.unsynced.insert(
                    id,
                    CatchupWatcher {
                        range,
                        tx,
                        disconnect_permit: PlMutex::new(Some(disconnect_permit)),
                        last_delivered,
                        driver: driver_abort,
                    },
                );
                drop(reg);
            }
        }

        Ok(WatchStream {
            rx,
            registry: Arc::downgrade(&self.registry),
            id,
        })
    }

    /// Borrow the underlying store. Used by tests and callers that issue
    /// `Put` / `Range` ops alongside a watch.
    #[must_use]
    pub fn store(&self) -> &Arc<MvccStore<B>> {
        &self.store
    }

    /// Number of registered watchers across both `synced` and
    /// `unsynced` groups. Invariant under catch-up promotion.
    /// Public (not test-only) so a future `mango-server` admin /
    /// observability surface can read it.
    #[must_use]
    pub fn watcher_count(&self) -> usize {
        self.registry.read().total_len()
    }
}

impl<B: Backend> WatchableStore<B> {
    /// Inline dispatch from the writer hot path. Invoked by the
    /// trampoline observer ([`ObserverHandle`]) under the writer's
    /// `tokio::sync::Mutex`, AFTER `snapshot.store()` (the
    /// dispatch-after-store ordering invariant; see Phase 3 plan
    /// §4.2).
    fn dispatch(&self, events: &[WatchEvent], at_main: i64) {
        // `at_main` is reserved for ROADMAP.md:865's progress-notify
        // watermark; unused in the inline-dispatch implementation but
        // taken here so the trait shape is forward-compatible.
        let _ = at_main;
        let reg = self.registry.read();
        let mut to_remove: Vec<WatcherId> = Vec::new();

        for w in reg.synced.values() {
            if w.pending_disconnect.load(Ordering::Relaxed) {
                continue;
            }

            for ev in events {
                if ev.revision.main() < w.start_rev {
                    continue;
                }
                if !w.range.covers(&ev.key) {
                    continue;
                }

                match w.tx.try_send(Ok(ev.clone())) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        // Eager-disconnect placeholder. Trigger heuristic
                        // tuning is item 866's call; the *signal* shape is
                        // committed here. The terminal Err is delivered
                        // through the reserved permit held in the
                        // Watcher (see channel capacity comment above).
                        w.pending_disconnect.store(true, Ordering::Relaxed);
                        w.consume_disconnect_permit(DisconnectReason::SlowConsumer);
                        to_remove.push(w.id);
                        break;
                    }
                    Err(TrySendError::Closed(_)) => {
                        // Receiver gone (Drop ran). No event delivery
                        // needed; no terminal Err needed (consumer
                        // initiated drop); just deregister.
                        to_remove.push(w.id);
                        break;
                    }
                }
            }
        }
        drop(reg);

        if !to_remove.is_empty() {
            let mut reg = self.registry.write();
            for id in to_remove {
                reg.synced.remove(&id);
            }
        }
    }

    // -------- Catch-up driver helpers (Phase 3 plan §4.3 / §4.4) --------
    //
    // These are intentionally narrow accessors that take `registry.read()`
    // (or `registry.write()` for `promote_to_synced`) for the minimum
    // duration needed and never hold the lock across `.await`. They are
    // `pub(crate)`-equivalent to the `catchup_drive` free function below
    // — they live on `impl WatchableStore` only because they need access
    // to `self.registry` and `self.store`.

    /// Read `last_delivered` for an unsynced watcher via the
    /// `Arc<AtomicI64>` sidecar. Returns `None` if the watcher
    /// has been removed from the registry (e.g. `PinnedDrop` ran).
    fn read_last_delivered(&self, id: WatcherId) -> Option<i64> {
        let reg = self.registry.read();
        let cw = reg.unsynced.get(&id)?;
        Some(cw.last_delivered.load(Ordering::Acquire))
    }

    /// Update `last_delivered` for an unsynced watcher. Silently
    /// no-ops if the watcher is gone (the driver is racing a
    /// drop; the abort will hit shortly).
    fn write_last_delivered(&self, id: WatcherId, value: i64) {
        let reg = self.registry.read();
        if let Some(cw) = reg.unsynced.get(&id) {
            cw.last_delivered.store(value, Ordering::Release);
        }
    }

    /// Clone the unsynced watcher's range under the registry read
    /// lock. Returns `None` if the watcher is no longer in the
    /// unsynced group (gone or already promoted).
    fn unsynced_range(&self, id: WatcherId) -> Option<WatcherRange> {
        let reg = self.registry.read();
        reg.unsynced.get(&id).map(|cw| cw.range.clone())
    }

    /// Clone the unsynced watcher's `Sender` for an `await`-bearing
    /// `tx.send(...).await` — caller MUST NOT hold the registry
    /// read lock across the await. Returns `None` if the watcher
    /// is gone.
    fn unsynced_tx_clone(
        &self,
        id: WatcherId,
    ) -> Option<mpsc::Sender<Result<WatchEvent, WatchError>>> {
        let reg = self.registry.read();
        reg.unsynced.get(&id).map(|cw| cw.tx.clone())
    }

    /// Idempotent terminal-disconnect for an unsynced watcher.
    /// Consumes the reserved disconnect permit (if not already
    /// taken) and removes the registry entry. Caller is the
    /// catch-up driver's outer wrapper on a `Disconnect(_)` exit.
    fn consume_unsynced_permit(&self, id: WatcherId, reason: DisconnectReason) {
        let cw = {
            let mut reg = self.registry.write();
            reg.unsynced.remove(&id)
        };
        if let Some(cw) = cw {
            cw.consume_disconnect_permit(reason);
            // `cw` (and its `Sender`) drops here — channel closes
            // for the receiver after the terminal Err is consumed.
        }
    }

    /// Race-free synced transition for an unsynced watcher.
    ///
    /// Phase 3 plan §4.4 (ROADMAP.md:863). Holds `registry.write()`
    /// for the duration: under that lock, no inline dispatch can
    /// run (dispatch takes `registry.read()`), and no other writer
    /// can publish a new snapshot in a way the watcher would miss
    /// — between `snapshot.store(R)` and `dispatch(...)` the
    /// writer holds its own `tokio::sync::Mutex` with NO `.await`
    /// point (Invariant W, see `MvccStore::dispatch_observer`).
    /// Therefore reading `current_revision()` here observes a
    /// snapshot whose dispatch (if any) has either fully completed
    /// already (events already in the channel via the catch-up
    /// scan, or in transit through the inline path — but the
    /// inline path skips unsynced entries) or is queued behind us
    /// on `registry.read()`.
    fn promote_to_synced(&self, id: WatcherId) -> PromoteResult {
        let mut reg = self.registry.write();
        let snap_rev = self.store.current_revision();
        let Some(cw_ref) = reg.unsynced.get(&id) else {
            return PromoteResult::WatcherGone;
        };
        let last_delivered = cw_ref.last_delivered.load(Ordering::Acquire);

        if last_delivered < snap_rev {
            return PromoteResult::Retry;
        }

        // Move the watcher across groups under the same lock
        // acquisition. We can't unwrap-with-message since clippy
        // denies expect_used; the Some-check above guarantees the
        // remove succeeds, but we still surface a typed fall-through
        // if it somehow doesn't (would mean another path mutated
        // the registry under our write lock — impossible).
        let Some(cw) = reg.unsynced.remove(&id) else {
            return PromoteResult::WatcherGone;
        };
        // Synced eligibility filter is `event.rev.main() >=
        // start_rev`; setting `start_rev = last_delivered + 1`
        // gives the watcher exactly the events it has not seen
        // yet. last_delivered = snap_rev means start_rev =
        // snap_rev + 1, which is the rev the next writer will
        // allocate.
        let new_start_rev = last_delivered.saturating_add(1);
        reg.synced.insert(
            id,
            Watcher {
                id,
                start_rev: new_start_rev,
                range: cw.range,
                tx: cw.tx,
                disconnect_permit: PlMutex::new(cw.disconnect_permit.lock().take()),
                pending_disconnect: AtomicBool::new(false),
            },
        );
        // The driver's AbortHandle (`cw.driver`) drops here. That's
        // fine — the driver task is about to return Ok(()) once
        // promote returns Done; dropping the handle without aborting
        // is allowed (tokio: dropping a JoinHandle/AbortHandle just
        // detaches the task).
        PromoteResult::Done
    }
}

/// Catch-up driver entrypoint. Spawned by [`WatchableStore::watch`]
/// for each watcher routed onto the unsynced path. Runs the inner
/// scan/promote loop until convergence, then publishes a terminal
/// disconnect via the reserved permit on `Disconnect(_)` exits.
///
/// Phase 3 plan §4.3 (ROADMAP.md:863).
async fn catchup_drive<B: Backend>(ws: Weak<WatchableStore<B>>, id: WatcherId) {
    let outcome = catchup_drive_inner(&ws, id).await;
    if let Err(DriverExit::Disconnect(reason)) = outcome {
        if let Some(strong) = ws.upgrade() {
            strong.consume_unsynced_permit(id, reason);
        }
        // If upgrade fails, the WatchableStore is already mid-Drop;
        // its Drop impl is what's draining the registry and emitting
        // StoreDropped. The reason we computed here is superseded —
        // StoreDropped wins.
    }
}

/// Catch-up scan/promote loop. See Phase 3 plan §4.3.
async fn catchup_drive_inner<B: Backend>(
    ws: &Weak<WatchableStore<B>>,
    id: WatcherId,
) -> Result<(), DriverExit> {
    let mut attempts: u32 = 0;
    loop {
        let strong = ws.upgrade().ok_or(DriverExit::StoreGone)?;

        // Atomic snapshot-pair under the writer mutex. The pair
        // observes the same point in time w.r.t. concurrent
        // compact/put/delete_range/txn — closes the v2-flagged
        // mid-scan compaction race (Phase 3 plan §4.5).
        let (mvcc_snap, snap_be) =
            strong
                .store
                .snapshot_pair_under_writer()
                .await
                .map_err(|err| match err {
                    MvccError::Backend(_) => DriverExit::Disconnect(DisconnectReason::Internal {
                        context: "snapshot_pair backend error",
                    }),
                    _ => DriverExit::Disconnect(DisconnectReason::Internal {
                        context: "snapshot_pair internal error",
                    }),
                })?;

        let last_delivered = strong
            .read_last_delivered(id)
            .ok_or(DriverExit::WatcherGone)?;
        let from_rev = last_delivered.saturating_add(1);

        // Mid-scan compaction guard. The snapshot-pair just taken
        // includes the published `compacted` floor; if it has moved
        // past `from_rev` we cannot recover this watcher's range.
        if mvcc_snap.compacted >= from_rev {
            return Err(DriverExit::Disconnect(DisconnectReason::Compacted {
                floor: mvcc_snap.compacted,
            }));
        }

        let to_rev = mvcc_snap.rev;

        if from_rev > to_rev {
            // Caught up. Try the synced transition.
            // NB: at registration time we required `start_rev <=
            // current_revision()` (or `start_rev == 0`) to route
            // unsynced; the resolved `start_rev - 1` initial
            // last_delivered was therefore `< current_revision()`,
            // so on the FIRST iteration `from_rev > to_rev` cannot
            // occur unless the snapshot's `rev` decreased between
            // registration and the snapshot-pair read (impossible
            // — current_revision is monotonic). On subsequent
            // iterations it's the convergence path.
            match strong.promote_to_synced(id) {
                PromoteResult::Done => return Ok(()),
                PromoteResult::Retry => { /* fall through to attempt-cap check */ }
                PromoteResult::WatcherGone => return Err(DriverExit::WatcherGone),
            }
        } else {
            // Look up the watcher's range under a brief read-lock.
            // We don't hold the lock across the scan or the awaits
            // below.
            let range = strong.unsynced_range(id).ok_or(DriverExit::WatcherGone)?;

            let events = match strong.store.catchup_scan(
                range.start.as_ref(),
                range.end.as_ref(),
                from_rev,
                to_rev,
                &mvcc_snap,
                &snap_be,
            ) {
                Ok(events) => events,
                Err(MvccError::Compacted { floor, .. }) => {
                    return Err(DriverExit::Disconnect(DisconnectReason::Compacted {
                        floor,
                    }));
                }
                Err(_other) => {
                    return Err(DriverExit::Disconnect(DisconnectReason::Internal {
                        context: "catchup_scan backend or index error",
                    }));
                }
            };

            // Drop the snapshot pair before awaiting on tx.send().
            // `B::Snapshot` may hold a backend transaction handle
            // (redb `ReadTransaction`); keeping it alive across an
            // unbounded await stalls compaction reclamation.
            drop(snap_be);
            drop(mvcc_snap);

            for ev in events {
                let tx = strong
                    .unsynced_tx_clone(id)
                    .ok_or(DriverExit::WatcherGone)?;
                // Backpressure: a slow consumer parks the driver
                // here. Unsupported as a DoS surface — Phase 6
                // admission control owns the hard cap on unsynced
                // count per store.
                tx.send(Ok(ev))
                    .await
                    .map_err(|_| DriverExit::ReceiverGone)?;
            }
            // After the batch lands in the channel, advance
            // last_delivered to `to_rev`. Order matters: only
            // update *after* every event in [from_rev, to_rev]
            // has been accepted, so a promote racing the next
            // iteration sees a sound watermark (no in-flight
            // events not yet on the channel).
            strong.write_last_delivered(id, to_rev);
        }

        // Drop strong ref before next iteration so the WatchableStore
        // can drop if all user Arcs are released.
        drop(strong);

        attempts = attempts.saturating_add(1);
        if attempts >= MAX_CATCHUP_ATTEMPTS {
            return Err(DriverExit::Disconnect(
                DisconnectReason::CatchupConvergenceFailed { attempts },
            ));
        }
    }
}

impl<B: Backend> Drop for WatchableStore<B> {
    /// Drain the registry and emit a terminal
    /// [`WatchError::Disconnected`]`(`[`DisconnectReason::StoreDropped`]`)`
    /// to every active watcher.
    ///
    /// Safety / soundness:
    ///
    /// - The terminal send is **infallible** because each watcher
    ///   holds a reserved [`OwnedPermit`] in its `disconnect_permit`
    ///   slot. Consuming the permit places the `Err(StoreDropped)`
    ///   on the channel without contending for capacity.
    /// - After the drain, the [`Sender`](mpsc::Sender) inside each
    ///   `Watcher` is dropped (registry entry removed), which closes
    ///   the channel. The watcher's [`WatchStream`] receives the
    ///   `Err(StoreDropped)` followed by `None`.
    /// - This `Drop` cannot recurse into the trampoline observer:
    ///   the observer's `Weak::upgrade` is what produced the `Arc`
    ///   that's about to be dropped; while we run, no other
    ///   `Arc<WatchableStore>` exists.
    fn drop(&mut self) {
        let mut reg = self.registry.write();
        let synced = std::mem::take(&mut reg.synced);
        let unsynced = std::mem::take(&mut reg.unsynced);
        // Drop the lock before consuming permits — the per-watcher
        // permit consumption only touches that watcher's own
        // `disconnect_permit` slot, no cross-locking needed.
        drop(reg);
        for (_id, w) in synced {
            w.consume_disconnect_permit(DisconnectReason::StoreDropped);
            // `w` (and its `Sender`) is dropped at the end of this
            // iteration; the channel closes for the receiver.
        }
        for (_id, cw) in unsynced {
            // Abort the catch-up driver task FIRST. The driver may
            // be parked on `tx.send().await` with a slow consumer;
            // we don't want it observing the consumed permit and
            // racing to send a duplicate terminal item. `.abort()`
            // is idempotent and the spawned task picks it up at the
            // next yield point (or immediately if not yet started).
            cw.driver.abort();
            cw.consume_disconnect_permit(DisconnectReason::StoreDropped);
            // `cw` (and its `Sender`) drops at end of iteration.
        }
    }
}

pin_project! {
    /// Stream of [`WatchEvent`]s for one registered watcher.
    ///
    /// `Unpin` (auto-implemented through `pin-project-lite`'s projection
    /// over `mpsc::Receiver`, which is itself `Unpin`).
    ///
    /// `Drop` removes the watcher from the registry; if the
    /// [`WatchableStore`] has already been dropped, `Drop` is a no-op
    /// (the registry `Arc` cannot be upgraded from the `Weak`).
    ///
    /// # Cancellation safety
    ///
    /// [`Self::poll_next`] delegates to
    /// [`tokio::sync::mpsc::Receiver::poll_recv`], which is documented
    /// cancel-safe in tokio 1.x.
    pub struct WatchStream {
        #[pin]
        rx: mpsc::Receiver<Result<WatchEvent, WatchError>>,
        registry: Weak<RwLock<Registry>>,
        id: WatcherId,
    }

    impl PinnedDrop for WatchStream {
        fn drop(this: Pin<&mut Self>) {
            // pin-project-lite's PinnedDrop block exposes a Pin<&mut
            // Self> -> projected fields with no user `unsafe`. We
            // only touch unpinned fields (registry, id).
            let this = this.project();
            if let Some(reg) = this.registry.upgrade() {
                // parking_lot::RwLock::write is sync. Under the inline-
                // dispatch design this lock is contended for at most a
                // single observer call duration. Phase 3 plan §11 bench
                // case 2 measures the p99 at C=100 watchers; item 866
                // refactors to a deregister channel if the bench trips
                // the T4 threshold.
                let mut w = reg.write();
                // Watcher may live in either group depending on
                // catch-up state; the `Registry::remove` helper
                // tries both. Id space is disjoint across groups
                // (Phase 3 plan §4.4 / ROADMAP.md:863).
                let removed = w.remove(*this.id);
                // Drop the registry lock BEFORE aborting the driver
                // task — `AbortHandle::abort` is sync and cheap, but
                // we don't need the lock held for it.
                drop(w);
                if let RegistryRemoval::Unsynced(cw) = removed {
                    // Stream consumer is gone; the catch-up driver
                    // would otherwise either (a) park on `tx.send().await`
                    // until the closed channel surfaces a SendError on
                    // its next attempt, or (b) burn CPU on the scan
                    // loop. Abort terminates it deterministically.
                    cw.driver.abort();
                }
            }
            // mpsc::Receiver::Drop closes the receiver; any in-flight
            // writer try_send on this watcher's tx fails with
            // TrySendError::Closed, which the dispatch path handles via
            // the same to_remove route. Two-mechanism safety net.
        }
    }
}

impl std::fmt::Debug for WatchStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchStream")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl WatchStream {
    /// Poll for the next event. `None` means the watcher's channel
    /// closed (the [`WatchableStore`] was dropped). `Some(Ok(ev))` is a
    /// regular event; `Some(Err(WatchError::Disconnected(_)))` is the
    /// terminal item before close.
    ///
    /// # Cancellation safety
    ///
    /// Cancel-safe under `tokio::select!`. Delegates to
    /// [`tokio::sync::mpsc::Receiver::recv`].
    pub async fn recv(&mut self) -> Option<Result<WatchEvent, WatchError>> {
        self.rx.recv().await
    }
}

impl Stream for WatchStream {
    type Item = Result<WatchEvent, WatchError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        this.rx.as_mut().poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        reason = "test code: panics are the assertion mechanism, arithmetic bounds are loop counters"
    )]

    use super::{
        DisconnectReason, WatchError, WatchEvent, WatchEventKind, WatchableStore, WatcherRange,
        WriteObserver,
    };
    use crate::error::MvccError;
    use crate::revision::Revision;
    use crate::store::MvccStore;
    use bytes::Bytes;
    use mango_storage::{Backend, BackendConfig, InMemBackend};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn open() -> Arc<MvccStore<InMemBackend>> {
        let backend = InMemBackend::open(BackendConfig::new("/unused".into(), false))
            .expect("inmem open never fails");
        Arc::new(MvccStore::open(backend).expect("fresh open"))
    }

    /// Phase 3 plan §11 test #13. Discriminant pinning against
    /// etcd `mvccpb.Event_EventType` at tag `v3.5.16`.
    #[test]
    fn watch_event_kind_discriminants_match_etcd() {
        assert_eq!(WatchEventKind::Put as i32, 0);
        assert_eq!(WatchEventKind::Delete as i32, 1);
    }

    #[test]
    fn watch_event_constructable_with_all_fields() {
        let _e = WatchEvent {
            kind: WatchEventKind::Put,
            key: Bytes::from_static(b"k"),
            value: Bytes::from_static(b"v"),
            prev: None,
            revision: Revision::new(1, 0),
        };
    }

    /// Confirms a recording observer wired via `Arc<dyn WriteObserver>`
    /// dispatches under a normal call.
    #[test]
    fn write_observer_can_be_invoked_via_dyn_arc() {
        struct Counter {
            calls: AtomicUsize,
        }
        impl WriteObserver for Counter {
            fn on_apply(&self, events: &[WatchEvent], _at_main: i64) {
                assert!(!events.is_empty());
                self.calls.fetch_add(1, Ordering::Relaxed);
            }
        }
        let c: Arc<dyn WriteObserver> = Arc::new(Counter {
            calls: AtomicUsize::new(0),
        });
        let ev = WatchEvent {
            kind: WatchEventKind::Put,
            key: Bytes::from_static(b"k"),
            value: Bytes::from_static(b"v"),
            prev: None,
            revision: Revision::new(7, 0),
        };
        c.on_apply(&[ev], 7);
    }

    /// Phase 3 plan §11 test #14. Smoke test: double-attach via
    /// `WatchableStore::new` returns `Internal`.
    #[test]
    fn observer_double_attach_rejects() {
        let store = open();
        let _ws1 = WatchableStore::new(Arc::clone(&store)).expect("first new");
        let err = WatchableStore::new(Arc::clone(&store)).expect_err("second new rejected");
        match err {
            MvccError::Internal { context } => {
                assert_eq!(context, "observer slot already occupied");
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    /// Watcher-range membership invariants (single-key + half-open).
    #[test]
    fn watcher_range_covers_single_key() {
        let r = WatcherRange {
            start: Bytes::from_static(b"a"),
            end: Bytes::new(),
        };
        assert!(r.covers(b"a"));
        assert!(!r.covers(b"b"));
        assert!(!r.covers(b""));
    }

    #[test]
    fn watcher_range_covers_half_open() {
        let r = WatcherRange {
            start: Bytes::from_static(b"a"),
            end: Bytes::from_static(b"c"),
        };
        assert!(r.covers(b"a"));
        assert!(r.covers(b"b"));
        assert!(!r.covers(b"c"));
        assert!(!r.covers(b"d"));
    }

    /// Phase 3 plan §11 test #9. `start_rev < 0` returns `FutureRevision`.
    #[test]
    fn start_rev_negative_returns_future_revision() {
        let store = open();
        let ws = WatchableStore::new(store).expect("new");
        let err = ws
            .watch(Bytes::from_static(b"a"), Bytes::new(), -1)
            .expect_err("negative rejected");
        match err {
            MvccError::FutureRevision { requested, .. } => {
                assert_eq!(requested, -1);
            }
            other => panic!("expected FutureRevision, got {other:?}"),
        }
    }

    /// Phase 3 plan §11 test #10. Inverted half-open range returns
    /// `InvalidRange`.
    #[test]
    fn invalid_range_returns_invalid_range() {
        let store = open();
        let ws = WatchableStore::new(store).expect("new");
        let err = ws
            .watch(Bytes::from_static(b"b"), Bytes::from_static(b"a"), 0)
            .expect_err("inverted range rejected");
        assert!(matches!(err, MvccError::InvalidRange), "got {err:?}");
    }

    /// `watcher_count` reflects registrations and drop-time deregistrations.
    #[test]
    fn watcher_count_tracks_lifecycle() {
        let store = open();
        let ws = WatchableStore::new(store).expect("new");
        assert_eq!(ws.watcher_count(), 0);
        let s1 = ws
            .watch(Bytes::from_static(b"a"), Bytes::new(), 0)
            .expect("watch 1");
        assert_eq!(ws.watcher_count(), 1);
        let s2 = ws
            .watch(Bytes::from_static(b"b"), Bytes::new(), 0)
            .expect("watch 2");
        assert_eq!(ws.watcher_count(), 2);
        drop(s1);
        assert_eq!(ws.watcher_count(), 1);
        drop(s2);
        assert_eq!(ws.watcher_count(), 0);
    }

    /// Verifies that `DisconnectReason::StoreDropped` is in the public
    /// surface (consumers will pattern-match on it once the store-shutdown
    /// path is wired in commit 5).
    #[test]
    fn disconnect_reason_variants_are_distinguishable() {
        assert_ne!(
            DisconnectReason::SlowConsumer,
            DisconnectReason::StoreDropped
        );
    }

    /// `WatchError` carries a `DisconnectReason` and `Display` includes
    /// the variant name.
    #[test]
    fn watch_error_display_includes_reason() {
        let e = WatchError::Disconnected(DisconnectReason::SlowConsumer);
        let msg = format!("{e}");
        assert!(msg.contains("SlowConsumer"), "msg: {msg}");
        let e = WatchError::Disconnected(DisconnectReason::StoreDropped);
        let msg = format!("{e}");
        assert!(msg.contains("StoreDropped"), "msg: {msg}");
    }

    /// Compile-time: `WatcherRange::covers` is sound under empty start +
    /// non-empty end (the always-leading sentinel matches keys ≤ end).
    #[test]
    fn watcher_range_empty_start_covers_prefix() {
        let r = WatcherRange {
            start: Bytes::new(),
            end: Bytes::from_static(b"\xff"),
        };
        assert!(r.covers(b""));
        assert!(r.covers(b"a"));
        assert!(r.covers(b"\xfe"));
        assert!(!r.covers(b"\xff"));
    }

    /// L863 unit test #C0. `start_rev > 0 && start_rev <= current_revision()`
    /// is now accepted: the watcher is registered into the unsynced
    /// group and a catch-up driver is spawned. We assert the
    /// registration outcome only — the dispatched events are
    /// asserted in the integration tests in `tests/watch_catchup.rs`.
    #[tokio::test(flavor = "current_thread")]
    async fn start_rev_below_current_registers_unsynced_watcher() {
        let store = open();
        store.put(b"k", b"v").await.expect("put");
        assert_eq!(store.current_revision(), 1);
        let ws = WatchableStore::new(store).expect("new");
        let _stream = ws
            .watch(Bytes::from_static(b"k"), Bytes::new(), 1)
            .expect("unsynced watcher accepted");
        // total_len is 1 across synced + unsynced; the watcher may
        // already have been promoted to synced if the driver woke
        // before the assertion, so don't probe individual groups.
        assert_eq!(ws.watcher_count(), 1);
    }

    /// L863 unit test #C0b. `MvccError::Compacted` is still returned
    /// when `start_rev <= compacted_floor` — the catch-up path is
    /// only available against revisions still on disk.
    #[tokio::test(flavor = "current_thread")]
    async fn start_rev_below_compacted_floor_returns_compacted() {
        let store = open();
        // Create a few revisions then compact past them.
        store.put(b"k", b"v1").await.expect("put");
        store.put(b"k", b"v2").await.expect("put");
        store.put(b"k", b"v3").await.expect("put");
        store.compact(2).await.expect("compact");
        let ws = WatchableStore::new(store).expect("new");
        let err = ws
            .watch(Bytes::from_static(b"k"), Bytes::new(), 1)
            .expect_err("rejected as compacted");
        assert!(matches!(err, MvccError::Compacted { .. }), "got {err:?}");
    }
}
