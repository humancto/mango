//! Per-key revision tree (`KeyHistory`).
//!
//! Each user key in the MVCC store maps to one [`KeyHistory`] — an
//! append-only stack of `Generation`s, where each generation is a
//! contiguous run of revisions from one creation up to (and
//! including) one tombstone. Tombstoning a key closes the current
//! generation and opens a new empty trailing generation.
//!
//! Byte-for-byte semantic mirror of etcd's `keyIndex`
//! (`server/mvcc/key_index.go` at tag `v3.5.16`). The container —
//! the sharded `HashMap<Bytes, KeyHistory>` — is the next ROADMAP
//! item (L840) and is intentionally NOT in this module: every
//! operation here takes `&mut self` and the type is `Send + Sync`
//! by structural derivation only, leaving locking decisions to L840.
//!
//! # Lifecycle
//!
//! ```
//! use mango_mvcc::{KeyHistory, KeyHistoryError, Revision};
//!
//! let mut k = KeyHistory::new();
//! k.put(Revision::new(1, 0))?;
//! k.put(Revision::new(2, 0))?;
//! k.tombstone(Revision::new(3, 0))?;
//! k.put(Revision::new(4, 0))?;
//! k.tombstone(Revision::new(5, 0))?;
//!
//! // Three generations. Stack order: `generations[0]` is oldest;
//! // the trailing empty generation is `generations.last()`.
//! //   [0] {1,2,3(t)}
//! //   [1] {4,5(t)}
//! //   [2] {empty}
//! assert_eq!(k.generations_len(), 3);
//!
//! // get(4) returns the version visible at rev 4 — the put at (4,0).
//! let at4 = k.get(4)?;
//! assert_eq!(at4.modified, Revision::new(4, 0));
//! assert_eq!(at4.created, Revision::new(4, 0));
//! assert_eq!(at4.version, 1);
//! # Ok::<(), KeyHistoryError>(())
//! ```
//!
//! # Error posture
//!
//! Etcd's `keyIndex` panics on invariant violations (non-monotonic
//! put, tombstone-on-empty, etc.). Mango returns typed
//! [`KeyHistoryError`]s instead — both because the workspace's
//! `clippy::panic` lint forbids panicking on data shape and because
//! the sole legitimate caller (the Raft apply loop) is the only
//! agent positioned to escalate.

use core::fmt;
use std::collections::HashSet;
use std::hash::BuildHasher;

use crate::encoding::KeyKind;
use crate::Revision;

/// One generation: a contiguous run of revisions for a single key,
/// from creation through (optionally) a tombstone.
///
/// Module-private — `KeyHistory` is the only public surface. `Default`
/// builds an empty generation (no `created`, no `revs`, `ver = 0`).
#[derive(Clone, Default, Eq, PartialEq, Hash, Debug)]
struct Generation {
    /// Number of versions ever written to this generation. Always
    /// `>= revs.len()`; can exceed it after `restore` when only the
    /// latest rev is kept on disk.
    ver: i64,

    /// Revision at which this generation was opened (the first put
    /// of the generation). Meaningful only when `!revs.is_empty()`.
    created: Revision,

    /// Revisions in this generation, ascending. Last element is the
    /// tombstone if and only if this is not the final generation in
    /// `KeyHistory::generations`.
    revs: Vec<Revision>,
}

impl Generation {
    fn is_empty(&self) -> bool {
        self.revs.is_empty()
    }

    /// Walk the revs in descending order, calling `pred` on each.
    /// Returns the index where `pred` first returned `false` (in the
    /// ascending coordinate system), or `None` if `pred` returned
    /// `true` for every rev.
    ///
    /// Mirrors etcd `key_index.go::generation.walk` (`v3.5.16`).
    fn walk_desc<F>(&self, mut pred: F) -> Option<usize>
    where
        F: FnMut(Revision) -> bool,
    {
        for (offset, rev) in self.revs.iter().rev().enumerate() {
            if !pred(*rev) {
                // Convert from reverse-iteration offset to ascending
                // index. `offset < revs.len()` so the subtraction
                // never underflows.
                let len = self.revs.len();
                return Some(
                    len.checked_sub(offset)
                        .and_then(|v| v.checked_sub(1))
                        .unwrap_or_else(|| {
                            unreachable!("walk_desc: offset {offset} out of range for len {len}")
                        }),
                );
            }
        }
        None
    }

    /// `revs.last().main` if the generation is non-empty.
    fn last_main(&self) -> Option<i64> {
        self.revs.last().map(|r| r.main())
    }

    /// `revs.first().main` if the generation is non-empty.
    fn first_main(&self) -> Option<i64> {
        self.revs.first().map(|r| r.main())
    }
}

/// The per-key revision tree.
///
/// See the module-level rustdoc for lifecycle and semantics.
///
/// Construct with [`KeyHistory::new`] (or `Default`). All mutating
/// operations take `&mut self`. Equality is structural — `Eq` and
/// `Hash` are derived, so doc-comment-fixture tests can use
/// `assert_eq!` against a hand-built `KeyHistory`.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct KeyHistory {
    /// Last modified revision across all generations. Etcd's
    /// `keyIndex.modified` (`server/mvcc/key_index.go::keyIndex`,
    /// `v3.5.16`).
    modified: Revision,

    /// Generations stack, ascending in time. Always non-empty under
    /// the steady-state lifecycle invariant; the trailing generation
    /// is `{empty}` after a tombstone or non-empty after a put.
    /// **Exception**: a fresh `KeyHistory::restore(...)` carries a
    /// single non-empty generation with no trailing empty — see
    /// [`KeyHistory::restore`].
    generations: Vec<Generation>,
}

impl Default for KeyHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyHistory {
    /// A fresh `KeyHistory` with one empty generation, ready for the
    /// first put.
    #[must_use]
    pub fn new() -> Self {
        Self {
            modified: Revision::default(),
            generations: vec![Generation::default()],
        }
    }

    /// The most recently modified revision. `Revision::default()`
    /// (i.e. `(0, 0)`) on a fresh `KeyHistory`.
    #[must_use]
    pub const fn modified(&self) -> Revision {
        self.modified
    }

    /// `true` iff the key currently has no live versions.
    ///
    /// Specifically: there is exactly one generation and it is
    /// empty. A `KeyHistory` returned from `restore` always has a
    /// non-empty generation, so `is_empty()` returns `false`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.generations.len() == 1 && self.generations.first().is_some_and(Generation::is_empty)
    }

    /// Number of generations. Test/debug-only accessor; production
    /// callers MUST NOT depend on this for correctness.
    #[doc(hidden)]
    #[must_use]
    pub fn generations_len(&self) -> usize {
        self.generations.len()
    }

    /// Append `rev` to the current (trailing) generation.
    ///
    /// # Errors
    ///
    /// [`KeyHistoryError::NonMonotonic`] if `rev <= self.modified()`.
    /// Etcd panics here (`server/mvcc/key_index.go::keyIndex.put`,
    /// `v3.5.16`); Mango returns a typed error.
    pub fn put(&mut self, rev: Revision) -> Result<(), KeyHistoryError> {
        if rev <= self.modified {
            return Err(KeyHistoryError::NonMonotonic {
                given: rev,
                modified: self.modified,
            });
        }

        // Lifecycle invariant: generations is never empty under
        // steady state. A fresh `KeyHistory::new()` always has one
        // empty trailing generation; tombstone always pushes one
        // before returning.
        let last_idx = self.generations.len().checked_sub(1).unwrap_or_else(|| {
            unreachable!("KeyHistory invariant: generations is never empty");
        });
        let g = self.generations.get_mut(last_idx).unwrap_or_else(|| {
            unreachable!("KeyHistory invariant: last generation index in bounds");
        });

        if g.revs.is_empty() {
            // First put of this generation — record the creation rev.
            g.created = rev;
            // TODO(L859): increment keysGauge here
            // (etcd `key_index.go:100`).
        }
        g.revs.push(rev);
        g.ver = g.ver.checked_add(1).unwrap_or_else(|| {
            unreachable!("Generation::ver overflow at i64::MAX");
        });
        self.modified = rev;
        Ok(())
    }

    /// Append a tombstone revision and open a fresh empty trailing
    /// generation.
    ///
    /// # Errors
    ///
    /// - [`KeyHistoryError::NonMonotonic`] from the inner `put`.
    /// - [`KeyHistoryError::TombstoneOnEmpty`] if the current
    ///   generation has no revisions (i.e. the key is already
    ///   tombstoned, or never written).
    pub fn tombstone(&mut self, rev: Revision) -> Result<(), KeyHistoryError> {
        if self.current_generation_is_empty() {
            return Err(KeyHistoryError::TombstoneOnEmpty);
        }
        self.put(rev)?;
        self.generations.push(Generation::default());
        // TODO(L859): decrement keysGauge here
        // (etcd `key_index.go:137`).
        Ok(())
    }

    fn current_generation_is_empty(&self) -> bool {
        match self.generations.last() {
            Some(g) => g.is_empty(),
            None => unreachable!("KeyHistory invariant: generations is never empty"),
        }
    }

    /// Fetch the version of the key visible at `at_rev`.
    ///
    /// `at_rev` is interpreted as `Revision { main: at_rev, sub: 0 }`
    /// for the purposes of the search — matching etcd's `int64`-only
    /// `get` parameter (`server/mvcc/key_index.go::keyIndex.get`,
    /// `v3.5.16`).
    ///
    /// # Errors
    ///
    /// [`KeyHistoryError::RevisionNotFound`] if the key did not
    /// exist at `at_rev`, was tombstoned at or before `at_rev`
    /// without a subsequent put, or `at_rev` is before the first
    /// put.
    pub fn get(&self, at_rev: i64) -> Result<KeyAtRev, KeyHistoryError> {
        let g = self
            .find_generation(at_rev)
            .ok_or(KeyHistoryError::RevisionNotFound)?;

        // Walk descending; stop at the first rev with main <= at_rev.
        let n = g
            .walk_desc(|r| r.main() > at_rev)
            .ok_or(KeyHistoryError::RevisionNotFound)?;

        // n is the ascending index of the visible rev.
        let modified = *g.revs.get(n).unwrap_or_else(|| {
            unreachable!("walk_desc returned in-bounds index {n}");
        });
        let version = generation_version_at(g, n)?;

        Ok(KeyAtRev {
            modified,
            created: g.created,
            version,
        })
    }

    /// Returns the generation index containing a version visible at
    /// `at_rev`, or `None` if the key did not exist at `at_rev`.
    ///
    /// Mirrors etcd `key_index.go::keyIndex.findGeneration`.
    fn find_generation(&self, at_rev: i64) -> Option<&Generation> {
        let last_idx = self.generations.len().checked_sub(1)?;

        let mut cg = last_idx;
        loop {
            let g = self.generations.get(cg)?;

            if g.is_empty() {
                cg = cg.checked_sub(1)?;
                continue;
            }

            // Non-final generation with tombstone main <= at_rev:
            // we're inside a post-tombstone gap → not found.
            if cg != last_idx {
                if let Some(tomb_main) = g.last_main() {
                    if tomb_main <= at_rev {
                        return None;
                    }
                }
            }

            // First rev's main <= at_rev: this is our generation.
            if let Some(first_main) = g.first_main() {
                if first_main <= at_rev {
                    return Some(g);
                }
            }

            cg = cg.checked_sub(1)?;
        }
    }

    /// All revisions at or after `since_rev`, dedup-by-main keeping
    /// the largest `sub` per main. For the watch path.
    ///
    /// `since_rev` is treated as `Revision { main: since_rev, sub: 0 }`,
    /// matching etcd `key_index.go::keyIndex.since` (`v3.5.16`).
    ///
    /// Allocates a fresh `Vec`. Use [`KeyHistory::since_into`] when
    /// the caller already owns a buffer (the L859 watch hub).
    #[must_use]
    pub fn since(&self, since_rev: i64) -> Vec<Revision> {
        let mut out = Vec::new();
        self.since_into(since_rev, &mut out);
        out
    }

    /// Append matching revisions to a caller-owned `Vec`. Writes
    /// directly into `out` with no internal allocation, so the L859
    /// watch hub can reuse one `Vec` across many calls.
    ///
    /// Dedup-by-main keeps the largest sub per main (etcd
    /// `key_index.go::keyIndex.since` semantics): when the next rev's
    /// main equals `out.last().main`, the last slot is overwritten in
    /// place rather than pushed. Dedup operates only on the suffix
    /// this call appends; pre-existing entries in `out` are not
    /// touched.
    pub fn since_into(&self, since_rev: i64, out: &mut Vec<Revision>) {
        let since = Revision::new(since_rev, 0);

        // Find the starting generation: walk backwards from the last
        // until we find one whose `created < since`.
        let Some(mut gi) = self.generations.len().checked_sub(1) else {
            return;
        };

        while gi > 0 {
            let Some(g) = self.generations.get(gi) else {
                return;
            };
            if g.is_empty() {
                let Some(v) = gi.checked_sub(1) else { break };
                gi = v;
                continue;
            }
            if since > g.created {
                break;
            }
            let Some(v) = gi.checked_sub(1) else { break };
            gi = v;
        }

        // Walk forward from gi, applying dedup-by-main on the suffix
        // this call writes (anchored at `start`).
        let start = out.len();
        let mut last_main: Option<i64> = None;
        for g in self.generations.iter().skip(gi) {
            for r in &g.revs {
                if since > *r {
                    continue;
                }
                if last_main == Some(r.main()) {
                    // Replace the last-written slot — in our suffix.
                    if out.len() > start {
                        if let Some(slot) = out.last_mut() {
                            *slot = *r;
                        }
                    } else {
                        out.push(*r);
                    }
                    continue;
                }
                out.push(*r);
                last_main = Some(r.main());
            }
        }
    }

    /// Compact: remove revisions with `main <= at_rev` except the
    /// largest such in each generation, dropping any generation that
    /// becomes empty.
    ///
    /// Returns `true` if the `KeyHistory` is now empty and the
    /// container SHOULD drop this entry; `false` otherwise.
    ///
    /// `available` is populated with the surviving compacted-floor
    /// revision. **Differs from [`KeyHistory::keep`]** on the
    /// trailing tombstone of a non-final generation: `compact`
    /// retains it in `available` (etcd hash-stability requirement);
    /// `keep` removes it. See etcd
    /// `key_index.go:240-249` (`v3.5.16`).
    pub fn compact<S: BuildHasher>(
        &mut self,
        at_rev: i64,
        available: &mut HashSet<Revision, S>,
    ) -> bool {
        if self.is_empty() {
            return true;
        }

        let (gen_idx, rev_index) = self.do_compact_readonly(at_rev, available);

        // Truncate the chosen generation's revs to start at rev_index.
        if let Some(g) = self.generations.get_mut(gen_idx) {
            if !g.is_empty() {
                if let Some(idx) = rev_index {
                    if idx > 0 {
                        g.revs.drain(0..idx);
                    }
                }
            }
        }

        // Drop earlier generations.
        if gen_idx > 0 {
            self.generations.drain(0..gen_idx);
        }

        self.is_empty()
    }

    /// Plan-only variant of [`KeyHistory::compact`]: populates
    /// `available` with what compaction would retain, but does not
    /// mutate `self`.
    ///
    /// Differs from `compact` on the trailing tombstone of a
    /// non-final generation: `keep` removes it from `available`
    /// (`server/mvcc/key_index.go::keyIndex.keep`, `v3.5.16`).
    pub fn keep<S: BuildHasher>(&self, at_rev: i64, available: &mut HashSet<Revision, S>) {
        if self.is_empty() {
            return;
        }

        // Non-mutating analog of do_compact. Build a temp set, then
        // copy with the tombstone-removal quirk.
        let (gen_idx, rev_index) = self.do_compact_readonly(at_rev, available);

        let Some(g) = self.generations.get(gen_idx) else {
            return;
        };
        if g.is_empty() {
            return;
        }

        // If the kept rev is the last in this generation AND this is
        // not the final generation, it's a trailing tombstone — drop
        // it from `available`.
        let Some(last_idx_in_gen) = g.revs.len().checked_sub(1) else {
            return;
        };
        let Some(last_gen_idx) = self.generations.len().checked_sub(1) else {
            return;
        };
        if let Some(idx) = rev_index {
            if idx == last_idx_in_gen && gen_idx != last_gen_idx {
                if let Some(rev) = g.revs.get(idx) {
                    available.remove(rev);
                }
            }
        }
    }

    /// Read-only inner of `compact` and `keep`: walks generations,
    /// populates `available`, returns the (`gen_idx`, `rev_index`)
    /// split point. Mutation of `self.revs` happens in `compact`
    /// after this returns; `keep` uses the return value to decide
    /// the trailing-tombstone removal.
    fn do_compact_readonly<S: BuildHasher>(
        &self,
        at_rev: i64,
        available: &mut HashSet<Revision, S>,
    ) -> (usize, Option<usize>) {
        let last_gen_idx = self.generations.len().saturating_sub(1);
        let mut gen_idx = 0_usize;

        // Find the first generation whose tombstone main >= at_rev,
        // or the final generation, whichever comes first.
        while gen_idx < last_gen_idx {
            let Some(g) = self.generations.get(gen_idx) else {
                break;
            };
            match g.last_main() {
                Some(tomb) if tomb >= at_rev => break,
                _ => {}
            }
            gen_idx = gen_idx.saturating_add(1);
        }

        let Some(g) = self.generations.get(gen_idx) else {
            return (gen_idx, None);
        };

        // Walk descending; for each rev with main <= at_rev, add to
        // available. Stop at the first such rev.
        let rev_index = g.walk_desc(|r| {
            if r.main() <= at_rev {
                available.insert(r);
                false
            } else {
                true
            }
        });

        (gen_idx, rev_index)
    }

    /// All `(rev, kind)` pairs in this `KeyHistory` whose `main`
    /// component lies in the inclusive range `[lo, hi]`, yielded in
    /// ascending order by stored `Revision`.
    ///
    /// The last revision of any non-trailing generation is a
    /// `Tombstone`; every other stored revision is a `Put`. The
    /// trailing empty generation contributes nothing. The walk is
    /// bounded by per-key history depth (typically `< 100` under
    /// realistic workloads), and the caller holds the per-shard
    /// read-lock for the duration of the walk — so per-key history
    /// depth bounds the lock-hold time.
    ///
    /// The L863 catch-up scan is the sole intended caller; the
    /// per-key history walk feeds `WatchEvent` synthesis.
    pub fn events_in_range(
        &self,
        lo: i64,
        hi: i64,
    ) -> impl Iterator<Item = (Revision, KeyEventKind)> + '_ {
        let last_gen_idx = self.generations.len().saturating_sub(1);
        self.generations
            .iter()
            .enumerate()
            .flat_map(move |(gi, g)| {
                let is_final = gi == last_gen_idx;
                let revs_len = g.revs.len();
                g.revs.iter().copied().enumerate().map(move |(ri, r)| {
                    // The last rev of a non-final gen is the
                    // tombstone that closed it; every other rev
                    // (including the only rev of the final gen) is
                    // a Put. `revs_len.checked_sub(1)` is `None`
                    // only on an empty gen, which the outer iter
                    // skips structurally (no revs to walk).
                    let is_last_in_gen = revs_len.checked_sub(1).is_some_and(|last| ri == last);
                    let kind = if !is_final && is_last_in_gen {
                        KeyEventKind::Tombstone
                    } else {
                        KeyEventKind::Put
                    };
                    (r, kind)
                })
            })
            .filter(move |(r, _)| r.main() >= lo && r.main() <= hi)
    }

    /// The highest stored `KeyAtRev` whose stored revision is
    /// **strictly less than** `rev` (lex order on `(main, sub)`).
    ///
    /// Cross-generation semantics:
    ///
    /// - If the strict predecessor is in the same generation as the
    ///   anchor for `rev`, return that generation's `KeyAtRev` for
    ///   the predecessor.
    /// - If `rev` sits at the start of a generation (no in-gen
    ///   predecessor) — i.e. the strict predecessor lies in an
    ///   earlier generation — walk back to that earlier generation.
    ///   The trailing tombstone of a non-final generation is **not**
    ///   a live version (see [`KeyHistory::keep`]); skip it and
    ///   return the `KeyAtRev` for the LAST LIVE rev of that
    ///   earlier generation.
    /// - If no prior live rev exists in any generation, return `None`.
    ///
    /// Load-bearing for L863's `compute_prev_kv_strict`: without the
    /// cross-gen walk, a `Put` that opens a new generation (i.e. the
    /// first put after a tombstone) would always get `prev = None`,
    /// which is correct only if there is no earlier generation.
    ///
    /// # Errors
    ///
    /// [`KeyHistoryError::RevisionNotFound`] if the per-generation
    /// `version` arithmetic underflows. Structurally impossible
    /// under the lifecycle invariants but propagated for parity with
    /// [`KeyHistory::get`].
    pub fn get_strict_lt(&self, rev: Revision) -> Result<Option<KeyAtRev>, KeyHistoryError> {
        let Some(last_gen_idx) = self.generations.len().checked_sub(1) else {
            return Ok(None);
        };
        let mut gi = last_gen_idx;
        loop {
            let Some(g) = self.generations.get(gi) else {
                return Ok(None);
            };

            if !g.is_empty() {
                let is_final = gi == last_gen_idx;
                // Largest in-gen rev strictly less than `rev`. The
                // `walk_desc` predicate "rev >= target" returns
                // `false` exactly at the first (descending) rev
                // that is strictly less than `target` — its
                // ascending index is what we want.
                if let Some(cand_idx) = g.walk_desc(|r| r >= rev) {
                    let target_idx = if !is_final && g.revs.len().checked_sub(1) == Some(cand_idx) {
                        // Trailing tombstone of a non-final gen.
                        // Lifecycle invariant: a non-final gen has
                        // at least one Put (the put that opened it)
                        // before the tombstone, so cand_idx >= 1.
                        cand_idx.checked_sub(1)
                    } else {
                        Some(cand_idx)
                    };
                    if let Some(idx) = target_idx {
                        let Some(modified) = g.revs.get(idx).copied() else {
                            return Ok(None);
                        };
                        let version = generation_version_at(g, idx)?;
                        return Ok(Some(KeyAtRev {
                            modified,
                            created: g.created,
                            version,
                        }));
                    }
                    // Single-rev non-final gen: lifecycle
                    // violation in steady state. Defensive
                    // fall-through to the previous gen.
                }
            }

            gi = match gi.checked_sub(1) {
                Some(v) => v,
                None => return Ok(None),
            };
        }
    }

    /// Reconstruct a `KeyHistory` from on-disk state — a single
    /// non-empty generation. Used by the recovery path (L852)
    /// while replaying the `key` bucket.
    ///
    /// **Invariant exception**: the returned `KeyHistory` has a
    /// single non-empty generation with no trailing empty. This
    /// differs from `new()` and from any `KeyHistory` reached via
    /// `tombstone`. A subsequent `tombstone` works correctly because
    /// `tombstone` itself appends the trailing-empty generation.
    ///
    /// # Errors
    ///
    /// [`KeyHistoryError::RestoreInvalid`] if any of:
    /// - `created.main < 0` ([`RestoreInvalidReason::NegativeMain`])
    /// - `created.sub < 0` ([`RestoreInvalidReason::NegativeSub`])
    /// - `modified < created` lex-order
    ///   ([`RestoreInvalidReason::ModifiedBeforeCreated`])
    /// - `ver < 1` ([`RestoreInvalidReason::VersionLessThanOne`])
    pub fn restore(
        created: Revision,
        modified: Revision,
        ver: i64,
    ) -> Result<Self, KeyHistoryError> {
        if created.main() < 0 || modified.main() < 0 {
            return Err(KeyHistoryError::RestoreInvalid {
                reason: RestoreInvalidReason::NegativeMain,
            });
        }
        if created.sub() < 0 || modified.sub() < 0 {
            return Err(KeyHistoryError::RestoreInvalid {
                reason: RestoreInvalidReason::NegativeSub,
            });
        }
        if modified < created {
            return Err(KeyHistoryError::RestoreInvalid {
                reason: RestoreInvalidReason::ModifiedBeforeCreated,
            });
        }
        if ver < 1 {
            return Err(KeyHistoryError::RestoreInvalid {
                reason: RestoreInvalidReason::VersionLessThanOne,
            });
        }

        // TODO(L859): increment keysGauge here
        // (etcd `key_index.go:119`).
        let g = Generation {
            ver,
            created,
            revs: vec![modified],
        };
        Ok(Self {
            modified,
            generations: vec![g],
        })
    }
}

impl fmt::Debug for KeyHistory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Mirrors etcd `keyIndex.String` shape, readable on test failure.
        writeln!(f, "KeyHistory {{ modified: {}, generations:", self.modified)?;
        for (i, g) in self.generations.iter().enumerate() {
            write!(f, "  [{i}] created: {}, ver: {}, revs: [", g.created, g.ver)?;
            for (j, r) in g.revs.iter().enumerate() {
                if j > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{r}")?;
            }
            writeln!(f, "]")?;
        }
        write!(f, "}}")
    }
}

/// Compute the version number for the rev at index `n` (ascending)
/// in `g`. Mirrors etcd's `g.ver - int64(len(g.revs)-n-1)` formula
/// (`server/mvcc/key_index.go::keyIndex.get`, `v3.5.16`), which is
/// load-bearing for the post-`restore` case where `g.ver > revs.len()`.
fn generation_version_at(g: &Generation, n: usize) -> Result<i64, KeyHistoryError> {
    let len_revs = i64::try_from(g.revs.len()).unwrap_or_else(|_| {
        unreachable!("Generation::revs.len() exceeds i64::MAX (impossible on 64-bit platforms)");
    });
    let n_i64 = i64::try_from(n).unwrap_or_else(|_| {
        unreachable!("walk_desc returned out-of-i64 index — invariant violation");
    });
    let trailing = len_revs
        .checked_sub(n_i64)
        .and_then(|v| v.checked_sub(1))
        .ok_or(KeyHistoryError::RevisionNotFound)?;
    g.ver
        .checked_sub(trailing)
        .ok_or(KeyHistoryError::RevisionNotFound)
}

/// The `(modified, created, version)` tuple visible at a given rev.
///
/// Mirrors etcd's `keyIndex.get` return shape
/// (`server/mvcc/key_index.go`, `v3.5.16`).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
#[non_exhaustive]
pub struct KeyAtRev {
    /// Revision at which the visible version was modified.
    pub modified: Revision,

    /// Revision at which the generation containing the visible
    /// version was created.
    pub created: Revision,

    /// Version number (1-indexed) within the generation. `i64`
    /// matches etcd's wire-format `int64`.
    pub version: i64,
}

/// Kind of event yielded by [`KeyHistory::events_in_range`] —
/// distinguishes a live revision (`Put`) from a tombstone
/// (`Tombstone`) on a per-revision basis. Mirrors the on-disk
/// distinction expressed by [`KeyKind`].
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
#[non_exhaustive]
pub enum KeyEventKind {
    /// A live revision — the catch-up scan emits a `Put` watch event.
    Put,
    /// A tombstone — the catch-up scan emits a `Delete` watch event.
    Tombstone,
}

impl From<KeyKind> for KeyEventKind {
    fn from(kind: KeyKind) -> Self {
        match kind {
            KeyKind::Put => Self::Put,
            KeyKind::Tombstone => Self::Tombstone,
        }
    }
}

/// Reasons a `restore` call may reject its inputs.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
#[non_exhaustive]
pub enum RestoreInvalidReason {
    /// `created.main` or `modified.main` is negative.
    NegativeMain,
    /// `created.sub` or `modified.sub` is negative.
    NegativeSub,
    /// `modified < created` in lex order on `(main, sub)`.
    ModifiedBeforeCreated,
    /// `ver < 1`.
    VersionLessThanOne,
}

impl fmt::Display for RestoreInvalidReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NegativeMain => f.write_str("negative main revision"),
            Self::NegativeSub => f.write_str("negative sub revision"),
            Self::ModifiedBeforeCreated => f.write_str("modified rev precedes created rev"),
            Self::VersionLessThanOne => f.write_str("version < 1"),
        }
    }
}

/// Errors returned by [`KeyHistory`] operations.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KeyHistoryError {
    /// A `put` was attempted with a revision `<= modified`. Etcd
    /// panics in the equivalent path; Mango returns this error.
    #[error("non-monotonic put: given {given} <= modified {modified}")]
    NonMonotonic {
        /// The rejected input revision.
        given: Revision,
        /// The existing high watermark.
        modified: Revision,
    },

    /// `tombstone` called on a `KeyHistory` whose current generation
    /// is empty (i.e. already tombstoned, or never written).
    #[error("tombstone on empty generation")]
    TombstoneOnEmpty,

    /// `get` did not find a version visible at the requested rev.
    #[error("revision not found")]
    RevisionNotFound,

    /// `restore` rejected its inputs.
    #[error("restore invalid: {reason}")]
    RestoreInvalid {
        /// The specific input invariant that failed.
        reason: RestoreInvalidReason,
    },
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use super::*;
    use proptest::prelude::*;

    /// Build the exact `KeyHistory` produced by the etcd doc-comment
    /// lifecycle: `put(1.0); put(2.0); tombstone(3.0); put(4.0); tombstone(5.0)`.
    fn doc_fixture() -> KeyHistory {
        let mut k = KeyHistory::new();
        k.put(Revision::new(1, 0)).expect("put 1");
        k.put(Revision::new(2, 0)).expect("put 2");
        k.tombstone(Revision::new(3, 0)).expect("tomb 3");
        k.put(Revision::new(4, 0)).expect("put 4");
        k.tombstone(Revision::new(5, 0)).expect("tomb 5");
        k
    }

    #[test]
    fn new_is_empty_with_one_generation() {
        let k = KeyHistory::new();
        assert!(k.is_empty());
        assert_eq!(k.modified(), Revision::default());
        assert_eq!(k.generations_len(), 1);
        assert_eq!(KeyHistory::default(), k);
    }

    #[test]
    fn etcd_doc_lifecycle() {
        let k = doc_fixture();
        // Three generations: {1,2,3(t)}, {4,5(t)}, {empty}.
        assert_eq!(k.generations_len(), 3);
        assert_eq!(k.modified(), Revision::new(5, 0));
        assert!(!k.is_empty());

        assert_eq!(k.generations[0].revs.len(), 3);
        assert_eq!(k.generations[1].revs.len(), 2);
        assert_eq!(k.generations[2].revs.len(), 0);
    }

    #[test]
    fn put_rejects_non_monotonic() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(2, 0)).unwrap();
        let err = k.put(Revision::new(1, 0)).unwrap_err();
        assert!(matches!(
            err,
            KeyHistoryError::NonMonotonic {
                given,
                modified
            } if given == Revision::new(1, 0) && modified == Revision::new(2, 0)
        ));
    }

    #[test]
    fn put_rejects_equal_main_lower_sub() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(1, 5)).unwrap();
        let err = k.put(Revision::new(1, 3)).unwrap_err();
        assert!(matches!(err, KeyHistoryError::NonMonotonic { .. }));
    }

    #[test]
    fn put_accepts_equal_main_higher_sub() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(1, 0)).unwrap();
        k.put(Revision::new(1, 1)).unwrap();
        assert_eq!(k.modified(), Revision::new(1, 1));
    }

    #[test]
    fn tombstone_on_empty_returns_err() {
        let mut k = KeyHistory::new();
        let err = k.tombstone(Revision::new(1, 0)).unwrap_err();
        assert!(matches!(err, KeyHistoryError::TombstoneOnEmpty));
    }

    #[test]
    fn tombstone_on_post_tombstone_empty_returns_err() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(1, 0)).unwrap();
        k.tombstone(Revision::new(2, 0)).unwrap();
        let err = k.tombstone(Revision::new(3, 0)).unwrap_err();
        assert!(matches!(err, KeyHistoryError::TombstoneOnEmpty));
    }

    #[test]
    fn get_at_rev_before_creation_returns_err() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(5, 0)).unwrap();
        let err = k.get(3).unwrap_err();
        assert!(matches!(err, KeyHistoryError::RevisionNotFound));
    }

    #[test]
    fn get_at_tombstone_rev_itself_returns_err() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(1, 0)).unwrap();
        k.tombstone(Revision::new(2, 0)).unwrap();
        // get(2) lands inside the post-tombstone gap of the (now)
        // non-final generation — etcd returns nil here.
        let err = k.get(2).unwrap_err();
        assert!(matches!(err, KeyHistoryError::RevisionNotFound));
    }

    #[test]
    fn get_after_tombstone_before_next_put_returns_err() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(1, 0)).unwrap();
        k.tombstone(Revision::new(2, 0)).unwrap();
        let err = k.get(3).unwrap_err();
        assert!(matches!(err, KeyHistoryError::RevisionNotFound));
    }

    #[test]
    fn get_returns_correct_version_count() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(1, 0)).unwrap();
        k.put(Revision::new(2, 0)).unwrap();
        k.put(Revision::new(3, 0)).unwrap();
        let at = k.get(3).unwrap();
        assert_eq!(at.modified, Revision::new(3, 0));
        assert_eq!(at.created, Revision::new(1, 0));
        assert_eq!(at.version, 3);
    }

    #[test]
    fn get_returns_visible_version_inside_generation() {
        let k = doc_fixture();
        // get(2) sees the put at (2,0): version 2 of the first generation.
        let at = k.get(2).unwrap();
        assert_eq!(at.modified, Revision::new(2, 0));
        assert_eq!(at.created, Revision::new(1, 0));
        assert_eq!(at.version, 2);
    }

    #[test]
    fn get_at_first_gen_first_put_succeeds() {
        let k = doc_fixture();
        let at = k.get(1).unwrap();
        assert_eq!(at.modified, Revision::new(1, 0));
        assert_eq!(at.created, Revision::new(1, 0));
        assert_eq!(at.version, 1);
    }

    #[test]
    fn get_in_second_generation_succeeds() {
        let k = doc_fixture();
        let at = k.get(4).unwrap();
        assert_eq!(at.modified, Revision::new(4, 0));
        assert_eq!(at.created, Revision::new(4, 0));
        assert_eq!(at.version, 1);
    }

    #[test]
    fn since_dedups_by_main() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(5, 0)).unwrap();
        k.put(Revision::new(5, 1)).unwrap();
        let revs = k.since(0);
        assert_eq!(revs, vec![Revision::new(5, 1)]);
    }

    #[test]
    fn since_walks_across_generations() {
        let k = doc_fixture();
        let revs = k.since(2);
        assert_eq!(
            revs,
            vec![
                Revision::new(2, 0),
                Revision::new(3, 0),
                Revision::new(4, 0),
                Revision::new(5, 0),
            ]
        );
    }

    #[test]
    fn since_treats_argument_as_main_with_zero_sub() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(5, 3)).unwrap();
        // since=5 means since=Revision(5,0); (5,3) > (5,0) so it's included.
        let revs = k.since(5);
        assert_eq!(revs, vec![Revision::new(5, 3)]);
    }

    #[test]
    fn since_into_writes_directly_into_caller_buffer() {
        // since_into must not allocate when the caller-owned buffer
        // already has sufficient capacity. Watch hub (L859) reuses
        // one Vec across calls and depends on this.
        let k = doc_fixture();
        let mut buf: Vec<Revision> = Vec::with_capacity(8);
        let cap_before = buf.capacity();
        k.since_into(2, &mut buf);
        assert_eq!(buf.len(), 4);
        assert_eq!(
            buf.capacity(),
            cap_before,
            "since_into must not reallocate when capacity is sufficient"
        );
    }

    #[test]
    fn since_into_appends_to_existing_buffer_without_clobbering() {
        let k = doc_fixture();
        let sentinel = Revision::new(999, 999);
        let mut buf: Vec<Revision> = vec![sentinel];
        k.since_into(2, &mut buf);
        assert_eq!(buf.len(), 5);
        assert_eq!(buf[0], sentinel, "pre-existing entry must be preserved");
        assert_eq!(buf[1], Revision::new(2, 0));
        assert_eq!(buf[4], Revision::new(5, 0));
    }

    #[test]
    fn etcd_doc_compact_2() {
        let mut k = doc_fixture();
        let mut available: HashSet<Revision> = HashSet::new();
        let dropped = k.compact(2, &mut available);
        assert!(!dropped);
        // Generations: {2,3(t)}, {4,5(t)}, {empty}
        assert_eq!(k.generations_len(), 3);
        assert_eq!(
            k.generations[0].revs,
            vec![Revision::new(2, 0), Revision::new(3, 0)]
        );
        assert_eq!(
            k.generations[1].revs,
            vec![Revision::new(4, 0), Revision::new(5, 0)]
        );
        assert!(k.generations[2].revs.is_empty());
    }

    #[test]
    fn etcd_doc_compact_4() {
        let mut k = doc_fixture();
        let mut available: HashSet<Revision> = HashSet::new();
        let dropped = k.compact(4, &mut available);
        assert!(!dropped);
        // Generations: {4,5(t)}, {empty}
        assert_eq!(k.generations_len(), 2);
        assert_eq!(
            k.generations[0].revs,
            vec![Revision::new(4, 0), Revision::new(5, 0)]
        );
        assert!(k.generations[1].revs.is_empty());
    }

    #[test]
    fn etcd_doc_compact_5() {
        let mut k = doc_fixture();
        let mut available: HashSet<Revision> = HashSet::new();
        let dropped = k.compact(5, &mut available);
        assert!(!dropped);
        // Generations: {5(t)}, {empty}
        assert_eq!(k.generations_len(), 2);
        assert_eq!(k.generations[0].revs, vec![Revision::new(5, 0)]);
        assert!(k.generations[1].revs.is_empty());
    }

    #[test]
    fn etcd_doc_compact_6() {
        let mut k = doc_fixture();
        let mut available: HashSet<Revision> = HashSet::new();
        let dropped = k.compact(6, &mut available);
        assert!(dropped);
        assert!(k.is_empty());
    }

    #[test]
    fn keep_doc_compact_2() {
        let k = doc_fixture();
        let mut available: HashSet<Revision> = HashSet::new();
        k.keep(2, &mut available);
        // KeyHistory unchanged
        assert_eq!(k, doc_fixture());
        // available retains (2,0) — the largest <= 2 in gen 0.
        assert!(available.contains(&Revision::new(2, 0)));
    }

    #[test]
    fn keep_does_not_mutate() {
        let k = doc_fixture();
        let snapshot = k.clone();
        let mut available: HashSet<Revision> = HashSet::new();
        k.keep(3, &mut available);
        assert_eq!(k, snapshot);
    }

    #[test]
    fn compact_vs_keep_diverge_on_trailing_tombstone() {
        // at_rev=3 lands on the trailing tombstone of gen 0 (a
        // non-final generation). etcd compact retains it in
        // `available` for hash stability; etcd keep removes it.
        let mut k_compact = doc_fixture();
        let k_keep = doc_fixture();

        let mut a_compact: HashSet<Revision> = HashSet::new();
        let mut a_keep: HashSet<Revision> = HashSet::new();

        let _ = k_compact.compact(3, &mut a_compact);
        k_keep.keep(3, &mut a_keep);

        // The trailing tombstone (3,0) of gen 0 (non-final) is in
        // a_compact (etcd compact retains it for hash stability),
        // not in a_keep (etcd keep removes it).
        assert!(a_compact.contains(&Revision::new(3, 0)));
        assert!(!a_keep.contains(&Revision::new(3, 0)));
    }

    #[test]
    fn restore_constructs_single_generation() {
        let k = KeyHistory::restore(Revision::new(1, 0), Revision::new(7, 2), 3).unwrap();
        let at = k.get(7).unwrap();
        assert_eq!(at.modified, Revision::new(7, 2));
        assert_eq!(at.created, Revision::new(1, 0));
        assert_eq!(at.version, 3);
    }

    #[test]
    fn restore_get_at_rev_after_modified_returns_modified_version() {
        // Post-restore exception: `g.ver` exceeds `revs.len()`. The
        // `g.ver - (len-n-1)` formula must still hold for queries
        // beyond `modified.main`.
        let k = KeyHistory::restore(Revision::new(1, 0), Revision::new(7, 2), 3).unwrap();
        let at = k.get(20).unwrap();
        assert_eq!(at.modified, Revision::new(7, 2));
        assert_eq!(at.created, Revision::new(1, 0));
        assert_eq!(at.version, 3);
    }

    #[test]
    fn restore_after_then_tombstone() {
        let mut k = KeyHistory::restore(Revision::new(1, 0), Revision::new(7, 2), 3).unwrap();
        k.tombstone(Revision::new(10, 0)).unwrap();
        // After tombstone: gen 0 has [modified=(7,2), tomb=(10,0)],
        // and a fresh empty trailing generation.
        assert_eq!(k.generations_len(), 2);
        assert!(!k.is_empty());
    }

    #[test]
    fn restore_rejects_negative_main() {
        let err = KeyHistory::restore(Revision::new(-1, 0), Revision::new(0, 0), 1).unwrap_err();
        assert!(matches!(
            err,
            KeyHistoryError::RestoreInvalid {
                reason: RestoreInvalidReason::NegativeMain
            }
        ));
    }

    #[test]
    fn restore_rejects_negative_sub() {
        let err = KeyHistory::restore(Revision::new(0, -1), Revision::new(0, 0), 1).unwrap_err();
        assert!(matches!(
            err,
            KeyHistoryError::RestoreInvalid {
                reason: RestoreInvalidReason::NegativeSub
            }
        ));
    }

    #[test]
    fn restore_rejects_modified_before_created() {
        let err = KeyHistory::restore(Revision::new(5, 0), Revision::new(3, 0), 1).unwrap_err();
        assert!(matches!(
            err,
            KeyHistoryError::RestoreInvalid {
                reason: RestoreInvalidReason::ModifiedBeforeCreated
            }
        ));
    }

    #[test]
    fn restore_rejects_zero_version() {
        let err = KeyHistory::restore(Revision::new(1, 0), Revision::new(1, 0), 0).unwrap_err();
        assert!(matches!(
            err,
            KeyHistoryError::RestoreInvalid {
                reason: RestoreInvalidReason::VersionLessThanOne
            }
        ));
    }

    #[test]
    fn is_empty_after_full_compact() {
        let mut k = doc_fixture();
        let mut available: HashSet<Revision> = HashSet::new();
        let dropped = k.compact(6, &mut available);
        assert!(dropped);
        assert!(k.is_empty());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// `modified()` always equals the last-inserted revision.
        #[test]
        fn proptest_modified_equals_last_inserted(
            ops in proptest::collection::vec(0_i64..1000, 1..50),
        ) {
            let mut k = KeyHistory::new();
            let mut last = Revision::default();
            let mut main = 0_i64;
            for delta in ops {
                main = main.saturating_add(delta).saturating_add(1);
                let rev = Revision::new(main, 0);
                k.put(rev).unwrap();
                last = rev;
            }
            prop_assert_eq!(k.modified(), last);
        }

        /// `compact(x); compact(x)` is the same as one `compact(x)`.
        #[test]
        fn proptest_compact_is_idempotent(
            puts in proptest::collection::vec(0_i64..50, 1..30),
            at in 0_i64..100,
        ) {
            let mut k = KeyHistory::new();
            let mut main = 0_i64;
            for delta in puts {
                main = main.saturating_add(delta).saturating_add(1);
                k.put(Revision::new(main, 0)).unwrap();
            }
            let mut k2 = k.clone();
            let mut a1: HashSet<Revision> = HashSet::new();
            let mut a2: HashSet<Revision> = HashSet::new();
            let _ = k.compact(at, &mut a1);
            let _ = k2.compact(at, &mut a2);
            let _ = k2.compact(at, &mut a2);
            prop_assert_eq!(k, k2);
        }

        /// A single put at non-negative `(m, s)` is gettable at `m`.
        #[test]
        fn proptest_get_after_put_succeeds(
            main in 1_i64..i64::MAX,
            sub in 0_i64..i64::MAX,
        ) {
            let mut k = KeyHistory::new();
            k.put(Revision::new(main, sub)).unwrap();
            let at = k.get(main).unwrap();
            prop_assert_eq!(at.modified, Revision::new(main, sub));
        }

        /// `modified()` is monotonically non-decreasing across any
        /// valid op sequence.
        #[test]
        fn proptest_modified_non_decreasing(
            ops in proptest::collection::vec(0_i64..100, 1..30),
        ) {
            let mut k = KeyHistory::new();
            let mut prev = Revision::default();
            let mut main = 0_i64;
            for delta in ops {
                main = main.saturating_add(delta).saturating_add(1);
                k.put(Revision::new(main, 0)).unwrap();
                prop_assert!(k.modified() >= prev);
                prev = k.modified();
            }
        }

        /// `keep` does not mutate `self`.
        #[test]
        fn proptest_keep_is_pure(
            puts in proptest::collection::vec(0_i64..50, 1..20),
            at in 0_i64..200,
        ) {
            let mut k = KeyHistory::new();
            let mut main = 0_i64;
            for delta in puts {
                main = main.saturating_add(delta).saturating_add(1);
                k.put(Revision::new(main, 0)).unwrap();
            }
            let snapshot = k.clone();
            let mut available: HashSet<Revision> = HashSet::new();
            k.keep(at, &mut available);
            prop_assert_eq!(k, snapshot);
        }
    }

    // === L863 catch-up primitives ===

    /// U1 — `events_in_range` walks across generations in ascending
    /// order, marking the trailing rev of every non-final generation
    /// as `Tombstone` and every other rev as `Put`.
    #[test]
    fn key_history_events_in_range_walks_generations() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(1, 0)).unwrap();
        k.tombstone(Revision::new(2, 0)).unwrap();
        k.put(Revision::new(3, 0)).unwrap();
        k.tombstone(Revision::new(4, 0)).unwrap();
        k.put(Revision::new(5, 0)).unwrap();

        let all: Vec<(Revision, KeyEventKind)> = k.events_in_range(0, 100).collect();
        assert_eq!(
            all,
            vec![
                (Revision::new(1, 0), KeyEventKind::Put),
                (Revision::new(2, 0), KeyEventKind::Tombstone),
                (Revision::new(3, 0), KeyEventKind::Put),
                (Revision::new(4, 0), KeyEventKind::Tombstone),
                (Revision::new(5, 0), KeyEventKind::Put),
            ]
        );

        let subset: Vec<(Revision, KeyEventKind)> = k.events_in_range(2, 4).collect();
        assert_eq!(
            subset,
            vec![
                (Revision::new(2, 0), KeyEventKind::Tombstone),
                (Revision::new(3, 0), KeyEventKind::Put),
                (Revision::new(4, 0), KeyEventKind::Tombstone),
            ]
        );
    }

    /// U2 — `events_in_range` filters strictly on `main`, treating
    /// `lo` and `hi` as inclusive boundaries.
    #[test]
    fn key_history_events_in_range_filters_main_revision() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(1, 0)).unwrap();
        k.put(Revision::new(2, 0)).unwrap();
        k.put(Revision::new(3, 0)).unwrap();
        k.put(Revision::new(4, 0)).unwrap();

        // Empty range on the low side.
        let none: Vec<_> = k.events_in_range(10, 20).collect();
        assert!(none.is_empty());

        // Single-rev window pinned to lo == hi.
        let one: Vec<_> = k.events_in_range(2, 2).collect();
        assert_eq!(one, vec![(Revision::new(2, 0), KeyEventKind::Put)]);

        // Hi inclusive — boundary check.
        let mid: Vec<_> = k.events_in_range(2, 3).collect();
        assert_eq!(
            mid,
            vec![
                (Revision::new(2, 0), KeyEventKind::Put),
                (Revision::new(3, 0), KeyEventKind::Put),
            ]
        );
    }

    /// U3 — `events_in_range` on `Put(1)/Tomb(2)` (with the trailing
    /// empty generation invariant) yields `[(1,Put),(2,Tomb)]`. The
    /// trailing empty generation contributes nothing.
    #[test]
    fn key_history_events_in_range_empty_trailing_generation() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(1, 0)).unwrap();
        k.tombstone(Revision::new(2, 0)).unwrap();
        assert_eq!(k.generations_len(), 2);

        let events: Vec<_> = k.events_in_range(0, 100).collect();
        assert_eq!(
            events,
            vec![
                (Revision::new(1, 0), KeyEventKind::Put),
                (Revision::new(2, 0), KeyEventKind::Tombstone),
            ]
        );
    }

    /// U5a — `get_strict_lt` returns the same-generation predecessor
    /// when one exists.
    #[test]
    fn get_strict_lt_same_generation_predecessor() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(7, 0)).unwrap();
        k.put(Revision::new(7, 1)).unwrap();

        let pred = k.get_strict_lt(Revision::new(7, 1)).unwrap().unwrap();
        assert_eq!(pred.modified, Revision::new(7, 0));
        assert_eq!(pred.created, Revision::new(7, 0));
        assert_eq!(pred.version, 1);

        let none = k.get_strict_lt(Revision::new(7, 0)).unwrap();
        assert!(none.is_none(), "no predecessor for the very first put");
    }

    /// U5b — `get_strict_lt` walks back to the previous generation
    /// when there is no in-gen predecessor, skipping the trailing
    /// tombstone (which is not a live version).
    #[test]
    fn get_strict_lt_cross_generation_predecessor_skips_tombstone() {
        let mut k = KeyHistory::new();
        k.put(Revision::new(1, 0)).unwrap();
        k.tombstone(Revision::new(2, 0)).unwrap();
        k.put(Revision::new(3, 0)).unwrap();

        // (3,0) is the first rev of gen 1. Strict predecessor is in
        // gen 0; the tombstone (2,0) is the last rev of gen 0 but is
        // NOT a live version, so we walk past it to (1,0).
        let pred = k.get_strict_lt(Revision::new(3, 0)).unwrap().unwrap();
        assert_eq!(
            pred.modified,
            Revision::new(1, 0),
            "must skip tombstone (2,0) and return the live (1,0)"
        );
        assert_eq!(pred.created, Revision::new(1, 0));
        assert_eq!(pred.version, 1);

        // (1,0) is the very first stored rev; no predecessor.
        let none = k.get_strict_lt(Revision::new(1, 0)).unwrap();
        assert!(none.is_none());
    }

    /// U6 — `Revision`'s `Ord` is lex on `(main, sub)`. The L863
    /// proof relies on this ordering for `get_strict_lt`. A future
    /// refactor of `Revision` (e.g. swapping `Ord` for a custom
    /// impl) must preserve it; this test fails fast if it doesn't.
    #[test]
    fn revision_ord_is_main_then_sub() {
        let a = Revision::new(5, 0);
        let b = Revision::new(5, 1);
        let c = Revision::new(6, 0);
        assert!(a < b);
        assert!(b < c);
        assert!(a < c);
        assert_eq!(a, Revision::new(5, 0));
    }
}
