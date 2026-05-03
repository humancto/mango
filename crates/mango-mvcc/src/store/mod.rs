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
//! - `current_main: AtomicI64` — highest fully-published revision.
//!   Release-stored at end of every successful commit; Acquire-
//!   loaded by `Range` and `current_revision`.
//! - `compacted: AtomicI64` — compacted floor. Release-stored
//!   after the on-disk delete commit in `Compact`; Acquire-loaded
//!   by `Range`. Zero = none.
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
pub mod txn;
mod writer;

pub use lease::LeaseId;
pub use range::{KeyValue, RangeRequest, RangeResult};
pub use txn::{Compare, CompareOp, RequestOp, ResponseOp, TxnRequest, TxnResponse};

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::atomic::{AtomicI64, Ordering};

use bytes::Bytes;
use mango_storage::{Backend, ReadSnapshot, WriteBatch};

use crate::bucket::{register, KEY_BUCKET_ID};
use crate::encoding::{encode_key, KeyKind};
use crate::error::{MvccError, OpenError};
use crate::key_history::KeyHistoryError;
use crate::revision::Revision;
use crate::sharded_key_index::{KeyIndexError, ShardedKeyIndex};

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
    /// (will be wrapped in `arc_swap::ArcSwap<Arc<...>>` then),
    /// not a stopgap. Map (not Set) so the watch cache can extend
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
    /// Highest fully-published revision. Release-stored at end of
    /// every successful commit; Acquire-loaded by `Range` /
    /// [`Self::current_revision`].
    current_main: AtomicI64,
    /// Compacted floor. Release-stored after on-disk delete commit
    /// in `Compact`; Acquire-loaded by `Range`. `0` = none.
    /// Read at construction time, set by `Compact` in a later
    /// commit.
    #[allow(dead_code)]
    compacted: AtomicI64,
}

impl<B: Backend> std::fmt::Debug for MvccStore<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MvccStore")
            .field("current_main", &self.current_main.load(Ordering::Relaxed))
            .field("compacted", &self.compacted.load(Ordering::Relaxed))
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
            current_main: AtomicI64::new(0),
            compacted: AtomicI64::new(0),
        })
    }

    /// Highest fully-published revision. Returns `0` on a fresh
    /// store.
    ///
    /// Acquire-loaded; pairs with the writer's Release-store at
    /// the end of every successful commit.
    #[must_use]
    pub fn current_revision(&self) -> i64 {
        self.current_main.load(Ordering::Acquire)
    }

    /// Borrow the underlying backend. Used by writer impls in
    /// subsequent commits; left `pub(crate)` to avoid leaking the
    /// backend into callers (they passed it in).
    #[allow(dead_code)]
    pub(crate) fn backend(&self) -> &B {
        &self.backend
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
    /// 7. Release-store `current_main` so readers see the new head.
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

        let mut batch = self.backend.begin_batch()?;
        let encoded = encode_key(rev, KeyKind::Put);
        batch.put(KEY_BUCKET_ID, encoded.as_bytes(), value)?;
        // No fsync — durability is Raft's WAL contract above this
        // layer (plan §5.2 step 5).
        let _ = self.backend.commit_batch(batch, false).await?;

        // No `.await` is held under either of the in-memory locks
        // below. `keys_in_order`'s guard is dropped before the
        // index is touched, so the lock-ordering edge `writer →
        // keys_in_order → index shard` holds (module docs).
        {
            let mut keys = self.keys_in_order.write();
            // `BTreeMap::insert` is idempotent on identical-key
            // overwrites — the value is `()`. Repeated puts on the
            // same user key keep the entry exactly once.
            let _ = keys.insert(key.into(), ());
        }

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

        let next = state.next_main.checked_add(1).ok_or(MvccError::Internal {
            context: "next_main overflow",
        })?;
        state.next_main = next;

        // Release-store: pairs with the Acquire-load in
        // `current_revision` and (later) `Range`.
        self.current_main.store(rev.main(), Ordering::Release);

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
    /// - [`MvccError::FutureRevision`] if `rev > current_main`.
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

        let current = self.current_main.load(Ordering::Acquire);
        let rev = req_revision.unwrap_or(current);
        let floor = self.compacted.load(Ordering::Acquire);

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
        let current = self.current_main.load(Ordering::Acquire);

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

        // Step 4: filter to keys live at `current` — already-
        // tombstoned keys must not consume a sub or appear on disk
        // a second time.
        let mut matched: Vec<Box<[u8]>> = Vec::with_capacity(candidates.len());
        for k in candidates {
            match self.index.get(&k, current) {
                Ok(_) => matched.push(k),
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

        // Step 7+: allocate a single main; sub increments per
        // physical write.
        let rev = Revision::new(state.next_main, 0);

        let mut batch = self.backend.begin_batch()?;
        let mut sub: i64 = 0;
        // Pair each key with the rev it gets tombstoned at, so the
        // post-commit `index.tombstone` calls reuse the on-disk
        // assignments verbatim.
        let mut tombstones: Vec<(Box<[u8]>, Revision)> = Vec::with_capacity(matched.len());
        for k in matched {
            let key_rev = Revision::new(rev.main(), sub);
            let encoded = encode_key(key_rev, KeyKind::Tombstone);
            batch.put(KEY_BUCKET_ID, encoded.as_bytes(), TOMBSTONE_VALUE)?;
            sub = sub.checked_add(1).ok_or(MvccError::Internal {
                context: "delete_range sub overflow",
            })?;
            tombstones.push((k, key_rev));
        }
        // No fsync — Raft's WAL above us owns durability (parity
        // with `Put`).
        let _ = self.backend.commit_batch(batch, false).await?;

        // Step 11: in-mem tombstones. Holding only the writer lock
        // here; `keys_in_order` is intentionally NOT modified
        // (review item R3). `index.tombstone` returning `Err` would
        // indicate the writer-lock invariant is broken (plan
        // §5.4 review item S3).
        for (k, key_rev) in &tombstones {
            if let Err(_e) = self.index.tombstone(k, *key_rev) {
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

        // Release-store: pairs with the Acquire-load in `Range` /
        // `current_revision`.
        self.current_main.store(rev.main(), Ordering::Release);

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
    /// **This commit (plan §8 commit 7) ships the read-only
    /// path only.** A branch that contains [`RequestOp::Put`]
    /// or [`RequestOp::DeleteRange`] returns
    /// [`MvccError::Internal`] until the mutating-branch
    /// dispatch lands in commit 8. Tests for the read-only
    /// path use only [`RequestOp::Range`] in both branches.
    ///
    /// Read-only txn semantics:
    /// - **No main rev is allocated.** `header_revision`
    ///   reflects the pre-txn `current` (etcd parity for
    ///   `storeTxnRead`).
    /// - The compare list may be empty; an empty list always
    ///   succeeds (review item M1 — etcd parity).
    /// - Range ops inside the chosen branch are executed in
    ///   order; their responses are index-aligned with the
    ///   branch slice.
    ///
    /// # Errors
    ///
    /// - [`MvccError::Backend`] / [`MvccError::KeyIndex`] /
    ///   [`MvccError::KeyDecode`] propagated from compare
    ///   evaluation or branch-op execution.
    /// - [`MvccError::Internal`] if the chosen branch contains
    ///   a mutating [`RequestOp`] (deferred to plan §8 commit
    ///   8). Read-only branches never hit this path.
    pub async fn txn(&self, req: TxnRequest) -> Result<TxnResponse, MvccError> {
        // Hold the writer lock for the entire txn so compare
        // evaluation and branch execution see the same `current`.
        // Read-only txns still take the writer lock — the
        // alternative (RwLock-style upgrade) would need a
        // different primitive and the cost of the async lock
        // acquisition is dwarfed by the work the txn does.
        let _state = self.writer.lock().await;
        let current = self.current_main.load(Ordering::Acquire);

        let outcomes = self.evaluate_compares(&req.compare)?;
        let succeeded = outcomes.iter().all(|&b| b);
        let branch = if succeeded {
            &req.success
        } else {
            &req.failure
        };

        let mut responses: Vec<ResponseOp> = Vec::with_capacity(branch.len());
        for op in branch {
            match op {
                RequestOp::Range(range_req) => {
                    // Re-acquires `current_main` inside `range`,
                    // but the writer lock keeps it stable — the
                    // value matches `current` above. Cloning the
                    // request avoids changing `range`'s public
                    // by-value signature.
                    let result = self.range(range_req.clone())?;
                    responses.push(ResponseOp::Range(result));
                }
                RequestOp::Put { .. } | RequestOp::DeleteRange { .. } => {
                    return Err(MvccError::Internal {
                        context: "mutating Txn ops land in L844 plan §8 commit 8",
                    });
                }
            }
        }

        Ok(TxnResponse {
            succeeded,
            responses,
            header_revision: current,
        })
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
        let current = self.current_main.load(Ordering::Acquire);
        // Snapshot is only needed for `Compare::Value`; skip
        // snapshot acquisition if no value compares appear.
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

    use super::MvccStore;
    use crate::bucket::{KEY_BUCKET_ID, KEY_INDEX_BUCKET_ID};
    use crate::error::OpenError;
    use mango_storage::{Backend, BackendConfig, InMemBackend, ReadSnapshot};

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
        // current_main is still 1 (we tombstoned through the
        // index only); rev=2 would be FutureRevision. Bump head
        // via the test hook to avoid the future-rev path.
        store
            .current_main
            .store(2, std::sync::atomic::Ordering::Release);
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

    #[tokio::test(flavor = "current_thread")]
    async fn txn_mutating_branch_returns_internal_until_commit_8() {
        // Boundary check: commit 7's read-only dispatch refuses
        // mutating ops with a typed Internal error. Removed when
        // commit 8 lands the mutating branch.
        let store = MvccStore::open(fresh_backend()).expect("open");
        let req = TxnRequest {
            compare: vec![],
            success: vec![RequestOp::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            }],
            failure: vec![],
            ..TxnRequest::default()
        };
        let err = store.txn(req).await.expect_err("must reject for now");
        match err {
            MvccError::Internal { context } => assert!(
                context.contains("commit 8"),
                "unexpected context: {context}"
            ),
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}
