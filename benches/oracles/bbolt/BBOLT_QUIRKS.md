# bbolt vs redb: accepted semantic deltas

This document enumerates the semantic differences between bbolt
and mango's `RedbBackend` that the differential harness does NOT
treat as divergence. Anything not in this list is either:

- An equivalence the harness asserts (§ "Hard contracts" in the
  plan — empty keys, empty values, `[low, high)` range convention,
  post-reopen persistence), or
- A real divergence that trips an ADR 0002 §5 Tier-1 trigger.

This file is seeded with the list agreed in
`.planning/differential-backend-vs-bbolt.plan.md` §5 and will be
refined as the harness runs and surface area grows.

---

## Accepted

### On-disk size

Both engines use 4 KiB pages with copy-on-write allocators, but
neither allocation strategy maps onto the other byte-for-byte.
Post-compaction / post-reopen file sizes can differ by up to ~2×
without indicating a correctness issue. The harness reports both
sizes in failure artifacts for debugging but does **not** assert
equality. Storage-efficiency claims belong in ROADMAP:829
(bench), not here.

### Bucket auto-creation

bbolt creates buckets on first write inside a `Tx.Update`;
mango's `Backend` requires an explicit `register_bucket` call. The
harness eliminates this at the fixture level by pre-registering
the three harness buckets (`b1`, `b2`, `b3`) on both engines
before any op runs. If a future op synthesizes bucket names
outside that set, the rule is to pre-register the full universe,
not to depend on lazy creation.

### `defragment` / `compact` file identity

bbolt's `bbolt.Compact` writes a **new file** and the caller
replaces the original atomically; redb's `defragment` operates
**in-place**. The harness measures pre-state and post-state (full
snapshot diff) but never compares file paths or inodes. As long
as committed state is preserved across the op, the engines are
equivalent for our purposes.

### Iterator stability under concurrent mutation

Not tested. The harness snapshots are taken **after** commit
boundaries, and proptest sequences never interleave a reader with
a concurrent writer in the same case. Concurrent-reader /
concurrent-writer semantics are explicitly out of scope per ADR
0002 §6 (single-writer model). If a future workload requires
concurrent-mutation iterators, this is the item to revisit.

---

## Not accepted (hard contracts — failure = bug)

These are listed here so the boundary is legible, but the harness
**will fail** if any of them diverges:

- Empty keys: both engines accept, or both reject with the same
  error class. Wrapper lifts bbolt's `ErrKeyRequired` to
  `BackendError::Other` to make the errors symmetric.
- Empty values: same contract. Wrapper lifts
  `ErrValueNil` / `ErrValueTooLarge` if they surface.
- Mid-batch error asymmetry: op N returning `ok: true` on one
  engine and `ok: false` on the other is a test failure, no
  exceptions.
- Range convention: both engines return the same key set for
  `[low, high)` given the same bucket contents.
- Post-reopen: any key committed before `CloseReopen` reads back
  with the same value after.

---

## Discovered quirks log

Additions to the **Accepted** list after a divergence investigation
should land here with:

1. A short description of the delta.
2. A link to the failure artifact in `target/differential-failures/`
   (commit SHA of the artifact).
3. Justification: why this is a genuine engine-specific quirk
   rather than a wrapper bug or an engine-swap trigger.
4. A reviewer sign-off: two maintainers minimum per ADR 0002 §5.

Template:

```
### <short title>

- Discovered: YYYY-MM-DD, case-hash XXXXXXXX
- Category: [page layout | concurrency | freelist | on-disk format]
- Description: ...
- Not-a-bug justification: ...
- Reviewers: ..., ...
```

No entries yet.
