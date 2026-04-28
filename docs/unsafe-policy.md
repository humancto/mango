# Unsafe-growth policy

Mango treats every new `unsafe` block as a reviewed event, not
an accident. The machinery that enforces this is a
[cargo-geiger](https://github.com/geiger-rs/cargo-geiger) count
of `unsafe` sites in the workspace, compared against a committed
`unsafe-baseline.json` on every PR.

This doc is the policy. The CI enforcement is
[`.github/workflows/geiger.yml`](../.github/workflows/geiger.yml);
the baseline lives in
[`unsafe-baseline.json`](../unsafe-baseline.json); the helper
scripts are under
[`scripts/geiger-*.sh`](../scripts/).

## Why this exists (three-layer defense)

Mango's `unsafe` posture is enforced at three layers:

1. **Compile-time** — `unsafe_code = "forbid"` in the workspace
   `Cargo.toml`. Any crate that wants to write `unsafe` must
   carry `#![allow(unsafe_code)]` and justify it.
2. **Runtime UB** — [Miri](miri.md) on the curated subset
   (`[workspace.metadata.mango.miri]`). Catches aliasing,
   provenance, uninit reads inside the `unsafe` blocks that do
   land.
3. **Growth** — this policy. Counts the `unsafe` sites per
   crate and fails CI if the count grows without an explicit
   PR label + author-written baseline bump.

The three are orthogonal. Compile-time says "you can't add
`unsafe` without flipping a crate-level flag". Miri says "the
`unsafe` that is there is sound". This layer says "a reviewer
has consciously approved the new `unsafe` appearing — it didn't
slip past as noise in a large diff."

## What counts

cargo-geiger reports five categories of `unsafe` per crate.
`geiger-check.sh` sums each across mango workspace crates and
compares against the baseline:

| Category      | Example                                                                 |
| ------------- | ----------------------------------------------------------------------- |
| `functions`   | `unsafe fn foo() { … }`                                                 |
| `exprs`       | `unsafe { *ptr }`, `unsafe { f() }` — one per unsafe expression         |
| `item_impls`  | `unsafe impl Send for T {}`                                             |
| `item_traits` | `unsafe trait Foo { … }`                                                |
| `methods`     | `unsafe fn foo(&self);` in a trait, `unsafe fn bar(&self)` on an `impl` |

Note: `exprs` counts _individual unsafe expressions_, not unsafe
blocks. An `unsafe { f(&*p) }` block can contribute more than
one to the `exprs` count because the dereference, the reference,
and the call are all counted as expressions. Don't try to reason
about the exact number from source; trust what geiger reports.

## Workspace-member filter

The gate scopes to Mango's own crates — transitive dependency
`unsafe` is cargo-vet's concern, not this tool's. The scripts
derive the authoritative member set from:

```sh
cargo metadata --no-deps --format-version=1 \
    | jq -r '.workspace_members[]' \
    | awk '{print $1}'
```

cargo-geiger output is then filtered to keep only packages whose
`.package.id.name` is in that set. No name-prefix heuristic;
adding or renaming a crate Just Works as long as it appears in
`[workspace] members`.

## Policy: monotonic

The rule per category, per event:

| Event          | Current vs baseline  | Label present | Baseline updated in PR  | Verdict                                     |
| -------------- | -------------------- | ------------- | ----------------------- | ------------------------------------------- |
| `pull_request` | all ≤ baseline       | —             | —                       | PASS                                        |
| `pull_request` | any > baseline       | no            | —                       | FAIL — growth without approval (exit 1)     |
| `pull_request` | any > baseline       | yes           | no                      | FAIL — missing baseline bump (exit 2)       |
| `pull_request` | any > baseline       | yes           | yes, matches current    | PASS                                        |
| `pull_request` | any > baseline       | yes           | yes, mismatches current | FAIL — baseline stale (exit 2)              |
| `push: main`   | all ≤ baseline       | —             | —                       | PASS                                        |
| `push: main`   | any > baseline       | —             | —                       | FAIL — should be unreachable (PR gate lets) |
| `merge_group`  | same as `push: main` | —             | —                       | same                                        |

**Shrinkage is free.** A PR that legitimately removes `unsafe`
does not need a baseline bump. The baseline may sit above
current counts indefinitely until a maintainer chooses to
re-anchor it via a small follow-up PR (run
`scripts/geiger-update-baseline.sh`, commit, push).

On `merge_group` the gate falls back to "growth is always a
failure" because the merge-queue event payload does not reliably
carry PR labels (`merge_group.pull_requests` is an array that
may be summarized). This is correct: the PR event has already
approved any legitimate growth — the merge-queue run is a
re-verification, and a fresh growth appearing at merge-queue
time really is a regression.

## How to introduce new `unsafe`

1. Write the `unsafe` block with a `// SAFETY:` comment (see
   `CONTRIBUTING.md` §7 for the format).
2. Rebase on `origin/main` first. Without this, your baseline
   numbers can drift from current-truth if another PR has
   shrunk unsafe counts since you branched.
3. Run `bash scripts/geiger-update-baseline.sh`. This runs
   cargo-geiger per workspace member, sums the counts, and
   rewrites `unsafe-baseline.json` with the new numbers plus
   a fresh ISO-8601 timestamp.
4. If the crate is new to the `unsafe` surface (i.e., it now
   carries `#![allow(unsafe_code)]`), also add it to
   `[workspace.metadata.mango.miri]` in the same PR so Miri
   covers it. See [`docs/miri.md`](miri.md).
5. Commit `unsafe-baseline.json` + the code + (if applicable)
   the Miri subset update in the same PR.
6. Ask a maintainer to apply the `unsafe-growth-approved`
   label. The gate will not pass without it.

## Failure modes

`geiger-check.sh` uses distinct exit codes so a reviewer can
tell at a glance which failure hit:

| Exit | Meaning                                                                         | Remediation                                                                                                                                                                    |
| ---: | ------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
|    0 | PASS                                                                            | —                                                                                                                                                                              |
|    1 | Growth without `unsafe-growth-approved` label                                   | Justify the growth; ask maintainer to apply the label; re-run.                                                                                                                 |
|    2 | Growth with label but baseline not updated / stale                              | Run `scripts/geiger-update-baseline.sh`, commit the diff, push.                                                                                                                |
|    3 | Scan-result validation error (unparseable JSON / schema)                        | Tooling broke. Check geiger version; retry. Not an unsafe-growth event. Schema variant: required storage dep absent or `tolerance` field present — fix `unsafe-baseline.json`. |
|    4 | Baseline `cargo_geiger_version` ≠ installed                                     | Bump cargo-geiger pin in workflow + baseline (see "Version-bump procedure").                                                                                                   |
|    6 | Storage-dep growth (redb / raft-engine) — per-category +10 budget exceeded      | ADR 0002 §5 trigger #8 refresh required. See "Storage-dep coverage" below.                                                                                                     |
|    7 | Storage-dep re-pin needed: source/version drift, dep absent, or stranger source | See "Storage-dep coverage" below.                                                                                                                                              |

## Storage-dep coverage (ROADMAP:823, ADR 0002 §5 trigger #8)

The default workspace gate (above) only counts unsafe sites in
mango's own crates. ADR 0002 §5 advisory trigger #8 requires us
to also watch the unsafe surface of the two storage deps the
storage layer depends on most directly: `redb` and `raft-engine`.
Either +10 over baseline (per cargo-geiger category) trips CI and
requires an ADR 0002 §5 refresh before the bump merges.

### Schema

`unsafe-baseline.json` carries a `storage_deps_required` flag
and a `storage_deps` map keyed by crate name:

```json
{
  "storage_deps_required": true,
  "storage_deps": {
    "redb": {
      "source": { "Registry": { "name": "...", "url": "..." } },
      "version": "<x.y.z>",
      "totals": { "functions": N, "exprs": N, "item_impls": N, "item_traits": N, "methods": N },
      "forbids_unsafe": false
    },
    "raft-engine": {
      "source": { "Git": { "url": "...", "rev": "<sha>" } },
      "version": "<x.y.z>",
      "totals": { ... },
      "forbids_unsafe": false
    }
  }
}
```

The `source` object mirrors cargo-geiger 0.13.0's externally-tagged
`Source` enum verbatim: `Path` / `Git{url,rev}` / `Registry{name,url}`.
The checker matches by `(name, source, version)` — version is part of
the key because the `Source` enum doesn't carry it, so a Registry
version bump from the same index produces an identical source object.

### Policy

| Condition                                                                          | Verdict                                            |
| ---------------------------------------------------------------------------------- | -------------------------------------------------- |
| `storage_deps_required: false` or absent                                           | Block dormant; workspace gate continues normally   |
| `storage_deps_required: true`, `redb` or `raft-engine` missing from `storage_deps` | FAIL — exit 3 (schema bypass-prevention)           |
| `tolerance` field present on any per-dep entry                                     | FAIL — exit 3 (B3 — tolerance is hardcoded +10)    |
| Per-category `current ≤ baseline + 10`, all match                                  | PASS                                               |
| Any single category: `current > baseline + 10`                                     | FAIL — exit 6, ADR 0002 §5 refresh required        |
| Stranger detector: scan contains required dep at unexpected source                 | FAIL — exit 7, re-pin baseline + review intent     |
| Per-dep match misses on `(name, source, version)`, same source different version   | FAIL — exit 7 (version drift)                      |
| Required dep absent from scan AND still in `Cargo.toml`                            | FAIL — exit 7 (cargo-geiger / feature unification) |
| Required dep absent from scan AND from `Cargo.toml`                                | FAIL — exit 7 (dep removed; bump baseline + ADR)   |

There is no `storage-growth-approved` label or any equivalent
label-only bypass: storage-dep growth requires a written ADR
refresh — a process that a single label cannot encode. The
reviewer looks for an ADR diff alongside the baseline diff in
the same PR.

### Bumping the storage-dep pin (intentional growth path)

When redb or raft-engine legitimately picks up new unsafe (a
release that reorganizes internals; a fork rebase that adds an
FFI shim; etc.):

1. Refresh ADR 0002 §5 trigger #8 with the new numbers and a
   sentence on what new unsafe surface appeared.
2. Run `MANGO_GEIGER_REPIN=1 bash scripts/geiger-update-baseline.sh`.
   The `MANGO_GEIGER_REPIN=1` env var is required to rewrite
   `storage_deps.*.source` and `.version`; default mode preserves
   them and would refuse to silently accept the drift (exit 5).
3. Commit ADR + baseline in the same PR.

### Flake remediation (M7)

cargo-geiger has small per-run nondeterminism on a multi-thousand-
token surface. If the gate fires within the +10 tolerance window
(e.g. a CI run reports `redb.exprs = baseline + 3` after a
runner-image refresh), the remediation is to re-anchor the
baseline only — no ADR refresh required, because no real growth
happened. Procedure:

1. Re-run the geiger workflow. If the numbers persist:
2. Run `bash scripts/geiger-update-baseline.sh` (default mode —
   source/version stay pinned, only `.totals` updates).
3. Commit with message `chore(geiger): re-anchor storage-dep
baseline (cargo-geiger flake)`.

ADR 0002 §5 trigger #8 only requires refresh on real growth
(category > baseline + 10). In-tolerance re-anchors are noise;
treating them as ADR events would punish reproducibility, not
reward it.

## Transitive `unsafe` as supply-chain signal

cargo-geiger also reports unsafe density for every transitive
dep. The current policy does NOT gate on that — it's
informational. A follow-up `cargo-vet` item (ROADMAP.md:798)
will use this density as one input to the vetting decision when
a new dep is added. Until then, the geiger JSON is a passive
record reviewers can consult.

## Version-bump procedure

cargo-geiger is pinned exactly (`0.13.0` today) in
`.github/workflows/geiger.yml` as `CARGO_GEIGER_VERSION`, and
the same version is stamped into `unsafe-baseline.json` as
`cargo_geiger_version`. `geiger-check.sh` enforces the two
match (exit 4 on skew).

To bump:

1. Update `CARGO_GEIGER_VERSION` in the workflow env.
2. Locally install the new version:
   `cargo install --locked cargo-geiger --version <new>`.
3. Re-run the full pipeline:
   `bash scripts/geiger-update-baseline.sh` — writes the new
   version + (possibly different) counts into the baseline.
4. Run `bash scripts/geiger-scripts-test.sh` — confirms the
   fixtures still parse under the new schema. If they drift,
   regenerate them with `bash scripts/geiger-gen-fixtures.sh`.
5. Commit the three changes (workflow env, baseline, fixtures)
   in one PR. rust-expert review.

## Known limitations

- **Per-crate movement.** The gate checks totals. Moving an
  `unsafe` block between crates net-zero passes silently. For
  a single-unsafe-crate workspace this is academic; if the
  topology splits into safety-boundary crates in the future,
  swap to per-crate enforcement (one jq change in
  `geiger-check.sh`).
- **Reproducibility across machines.** The baseline is
  deterministic given (cargo-geiger version, toolchain,
  source). A contributor running the updater on macOS with a
  different toolchain than CI's Ubuntu stable may get
  different counts. If this becomes a real problem, move the
  updater to run inside a Docker image that mirrors CI.
- **Fixture drift.** The synthetic fixtures under
  `tests/fixtures/geiger/` could in principle pass the check
  scripts while real geiger output fails. Mitigation: the
  test harness includes a real-geiger-on-toy-workspace
  scenario (`tests/fixtures/geiger-toy-workspace/`) that
  breaks if the schema drifts.

## MSRV interaction

cargo-geiger runs as a host tool on CI — it is installed via
`cargo install --locked` with the CI runner's stable toolchain
and does _not_ build against the workspace's MSRV (1.89).
What MSRV governs is whether the source parses when geiger
shells out to `rustc`/`cargo metadata`; stable-1.89 source
parses fine under any recent stable.

Put differently: bumping cargo-geiger is independent of bumping
`rust-version` in the workspace `Cargo.toml`. The two pins move
on separate schedules.

## Sanity-break recipe

A gate that has never failed is indistinguishable from a gate
that is broken. Periodically — or after any change to the check
scripts — verify the gate would fire:

```rust
// In crates/mango-loom-demo/src/lib.rs, inside tests mod.
// DO NOT COMMIT. Remove after confirming.
#[test]
fn sanity_break_geiger_would_catch_this() {
    let _: u32 = unsafe { std::mem::zeroed::<u32>() };
}
```

Run `bash scripts/geiger-update-baseline.sh --dry-run` (prints
the diff without mutating the baseline) and confirm
`unsafe_.exprs` would grow. Then
`bash scripts/geiger-check.sh <scanned-json> unsafe-baseline.json`
must exit 1 (growth without label).

For CI-side verification, the `geiger-sanity-break` job in
`.github/workflows/geiger.yml` runs this recipe automatically
on `workflow_dispatch`. A maintainer can fire it from the
Actions UI.

## See also

- [`docs/miri.md`](miri.md) — runtime UB detection; complements
  this policy. Miri catches _that_ `unsafe` is UB; this gate
  catches _when_ `unsafe` grows.
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) §7 — unsafe policy;
  §8 — local commands.
- [`ROADMAP.md`](../ROADMAP.md) — item 0.6 where this policy
  was declared; item 0.8 for the cargo-vet follow-up that
  uses geiger's transitive density.
- [cargo-geiger README](https://github.com/geiger-rs/cargo-geiger) —
  upstream docs, flag reference.
