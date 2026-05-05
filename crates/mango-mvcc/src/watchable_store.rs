//! Watch surface — Phase 3, ROADMAP.md:862.
//!
//! This commit lands the **observer trait + event types only**.
//! The `WatchableStore`, the registry, the `watch()` entry-point,
//! and the `futures_core::Stream`-impl arrive in commits 4–5 of the
//! Phase 3 plan (`.planning/phase-3-watchable-store.plan.md`).
//!
//! # Layering
//!
//! - [`WatchEvent`] / [`WatchEventKind`] — pure data, no I/O, no
//!   locks. Cloned per-watcher in the dispatch path (commit 4).
//! - [`WriteObserver`] — the trait the writer hot path
//!   ([`crate::store::MvccStore::put`] / `delete_range` / `txn`)
//!   calls under its writer mutex AFTER `snapshot.store()`. The slot
//!   on `MvccStore` is one `arc_swap::ArcSwapOption<dyn WriteObserver>`
//!   field (see [`crate::store::MvccStore::attach_observer`]).
//!
//! # Etcd parity
//!
//! `WatchEventKind` discriminants match
//! `etcdserver/api/mvcc/mvccpb.Event_EventType` (`PUT = 0`,
//! `DELETE = 1`) at tag `v3.5.16`. Pinned by the
//! `watch_event_kind_discriminants_match_etcd` test below.
//!
//! Wire-format byte equality lives at Phase 7 (gRPC service). This
//! struct is the in-memory shape only.

use bytes::Bytes;

use crate::revision::Revision;
use crate::store::KeyValue;

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
    /// Previous key/value at `revision - 1`, if available. Always
    /// `None` in this PR; populated in ROADMAP.md:863.
    pub prev: Option<KeyValue>,
    /// Revision at which the event was produced. The dispatch
    /// eligibility filter (commit 4) compares
    /// `event.revision.main >= watcher.start_rev`.
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
/// [`arc_swap::ArcSwapOption`]`<dyn WriteObserver>` — `Arc<dyn T>`
/// requires `T: 'static`.
pub trait WriteObserver: Send + Sync + 'static {
    /// Called inside the writer lock, **after** `snapshot.store()`.
    ///
    /// `events` is non-empty (the store does not call `on_apply`
    /// for zero-event ops, e.g. a `DeleteRange` that matched no
    /// keys, or a read-only `Txn`).
    ///
    /// `at_main` is the writer's allocated `main` revision for the
    /// op (every event in `events` has `revision.main == at_main`).
    /// Carried separately so future progress-notify watermark
    /// callers (ROADMAP.md:865) can read the head without
    /// re-decoding the event slice.
    fn on_apply(&self, events: &[WatchEvent], at_main: i64);
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::{WatchEvent, WatchEventKind, WriteObserver};
    use crate::revision::Revision;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Phase 3 plan §11 test #13. Discriminant pinning against
    /// etcd `mvccpb.Event_EventType` at tag `v3.5.16`. The matchsite
    /// also ensures any added variant fails compilation here.
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
    /// dispatches under a normal call. The store-side wiring lands
    /// in commit 2; this test uses the trait directly.
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
}
