//! MVCC model proptest (ROADMAP:851).
//!
//! Compares [`mango_mvcc::MvccStore`] against an **independent**
//! hand-written reference (`mod model`) that mirrors etcd's MVCC
//! semantics. Random sequences of `Put` / `DeleteRange` / `RangeAt` /
//! `Compact` ops are applied to both; observable outputs must match
//! at every step.
//!
//! # What this test catches that the in-source unit tests cannot
//!
//! - `(rev, sub)` allocation across multi-tombstone `DeleteRange`.
//! - Tombstone-then-compact-then-`RangeAt` history walks across
//!   *multiple* generations (the etcd hash-stability quirk —
//!   `key_history.rs::keep` drops the trailing tombstone of a
//!   non-final generation, `compact` retains it).
//! - The `keys_only` × `count_only` × `limit` flag-cross's
//!   interaction with `more` and `count`.
//! - The `Compacted` / `FutureRevision` / `InvalidRange` error
//!   precedence.
//!
//! # Mirror discipline
//!
//! Every model rule cites the line in `MvccStore` (or
//! `KeyHistory`) it mirrors. On any divergence: walk the citation
//! chain. The model is the test specification — if it disagrees with
//! etcd, fix the model.
//!
//! # Acceptance
//!
//! - **Default** (`cargo nextest`): 1024 cases × 0..=32 ops per case.
//! - **Thorough** (`MANGO_MVCC_MODEL_THOROUGH=1`): `10_000` cases ×
//!   0..=64 ops per case. Roadmap's "10k+" tier; run on-demand.
//!
//! Op-vector size is deliberately bounded: `InMemBackend::snapshot`
//! deep-clones every bucket on every call (`mango-storage/src/inmem/
//! mod.rs:91-98`), so range cost grows O(N) in keyspace. 32 ops keeps
//! the default run under 60s on developer hardware.
//!
//! # Miri
//!
//! `miri_smoke` is a `#[cfg(miri)]` block that runs the smoke
//! sequences without the proptest harness (Miri can't enumerate 1024
//! cases in any reasonable time). Single-thread `current_thread`
//! runtime + sequential ops + no `spawn_blocking` makes this Miri-
//! viable, unlike L847.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    missing_docs,
    unreachable_pub,
    reason = "test code: panics are the assertion mechanism, arithmetic is bounded by op counters; \
             `unreachable_pub` is silenced because `mod model` lives inline but uses `pub` for clarity"
)]

use std::collections::BTreeMap;
use std::env;
use std::fmt;

use bytes::Bytes;
use mango_mvcc::store::MvccStore;
use mango_mvcc::{KeyValue, MvccError, RangeRequest, RangeResult, Revision};
use mango_storage::{Backend, BackendConfig, InMemBackend};
use proptest::collection::vec as prop_vec;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

/// Field-by-field projection of a `KeyValue`, comparable across
/// the model's `ModelKv` and the store's `KeyValue`. `KeyValue` is
/// `#[non_exhaustive]` so the model can't construct one directly;
/// we lift both sides to this tuple and compare.
type KvProj = (Bytes, Revision, Revision, i64, Bytes);

fn project_kv(kv: &KeyValue) -> KvProj {
    (
        kv.key.clone(),
        kv.create_revision,
        kv.mod_revision,
        kv.version,
        kv.value.clone(),
    )
}

/// Field-by-field projection of a `RangeResult`. Same rationale:
/// `RangeResult` is `#[non_exhaustive]`, so the model returns
/// `ModelRangeResult` and we project the store side here.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RangeProj {
    kvs: Vec<KvProj>,
    more: bool,
    count: u64,
    header_revision: i64,
}

fn project_range(r: &RangeResult) -> RangeProj {
    RangeProj {
        kvs: r.kvs.iter().map(project_kv).collect(),
        more: r.more,
        count: r.count,
        header_revision: r.header_revision,
    }
}

// ============================================================
// Model
// ============================================================
//
// An independent reference that mirrors `MvccStore` byte-for-byte
// on the four ops under test. Citations to the store/index lines
// it mirrors are inline.

mod model {
    use super::*;

    /// One revision's effect on a key — Put or Tombstone.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum RevKind {
        Put { value: Bytes },
        Tombstone,
    }

    /// A revision in a generation. Mirrors `Generation::revs[i]`
    /// at `key_history.rs:74-77`.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct RevEntry {
        pub rev: Revision,
        pub kind: RevKind,
    }

    /// One generation: `(Put*, Tombstone?)`. Closed (last entry is
    /// `Tombstone`) for all but the final generation. Mirrors
    /// `Generation` at `key_history.rs:63-78`.
    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    pub struct Generation {
        /// The first put rev of this generation.
        pub created: Revision,
        /// Total puts ever applied to this generation (excludes the
        /// closing tombstone). Mirrors `Generation::ver` semantics
        /// at `key_history.rs:65-68`.
        pub ver: i64,
        /// Append-only revs in this generation, ascending.
        pub revs: Vec<RevEntry>,
    }

    impl Generation {
        pub fn is_empty(&self) -> bool {
            self.revs.is_empty()
        }
    }

    /// Per-key history. Mirrors `KeyHistory` at
    /// `key_history.rs:132-146`. Independent implementation: shares
    /// no code with the lib's `KeyHistory` so a bug in either
    /// surfaces as a divergence.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct KeyHistory {
        /// Stack of generations, ascending in time.
        pub generations: Vec<Generation>,
    }

    impl Default for KeyHistory {
        fn default() -> Self {
            Self {
                generations: vec![Generation::default()],
            }
        }
    }

    impl KeyHistory {
        /// Append a put to the trailing generation. Mirrors
        /// `KeyHistory::put` at `key_history.rs:197-228`.
        pub fn put(&mut self, rev: Revision, value: Bytes) {
            let g = self.generations.last_mut().expect("non-empty");
            if g.revs.is_empty() {
                g.created = rev;
            }
            g.ver += 1;
            g.revs.push(RevEntry {
                rev,
                kind: RevKind::Put { value },
            });
        }

        /// Append a tombstone, closing the current generation and
        /// opening a fresh empty one. Mirrors `KeyHistory::tombstone`
        /// at `key_history.rs:239-248`. Caller MUST verify the
        /// current generation is non-empty.
        pub fn tombstone(&mut self, rev: Revision) {
            let g = self.generations.last_mut().expect("non-empty");
            assert!(
                !g.revs.is_empty(),
                "tombstone-on-empty (writer-side invariant)"
            );
            g.ver += 1;
            g.revs.push(RevEntry {
                rev,
                kind: RevKind::Tombstone,
            });
            self.generations.push(Generation::default());
        }

        /// `true` iff there are no live versions (single empty
        /// trailing generation). Mirrors `KeyHistory::is_empty` at
        /// `key_history.rs:178-180`.
        pub fn is_empty(&self) -> bool {
            self.generations.len() == 1
                && self.generations.first().is_some_and(Generation::is_empty)
        }

        /// Find the version visible at `at_rev`. Returns
        /// `(modified, created, version)` per `KeyAtRev` at
        /// `key_history.rs:636-650`. Returns `None` if the key was
        /// tombstoned at-or-before `at_rev` (mirrors `RevisionNotFound`
        /// from `KeyHistory::get`).
        pub fn get_at(&self, at_rev: i64) -> Option<KeyAtRev> {
            let g = self.find_generation(at_rev)?;
            // Walk descending; find first rev with main <= at_rev.
            // Mirrors `Generation::walk_desc` at
            // `key_history.rs:91-111`.
            let n = g.revs.iter().rposition(|e| e.rev.main() <= at_rev)?;
            let entry = &g.revs[n];
            // A tombstone is never "visible" — `KeyHistory::get`
            // would walk into the post-tombstone gap and return
            // `RevisionNotFound`. We mirror by returning None.
            if matches!(entry.kind, RevKind::Tombstone) {
                return None;
            }
            // Version: g.ver - (revs.len() - n - 1). Mirrors
            // `generation_version_at` at `key_history.rs:614-628`.
            let trailing = (g.revs.len() as i64) - (n as i64) - 1;
            let version = g.ver - trailing;
            Some(KeyAtRev {
                modified: entry.rev,
                created: g.created,
                version,
            })
        }

        /// Mirrors `KeyHistory::find_generation` at
        /// `key_history.rs:297-328`.
        fn find_generation(&self, at_rev: i64) -> Option<&Generation> {
            let last_idx = self.generations.len().checked_sub(1)?;
            let mut cg = last_idx;
            loop {
                let g = self.generations.get(cg)?;
                if g.is_empty() {
                    cg = cg.checked_sub(1)?;
                    continue;
                }
                // Non-final generation with tombstone main <= at_rev
                // → we're in a post-tombstone gap.
                if cg != last_idx {
                    if let Some(last) = g.revs.last() {
                        if last.rev.main() <= at_rev {
                            return None;
                        }
                    }
                }
                if let Some(first) = g.revs.first() {
                    if first.rev.main() <= at_rev {
                        return Some(g);
                    }
                }
                cg = cg.checked_sub(1)?;
            }
        }

        /// Compute (`gen_idx`, `rev_index`) split point for compact
        /// or keep. Mirrors `KeyHistory::do_compact_readonly` at
        /// `key_history.rs:495-532`.
        fn do_compact_readonly(&self, at_rev: i64) -> (usize, Option<usize>) {
            let last_gen_idx = self.generations.len().saturating_sub(1);
            let mut gen_idx = 0_usize;
            while gen_idx < last_gen_idx {
                let Some(g) = self.generations.get(gen_idx) else {
                    break;
                };
                if let Some(last) = g.revs.last() {
                    if last.rev.main() >= at_rev {
                        break;
                    }
                }
                gen_idx = gen_idx.saturating_add(1);
            }
            let Some(g) = self.generations.get(gen_idx) else {
                return (gen_idx, None);
            };
            // Walk descending; first entry with main <= at_rev is
            // the kept rev_index (in ascending coords).
            let rev_index = g.revs.iter().rposition(|e| e.rev.main() <= at_rev);
            (gen_idx, rev_index)
        }

        /// Apply `compact(at_rev)` — etcd-parity for the **in-mem
        /// index** mutation. Retains the trailing tombstone of a
        /// non-final generation. Returns `true` iff the history is
        /// now empty (caller drops the entry). Mirrors
        /// `KeyHistory::compact` at `key_history.rs:419-447`.
        ///
        /// Asymmetric with `keep` on the trailing-tombstone-of-non-
        /// final-generation rule (the etcd hash-stability quirk
        /// at `key_history.rs:413-418`).
        pub fn compact(&mut self, at_rev: i64) -> bool {
            if self.is_empty() {
                return true;
            }
            let (gen_idx, rev_index) = self.do_compact_readonly(at_rev);
            if let Some(g) = self.generations.get_mut(gen_idx) {
                if !g.is_empty() {
                    if let Some(idx) = rev_index {
                        if idx > 0 {
                            g.revs.drain(0..idx);
                        }
                    }
                }
            }
            if gen_idx > 0 {
                self.generations.drain(0..gen_idx);
            }
            self.is_empty()
        }
    }

    /// Mirrors `KeyAtRev` at `key_history.rs:636-650`.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    pub struct KeyAtRev {
        pub modified: Revision,
        pub created: Revision,
        pub version: i64,
    }

    /// The model. Mirrors the externally-observable state of
    /// `MvccStore<InMemBackend>`.
    ///
    /// Two parallel maps mirror the store's split between the
    /// per-shard `index` (`KeyHistory` storage, only mutated by
    /// `KeyHistory::compact`) and the lock-free `keys_in_order`
    /// (live-key `BTreeSet`, reaped on tombstone-at-HEAD-after-
    /// compact). Conflating them caused a model bug: after a
    /// put-delete-compact-put cycle, the historical reads for the
    /// pre-compact generation were lost from the model but visible
    /// in the store. See `store/mod.rs:1191-1206` for the
    /// `reap_keys_in_order_after_compact` rule that drove this
    /// design — it reaps from `keys_in_order` only, leaving the
    /// shard's `KeyHistory` intact for historical `RangeAt`.
    #[derive(Clone, Debug, Default)]
    pub struct Model {
        /// Per-key `KeyHistory` storage. Mirrors the per-shard
        /// `index` (`store/mod.rs:140` — `ShardedKeyIndex`).
        /// Mutated only by `KeyHistory::compact` and
        /// `KeyHistory::put` / `KeyHistory::tombstone`. Never
        /// drained based on `live_keys` reaps.
        pub histories: BTreeMap<Vec<u8>, KeyHistory>,
        /// Live key set in `BTreeMap` order. Mirrors
        /// `keys_in_order` (`store/mod.rs:142`). Iterated by
        /// `range` and `delete_range`. Reaped on compact when the
        /// key has no visible generation at HEAD.
        pub live_keys: std::collections::BTreeSet<Vec<u8>>,
        /// Compaction floor. Mirrors `WriterState::compacted` —
        /// initial 0 per `Snapshot::empty` at `snapshot.rs:93-98`.
        pub compacted: i64,
        /// Next main rev to allocate. Initial 1 (the first put gets
        /// (1, 0)). Mirrors `WriterState::new`.
        pub next_main: i64,
    }

    impl Model {
        pub fn new() -> Self {
            Self {
                histories: BTreeMap::new(),
                live_keys: std::collections::BTreeSet::new(),
                compacted: 0,
                next_main: 1,
            }
        }

        /// Highest fully-published revision. `next_main - 1` since
        /// `next_main` is the *next* allocation and the head is the
        /// last allocated. Mirrors `current_revision()` at
        /// `store/mod.rs:257-262` (which reads `snap.rev`, set to
        /// `rev.main()` after each `Put` / `DeleteRange`).
        pub fn current_main(&self) -> i64 {
            self.next_main - 1
        }

        /// Apply a put. Mirrors `MvccStore::put` at
        /// `store/mod.rs:324-387`. Returns the allocated revision.
        pub fn put(&mut self, key: &[u8], value: Bytes) -> Revision {
            let rev = Revision::new(self.next_main, 0);
            let kh = self.histories.entry(key.to_vec()).or_default();
            kh.put(rev, value);
            self.live_keys.insert(key.to_vec());
            self.next_main += 1;
            rev
        }

        /// Apply a delete-range. Mirrors `MvccStore::delete_range`
        /// at `store/mod.rs:600-706`.
        ///
        /// - Empty `end` → single-key delete (line 611-617).
        /// - Iterates matched keys in `BTreeMap` order, filtering
        ///   already-tombstoned-at-`current` (line 624-644).
        /// - Empty match set: returns `(0, Revision::new(current, 0))`
        ///   without advancing `next_main` (line 647-649).
        /// - Otherwise: allocates one main; assigns sub `0..matched.len()`
        ///   in iteration order (line 651-668).
        pub fn delete_range(&mut self, key: &[u8], end: &[u8]) -> (u64, Revision) {
            let current = self.current_main();
            // Match candidates from `live_keys` (mirrors store's
            // `keys_in_order` walk at `store/mod.rs:610-622`).
            let candidates: Vec<Vec<u8>> = if end.is_empty() {
                if self.live_keys.contains(key) {
                    vec![key.to_vec()]
                } else {
                    Vec::new()
                }
            } else {
                self.live_keys
                    .range::<[u8], _>((
                        std::ops::Bound::Included(key),
                        std::ops::Bound::Excluded(end),
                    ))
                    .cloned()
                    .collect()
            };
            // Filter to keys live at `current` (per-key
            // tombstone-at-current check, store/mod.rs:627-643).
            let matched: Vec<Vec<u8>> = candidates
                .into_iter()
                .filter(|k| {
                    self.histories
                        .get(k)
                        .is_some_and(|kh| kh.get_at(current).is_some())
                })
                .collect();
            if matched.is_empty() {
                return (0, Revision::new(current, 0));
            }
            let main = self.next_main;
            for (sub, k) in matched.iter().enumerate() {
                let rev = Revision::new(main, sub as i64);
                let kh = self.histories.get_mut(k).expect("matched key in map");
                kh.tombstone(rev);
            }
            // store/mod.rs:675: `keys_in_order` is intentionally
            // NOT modified on delete. live_keys stays as-is.
            self.next_main += 1;
            let count = matched.len() as u64;
            (count, Revision::new(main, 0))
        }

        /// Apply a range read. Mirrors `MvccStore::range` at
        /// `store/mod.rs:433-569` line-by-line.
        ///
        /// Returns a [`super::RangeProj`] (not `RangeResult`) — the
        /// store's `RangeResult` and `KeyValue` are
        /// `#[non_exhaustive]` and can't be constructed from this
        /// integration crate. The harness projects the store's
        /// returned `RangeResult` to the same shape for the
        /// per-op equality check.
        pub fn range(&self, req: &RangeRequest) -> Result<super::RangeProj, MvccError> {
            let current = self.current_main();
            let rev = req.revision.unwrap_or(current);
            let floor = self.compacted;
            // Error precedence (mirrors store/mod.rs:458-472):
            // Compacted → FutureRevision → InvalidRange.
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
            if !req.end.is_empty() && req.end.as_slice() < req.key.as_slice() {
                return Err(MvccError::InvalidRange);
            }
            // Match candidates from `live_keys` in BTreeMap order.
            // Mirrors `keys_in_order` walk at `store/mod.rs:482-498`.
            let matched: Vec<&Vec<u8>> = if req.end.is_empty() {
                if self.live_keys.contains(&req.key) {
                    vec![self.live_keys.get(&req.key).expect("present")]
                } else {
                    Vec::new()
                }
            } else {
                self.live_keys
                    .range::<[u8], _>((
                        std::ops::Bound::Included(req.key.as_slice()),
                        std::ops::Bound::Excluded(req.end.as_slice()),
                    ))
                    .collect()
            };
            // Walk. Mirrors store/mod.rs:504-561.
            let mut kvs: Vec<super::KvProj> = Vec::new();
            let mut count: u64 = 0;
            let mut more = false;
            for k in &matched {
                let Some(at) = self.histories.get(*k).and_then(|kh| kh.get_at(rev)) else {
                    // Tombstoned-at-`rev` → silent skip
                    // (store/mod.rs:510).
                    continue;
                };
                count += 1;
                if req.count_only {
                    // store/mod.rs:532: continue BEFORE limit. So
                    // count_only ignores limit AND `more` is never
                    // set true.
                    continue;
                }
                let limit_hit = req.limit.is_some_and(|l| kvs.len() >= l);
                if limit_hit {
                    more = true;
                    continue;
                }
                // store/mod.rs:546-551: keys_only fills empty Bytes
                // (does NOT skip the kv).
                let value: Bytes = if req.keys_only {
                    Bytes::new()
                } else {
                    self.histories
                        .get(*k)
                        .and_then(|kh| kh.value_at_modified(at.modified))
                        .unwrap_or_default()
                };
                kvs.push((
                    Bytes::copy_from_slice(k),
                    at.created,
                    at.modified,
                    at.version,
                    value,
                ));
            }
            Ok(super::RangeProj {
                kvs,
                more,
                count,
                header_revision: current,
            })
        }

        /// Apply a compact. Mirrors `MvccStore::compact` at
        /// `store/mod.rs:1084-1127`.
        ///
        /// Two-phase to mirror the store's `keep` (used for the
        /// on-disk delete pass at `store/mod.rs:1101-1108`) AND
        /// `compact` (used for the in-mem index at
        /// `store/mod.rs:1123`):
        ///
        /// 1. Phase 1 — `keep`: build `available` via the keep rule
        ///    (drops trailing-tombstone of non-final generation).
        ///    Mirrors `key_history.rs:456-488`.
        /// 2. Phase 2 — `compact`: mutate per-key history with the
        ///    compact rule (retains trailing tombstone). Mirrors
        ///    `key_history.rs:419-447`.
        /// 3. Reap from `keys`: any key whose `get_at(current)`
        ///    returns None (tombstoned-at-current) is removed —
        ///    mirrors `reap_keys_in_order_after_compact` at
        ///    `store/mod.rs:1191-1206`.
        pub fn compact(&mut self, at_rev: i64) -> Result<(), MvccError> {
            let current = self.current_main();
            // store/mod.rs:1091-1093: `rev <= floor` is a no-op.
            if at_rev <= self.compacted {
                return Ok(());
            }
            // store/mod.rs:1094-1099: `rev > current` →
            // FutureRevision.
            if at_rev > current {
                return Err(MvccError::FutureRevision {
                    requested: at_rev,
                    current,
                });
            }
            // Phase 1 unused for the model — `available_keep` would
            // drive on-disk deletes; for the model the per-key
            // `compact()` mutation produces the same observable
            // result. The discipline here is that the compact pass
            // is *itself* asymmetric with `keep` only on the
            // trailing-tombstone-of-non-final-generation; that
            // tombstone, if retained, is then reaped by the
            // post-compact `keys_in_order` walk because
            // `get_at(current)` is None on a key whose only history
            // is a trailing tombstone. Net result: same observable
            // state.
            //
            // Phase 2 — mutate `histories` per `KeyHistory::compact`.
            // ShardedKeyIndex::compact also drops the entry entirely
            // when the per-key history becomes empty
            // (`store/mod.rs:1117-1123` calls `index.compact` which
            // trims via `KeyHistory::compact`; the index drops keys
            // with empty histories implicitly).
            self.histories.retain(|_, kh| {
                kh.compact(at_rev);
                !kh.is_empty()
            });
            // Reap from `live_keys`: any key whose `histories` entry
            // is gone (KeyNotFound arm) OR whose `get_at(current)`
            // is None (History(RevisionNotFound) arm). Mirrors
            // `reap_keys_in_order_after_compact` at
            // `store/mod.rs:1198-1204`.
            let histories = &self.histories;
            self.live_keys.retain(|k| {
                histories
                    .get(k)
                    .is_some_and(|kh| kh.get_at(current).is_some())
            });
            self.compacted = at_rev;
            Ok(())
        }
    }

    impl KeyHistory {
        /// Retrieve the value bytes at a specific `modified`
        /// revision. The model stores values inline so this is a
        /// direct lookup; the store fetches from the on-disk
        /// bucket at `store/mod.rs:549-551`.
        pub fn value_at_modified(&self, modified: Revision) -> Option<Bytes> {
            for g in &self.generations {
                for entry in &g.revs {
                    if entry.rev == modified {
                        if let RevKind::Put { value } = &entry.kind {
                            return Some(value.clone());
                        }
                    }
                }
            }
            None
        }
    }
}

// ============================================================
// Op surface (proptest-generated)
// ============================================================

#[derive(Clone, Debug)]
enum Op {
    Put { key: Vec<u8>, value: Bytes },
    DeleteRange { key: Vec<u8>, end: Vec<u8> },
    RangeAt(RangeRequest),
    Compact { rev: i64 },
}

/// Fixed alphabet of 8 single-byte keys so most ops collide and
/// produce meaningful tombstones / generation walks.
fn key_strat() -> impl Strategy<Value = Vec<u8>> {
    prop::sample::select(vec![
        b"a".to_vec(),
        b"b".to_vec(),
        b"c".to_vec(),
        b"d".to_vec(),
        b"e".to_vec(),
        b"f".to_vec(),
        b"g".to_vec(),
        b"h".to_vec(),
    ])
}

fn value_strat() -> impl Strategy<Value = Bytes> {
    prop_vec(any::<u8>(), 1..=8).prop_map(Bytes::from)
}

/// Sorted `(start, end)` so `start <= end`. Required because
/// `MvccStore::delete_range` does NOT check `InvalidRange` (it
/// feeds bounds straight to `BTreeMap::range`, which panics on
/// inverted bounds — see plan §B1).
fn sorted_range_pair() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    (key_strat(), key_strat()).prop_map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
}

/// `(key, end)` for `DeleteRange`. `end` empty 20% of the time
/// to exercise the single-key-delete branch (`store/mod.rs:611-617`).
fn delete_range_pair() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    prop_oneof![
        2 => key_strat().prop_map(|k| (k, Vec::new())),
        8 => sorted_range_pair(),
    ]
}

/// `(key, end)` for `RangeAt`. `end` empty 30% for single-key
/// reads.
fn range_pair() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    prop_oneof![
        3 => key_strat().prop_map(|k| (k, Vec::new())),
        7 => sorted_range_pair(),
    ]
}

/// Op generator. Compact is weighted low so a 32-op case sees
/// ~1.6 compactions on average — enough to advance the floor for
/// the `Compacted` error-path coverage in `RangeAt`.
fn op_strat() -> impl Strategy<Value = Op> {
    prop_oneof![
        45 => (key_strat(), value_strat()).prop_map(|(key, value)| Op::Put { key, value }),
        25 => delete_range_pair().prop_map(|(key, end)| Op::DeleteRange { key, end }),
        25 => (
            // revision: None 50%, Some(0..=current+2) 50% (current
            // resolved at apply-time; we can't generate against the
            // running model, so we pick a small absolute range and
            // let the harness clamp/check).
            prop_oneof![
                Just(None),
                (0_i64..=64_i64).prop_map(Some),
            ],
            range_pair(),
            prop_oneof![
                Just(None),
                (0_usize..=8_usize).prop_map(Some),
            ],
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(revision, (key, end), limit, keys_only, count_only)| {
                let mut req = RangeRequest::default();
                req.key = key;
                req.end = end;
                req.revision = revision;
                req.limit = limit;
                req.keys_only = keys_only;
                req.count_only = count_only;
                Op::RangeAt(req)
            }),
        5 => (0_i64..=64_i64).prop_map(|rev| Op::Compact { rev }),
    ]
}

// ============================================================
// Side-by-side failure diff (M6)
// ============================================================

struct PutDiff<'a> {
    op: &'a Op,
    model: Result<Revision, &'static str>,
    store: Result<Revision, String>,
}
impl fmt::Display for PutDiff<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "op {:?}\n--- model ---\n{:?}\n--- store ---\n{:?}",
            self.op, self.model, self.store
        )
    }
}

struct DeleteDiff<'a> {
    op: &'a Op,
    model: (u64, Revision),
    store: (u64, Revision),
}
impl fmt::Display for DeleteDiff<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "op {:?}\n--- model ---\n(deleted={}, rev={:?})\n--- store ---\n(deleted={}, rev={:?})",
            self.op, self.model.0, self.model.1, self.store.0, self.store.1
        )
    }
}

struct RangeDiff<'a> {
    op: &'a Op,
    model: Result<RangeProj, String>,
    store: Result<RangeProj, String>,
}
impl fmt::Display for RangeDiff<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "op {:?}\n--- model ---\n{:#?}\n--- store ---\n{:#?}",
            self.op, self.model, self.store
        )
    }
}

// ============================================================
// Per-op equivalence
// ============================================================

/// Render an `MvccError` to a stable variant tag for cross-impl
/// comparison. We compare *variants*, not full `Debug` strings —
/// the messages embed values that may legitimately match while
/// the variant differs (or vice versa).
fn err_tag(e: &MvccError) -> &'static str {
    match e {
        MvccError::Compacted { .. } => "Compacted",
        MvccError::FutureRevision { .. } => "FutureRevision",
        MvccError::InvalidRange => "InvalidRange",
        MvccError::Internal { .. } => "Internal",
        MvccError::Backend(_) => "Backend",
        MvccError::KeyHistory(_) => "KeyHistory",
        MvccError::KeyIndex(_) => "KeyIndex",
        MvccError::KeyDecode(_) => "KeyDecode",
        // `MvccError` is `#[non_exhaustive]`; future variants land
        // as `Other` for the harness to surface without breaking.
        _ => "Other",
    }
}

async fn apply_one(
    store: &MvccStore<InMemBackend>,
    model: &mut model::Model,
    op: &Op,
) -> Result<(), TestCaseError> {
    match op {
        Op::Put { key, value } => {
            let m = model.put(key, value.clone());
            let s = match store.put(key, value).await {
                Ok(rev) => rev,
                Err(e) => {
                    return Err(TestCaseError::fail(format!(
                        "store.put errored unexpectedly: {e:?}"
                    )));
                }
            };
            prop_assert_eq!(
                m,
                s,
                "{}",
                PutDiff {
                    op,
                    model: Ok(m),
                    store: Ok(s),
                }
            );
        }
        Op::DeleteRange { key, end } => {
            let m = model.delete_range(key, end);
            let s = match store.delete_range(key, end).await {
                Ok(t) => t,
                Err(e) => {
                    return Err(TestCaseError::fail(format!(
                        "store.delete_range errored unexpectedly: {e:?}"
                    )));
                }
            };
            prop_assert_eq!(
                m,
                s,
                "{}",
                DeleteDiff {
                    op,
                    model: m,
                    store: s,
                }
            );
        }
        Op::RangeAt(req) => {
            // Clamp `revision` to the model's actual head + 2 so
            // we exercise FutureRevision without burning too many
            // cases on requests that overshoot by hundreds.
            let mut req = req.clone();
            if let Some(r) = req.revision {
                let cap = model.current_main() + 2;
                if r > cap {
                    req.revision = Some(cap);
                }
            }
            let m = model.range(&req);
            let s = store.range(req.clone()).map(|r| project_range(&r));
            match (&m, &s) {
                (Ok(mr), Ok(sr)) => {
                    prop_assert_eq!(
                        mr,
                        sr,
                        "{}",
                        RangeDiff {
                            op: &Op::RangeAt(req.clone()),
                            model: Ok(mr.clone()),
                            store: Ok(sr.clone()),
                        }
                    );
                }
                (Err(me), Err(se)) => {
                    let mt = err_tag(me);
                    let st = err_tag(se);
                    prop_assert_eq!(
                        mt,
                        st,
                        "error variant divergence on op {:?}: model={} store={}",
                        Op::RangeAt(req.clone()),
                        mt,
                        st
                    );
                }
                (m_, s_) => {
                    return Err(TestCaseError::fail(format!(
                        "ok/err mismatch on op {:?}\n--- model ---\n{m_:?}\n--- store ---\n{s_:?}",
                        Op::RangeAt(req.clone()),
                    )));
                }
            }
        }
        Op::Compact { rev } => {
            // Clamp `rev` to model head so we don't burn cases on
            // hopeless FutureRevisions; small overshoot still
            // covered.
            let cap = model.current_main();
            let rev = (*rev).min(cap + 1);
            let m = model.compact(rev);
            let s = store.compact(rev).await;
            match (&m, &s) {
                (Ok(()), Ok(())) => {}
                (Err(me), Err(se)) => {
                    let mt = err_tag(me);
                    let st = err_tag(se);
                    prop_assert_eq!(
                        mt,
                        st,
                        "compact error variant divergence rev={}: model={} store={}",
                        rev,
                        mt,
                        st
                    );
                }
                (m_, s_) => {
                    return Err(TestCaseError::fail(format!(
                        "compact ok/err mismatch rev={rev}\n--- model ---\n{m_:?}\n--- store ---\n{s_:?}"
                    )));
                }
            }
        }
    }
    Ok(())
}

// ============================================================
// Per-case driver
// ============================================================

fn fresh_store() -> MvccStore<InMemBackend> {
    let backend = InMemBackend::open(BackendConfig::new("/unused".into(), false))
        .expect("InMemBackend opens");
    MvccStore::open(backend).expect("MvccStore opens against fresh backend")
}

async fn run_case(ops: Vec<Op>) -> Result<(), TestCaseError> {
    let store = fresh_store();
    let mut model = model::Model::new();
    for op in &ops {
        apply_one(&store, &mut model, op).await?;
    }
    // End-of-case ground-truth: a `RangeAt` over the full keyspace
    // at HEAD must agree on both sides. Catches subtle
    // post-compaction divergences that wouldn't surface on the
    // generated ops alone.
    let mut full = RangeRequest::default();
    full.key = b"a".to_vec();
    full.end = b"z".to_vec();
    let mr = model
        .range(&full)
        .map_err(|e| TestCaseError::fail(format!("model end-of-case range: {e:?}")))?;
    let sr = store
        .range(full)
        .map(|r| project_range(&r))
        .map_err(|e| TestCaseError::fail(format!("store end-of-case range: {e:?}")))?;
    prop_assert_eq!(
        &mr,
        &sr,
        "end-of-case range divergence\n--- model ---\n{:#?}\n--- store ---\n{:#?}",
        mr,
        sr
    );
    Ok(())
}

fn run_case_blocking(ops: Vec<Op>) -> Result<(), TestCaseError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("rt");
    rt.block_on(run_case(ops))
}

// ============================================================
// Smoke (M5 — strengthened to pin S1 / S2 risks)
// ============================================================

/// Pins S1: trailing-tombstone-of-non-final-generation across
/// a `Compact` that lands inside the first generation. The store's
/// `keep` rule (used for on-disk deletes) drops the trailing
/// tombstone; the `compact` rule (used in-mem) retains it. The
/// reap step then drops the key. Model must replicate.
#[test]
fn smoke_tombstone_compact_rangeat() {
    let ops = vec![
        Op::Put {
            key: b"a".to_vec(),
            value: Bytes::from_static(b"1"),
        },
        Op::DeleteRange {
            key: b"a".to_vec(),
            end: b"b".to_vec(),
        },
        Op::Put {
            key: b"a".to_vec(),
            value: Bytes::from_static(b"2"),
        },
        Op::DeleteRange {
            key: b"a".to_vec(),
            end: b"b".to_vec(),
        },
        Op::Compact { rev: 2 },
        Op::RangeAt({
            let mut r = RangeRequest::default();
            r.key = b"a".to_vec();
            r.end = b"b".to_vec();
            r.revision = Some(3);
            r
        }),
        Op::RangeAt({
            let mut r = RangeRequest::default();
            r.key = b"a".to_vec();
            r.end = b"b".to_vec();
            r
        }),
    ];
    run_case_blocking(ops).expect("S1 smoke passes");
}

/// Pins S2: `count_only = true` ignores `limit` and never sets
/// `more`; `keys_only = true` returns kvs with empty values but
/// respects `limit` (and sets `more`).
#[test]
fn smoke_count_only_and_keys_only_with_limit() {
    let ops = vec![
        Op::Put {
            key: b"a".to_vec(),
            value: Bytes::from_static(b"1"),
        },
        Op::Put {
            key: b"b".to_vec(),
            value: Bytes::from_static(b"2"),
        },
        Op::Put {
            key: b"c".to_vec(),
            value: Bytes::from_static(b"3"),
        },
        // count_only with limit=Some(1): expect count=3, more=false, kvs=[].
        Op::RangeAt({
            let mut r = RangeRequest::default();
            r.key = b"a".to_vec();
            r.end = b"z".to_vec();
            r.limit = Some(1);
            r.count_only = true;
            r
        }),
        // keys_only with limit=Some(1): expect count=3, more=true, kvs.len()=1, kvs[0].value empty.
        Op::RangeAt({
            let mut r = RangeRequest::default();
            r.key = b"a".to_vec();
            r.end = b"z".to_vec();
            r.limit = Some(1);
            r.keys_only = true;
            r
        }),
    ];
    run_case_blocking(ops).expect("S2 smoke passes");
}

/// Pins B1: `DeleteRange` is generated with sorted bounds; the
/// model and store agree on the `(0, (current,0))` empty-match
/// no-op return when the range covers no live keys.
#[test]
fn smoke_delete_range_empty_match_no_op() {
    let ops = vec![
        Op::Put {
            key: b"d".to_vec(),
            value: Bytes::from_static(b"x"),
        },
        // No keys in [a, b) → empty match.
        Op::DeleteRange {
            key: b"a".to_vec(),
            end: b"b".to_vec(),
        },
        // Then RangeAt to confirm head didn't advance: HEAD == 1 (one Put).
        Op::RangeAt({
            let mut r = RangeRequest::default();
            r.key = b"a".to_vec();
            r.end = b"z".to_vec();
            r
        }),
    ];
    run_case_blocking(ops).expect("B1 smoke passes");
}

// ============================================================
// Proptest harness
// ============================================================

fn case_count() -> u32 {
    if env::var("MANGO_MVCC_MODEL_THOROUGH").is_ok() {
        10_000
    } else {
        1024
    }
}

fn op_vec_max() -> usize {
    if env::var("MANGO_MVCC_MODEL_THOROUGH").is_ok() {
        64
    } else {
        32
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: case_count(),
        max_shrink_iters: 4096,
        // Project convention (see snapshot_consistency.rs:455
        // and btreemap_oracle.rs):
        // shrinker output to stdout is enough for repro.
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn model_proptest(ops in prop_vec(op_strat(), 0..=op_vec_max())) {
        run_case_blocking(ops)?;
    }
}

// ============================================================
// Miri smoke (M2)
// ============================================================
//
// Under Miri, the proptest harness above is far too slow to
// enumerate even one case. We instead run the three smoke
// sequences to exercise `MvccStore`'s sequential code paths
// under Miri's stricter aliasing checks. Single-thread
// `current_thread` runtime + sequential ops + no
// `spawn_blocking` keeps this Miri-viable.
#[cfg(miri)]
#[test]
fn miri_smoke_runs_all_three() {
    smoke_tombstone_compact_rangeat();
    smoke_count_only_and_keys_only_with_limit();
    smoke_delete_range_empty_match_no_op();
}
