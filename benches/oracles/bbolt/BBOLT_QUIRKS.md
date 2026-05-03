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

### Error-wire normalization

bbolt and redb surface semantically equivalent errors with different
strings — bbolt returns `"key required"` / `"value cannot be nil"`,
redb returns `"empty key"` / `"empty value"`. The harness strips
`"backend: "` and `"app: <Method>: "` prefixes (added by the wrapper
layers on each side) and applies a 2-entry alias table before
comparing error text. This is **not** a quirk in either engine — it
is a wire-format translation in the differential boundary, kept in
the harness rather than the production wrapper so the production
error types stay engine-native.

### CommitGroup atomicity

Both engines wrap a `CommitGroup` in **one atomic transaction with
one terminal fsync**: bbolt via `db.Update(func(tx)...)`, redb via
`RedbBackend::commit_group` (single `WriteTransaction` with
`durability = Immediate`). The harness diffs post-state at group
boundaries and does **not** observe per-batch transaction structure.
Earlier drafts of this doc incorrectly stated that redb ran a fsync
per batch; that was corrected in PR #53.

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

### Failure-artifact GC policy

`target/differential-failures/<utc-secs>-<hash8>/` directories are
**not** auto-cleaned by the harness — accumulating dirs is the
intended behaviour locally so a developer triaging a flake has the
full forensic trail. CI artifact retention is **7 days** for the PR
`differential` job and **30 days** for the nightly thorough sweep
(longer to cover weekend triage). Local developers should
periodically `rm -rf target/differential-failures/` if disk usage
becomes an issue. Cross-referenced from
`tests/differential_vs_bbolt/seeds/README.md`.

### Seed-file retirement policy

A file in `crates/mango-storage/tests/differential_vs_bbolt/seeds/`
may be removed only when **both** of the following hold:

1. The underlying bug fix has landed on `main` and the commit is
   referenced in the seed's accompanying note in
   `seeds/README.md`.
2. A proptest strategy (in `differential_vs_bbolt.rs` or a sibling
   harness) exercises the same op-shape such that a regression
   would be caught without the pinned seed.

Removal is a PR reviewed like any code change. This prevents
`seeds/` from becoming an indefinite dumping ground while
preserving the coverage gate the pinned case provides. New seeds
are added under the divergence triage workflow in
`seeds/README.md`.

---

## Now fixed (formerly divergent)

### Empty-end `DeleteRange`

Previously, redb's `apply_staged` treated a `DeleteRange` with
`end.is_empty()` as a degenerate empty range (because the underlying
`retain_in(start..end, ..)` call gets an inverted bound). bbolt
interprets `len(end) == 0` as **unbounded** — delete from `start`
onward. The original differential strategy (PR #53) drew keys of
length 1..=16 and never sampled `end == b""`, so the asymmetry was
invisible until the strategy was widened. Fix landed in commit
`db5c76d` (`fix(storage): DeleteRange empty-end means unbounded
upper`): `apply_staged` now branches on `end.is_empty()` and uses an
unbounded upper range, and `validate_ops` skips the `start > end`
check in that case. The fix lives in
`crates/mango-storage/src/redb/mod.rs`, **not** in the harness — the
`Backend::DeleteRange` contract is public, so band-aiding it inside
the differential test would mask the bug from every other caller.

Listed here as a closed item rather than under Accepted because the
wrapper now matches bbolt's documented contract — no ongoing
divergence to tolerate.

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

---

## Bench-mode wire-format quirks

These are not bbolt-engine quirks per se but cross-language
serialization quirks specific to the `--mode=bench` driver. They
land here because the same Go binary owns both modes and a future
maintainer reading `bench.go` will look here first.

### `hdrhistogram-go` non-zero `normalizingIndexOffset`

- Discovered: 2026-05-03 while landing ROADMAP:829 commit 9.
- Category: cross-language wire-format
- Description: `hdrhistogram-go` v1.x hardcodes
  `getNormalizingIndexOffset() = 1`
  (see `hdr.go:169` in v1.2.0). The Rust `hdrhistogram` 7.5.x
  deserializer rejects any non-zero normalizing offset with
  `DeserializeError::UnsupportedFeature`
  (`deserializer.rs:146-148`). The two libraries' V2-compressed
  payloads are therefore not directly interoperable out of the
  box.
- Workaround: `bench.go::encodeHistB64` byte-patches the inflated
  inner V2 payload at offset 8..12 to zero before re-deflating.
  Safe because no shifted recording happens in the bench harness;
  the offset is unused metadata.
- Verified by: `bbolt_runner.rs::tests::load_then_get_seq_round_trip`
  and `range_checksum_is_non_zero_on_non_empty_scan`, both of
  which decode the histogram via Rust's `LatencyHistogram::
from_base64_v2_deflate` after a real bench round-trip.
- If `hdrhistogram-go` ever ships a fix making
  `normalizingIndexOffset` configurable (or zero by default), the
  byte-patch can be deleted in favour of the library's `Encode`
  output verbatim.
