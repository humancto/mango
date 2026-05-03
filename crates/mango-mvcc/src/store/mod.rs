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

use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;
use std::sync::atomic::{AtomicI64, Ordering};

use bytes::Bytes;
use mango_storage::{Backend, ReadSnapshot, WriteBatch};

use crate::bucket::{register, KEY_BUCKET_ID};
use crate::encoding::{decode_key, encode_key, KeyKind};
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
    },
    /// Zero or more sub-allocated tombstones for a single
    /// [`RequestOp::DeleteRange`]. Empty when no live keys
    /// matched (the op contributes no physical writes).
    Delete {
        /// `(matched_key, allocated_rev)` pairs in match order.
        tombs: Vec<Tombstone>,
    },
}

/// One matched-key / allocated-revision pair inside an
/// [`OpPlan::Delete`] entry.
type Tombstone = (Box<[u8]>, Revision);

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
    /// Compacted floor. Release-stored after the on-disk delete
    /// commit in [`Self::compact`]; Acquire-loaded by `Range`.
    /// `0` = none.
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
        // below. Ordering is **index first, then `keys_in_order`**
        // (rust-expert PR #75 review R1): a concurrent reader
        // observing the new `current_main` (Release-stored at the
        // end of this fn) can scan `keys_in_order` and probe the
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
        let mut tombstones: Vec<Tombstone> = Vec::with_capacity(matched.len());
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
    /// batch commits and `current_main` advances (plan §5.5
    /// step 11).
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
        let current = self.current_main.load(Ordering::Acquire);

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
        // apply in-mem updates, then advance current_main.
        let head_rev = Revision::new(txn_main, 0);
        self.commit_txn_batch(&plan).await?;
        self.apply_txn_in_mem(&plan)?;

        let next = state.next_main.checked_add(1).ok_or(MvccError::Internal {
            context: "next_main overflow",
        })?;
        state.next_main = next;
        // Release-store: pairs with the Acquire-load in `Range` /
        // `current_revision`. Range ops below now see the post-
        // commit state.
        self.current_main.store(head_rev.main(), Ordering::Release);

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
    fn build_txn_plan(
        &self,
        branch: &[RequestOp],
        current: i64,
        txn_main: i64,
    ) -> Result<(Vec<OpPlan>, usize), MvccError> {
        let mut plan: Vec<OpPlan> = Vec::with_capacity(branch.len());
        let mut sub: i64 = 0;
        let mut total: usize = 0;
        for op in branch {
            match op {
                RequestOp::Range(_) => plan.push(OpPlan::Read),
                RequestOp::Put { key, value } => {
                    let rev = Revision::new(txn_main, sub);
                    sub = checked_add_sub(sub)?;
                    total = checked_add_total(total)?;
                    plan.push(OpPlan::Put {
                        key: key.clone(),
                        value: value.clone(),
                        rev,
                    });
                }
                RequestOp::DeleteRange { key, end } => {
                    let tombs =
                        self.plan_delete_range(key, end, current, txn_main, &mut sub, &mut total)?;
                    plan.push(OpPlan::Delete { tombs });
                }
            }
        }
        Ok((plan, total))
    }

    /// Compute the matched-key/sub list for a single
    /// [`RequestOp::DeleteRange`] under the writer lock. Filters
    /// already-tombstoned keys (review item S3); allocates one
    /// sub per surviving match and bumps the running sub /
    /// total counters.
    fn plan_delete_range(
        &self,
        key: &[u8],
        end: &[u8],
        current: i64,
        txn_main: i64,
        sub: &mut i64,
        total: &mut usize,
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
                Ok(_) => {
                    let rev = Revision::new(txn_main, *sub);
                    *sub = checked_add_sub(*sub)?;
                    *total = checked_add_total(*total)?;
                    tombs.push((k, rev));
                }
                Err(KeyIndexError::History(KeyHistoryError::RevisionNotFound)) => {}
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
                    for (_, rev) in tombs {
                        let encoded = encode_key(*rev, KeyKind::Tombstone);
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
                    for (k, rev) in tombs {
                        if let Err(_e) = self.index.tombstone(k, *rev) {
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
    /// - [`MvccError::FutureRevision`] if `rev > current_main`.
    /// - [`MvccError::Backend`] from snapshot acquisition,
    ///   `begin_batch`, `commit_batch`, or range iteration.
    /// - [`MvccError::KeyDecode`] if an on-disk encoded key
    ///   fails to decode (indicates backend corruption).
    pub async fn compact(&self, rev: i64) -> Result<(), MvccError> {
        let _state = self.writer.lock().await;
        let current = self.current_main.load(Ordering::Acquire);
        let floor = self.compacted.load(Ordering::Acquire);

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

        // Step 9: advance the floor. Done BEFORE the in-mem
        // compaction so a concurrent `Range` reader observing
        // the new floor will reject `rev < floor` reads — the
        // on-disk state is already physically advanced to match.
        self.compacted.store(rev, Ordering::Release);

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
}
