# Plan v2: cargo-geiger storage-dep baseline pin (ROADMAP:823)

> v2 incorporates rust-expert REVISE feedback (S1, S2, S3, B1–B4, M1, M2, M3, M4, M5, M6, N1, N2, N3, N5).
> v1 archived in git history; key changes are noted inline.

## Goal

Pin cargo-geiger token counts for the two storage deps named in
ADR 0002 §5 advisory trigger #8 — `redb` and `raft-engine` — so
that a silent unsafe-token regression on either dep trips CI
before the bump merges.

ROADMAP item (line 823, verbatim):

> **cargo-geiger baseline pin** for storage deps: redb 4.1.0 = 37
> `unsafe` tokens in src/; raft-engine master @ pinned SHA = 49
> tokens. Either +10 over baseline trips CI and requires ADR 0002
> refresh before the bump merges (per ADR 0002 §5 advisory trigger
> #8).

The existing Phase 0.5 cargo-geiger gate (workspace-internal,
filtered to `Path`-sourced packages — task #19) does NOT cover
external deps. Extending it is the work.

## Authoritative ground truth

cargo-geiger 0.13.0's `Source` enum
([upstream](https://github.com/geiger-rs/cargo-geiger/blob/cargo-geiger-0.13.0/cargo-geiger-serde/src/source.rs)):

```rust
pub enum Source {
    Git { url: Url, rev: String },
    Registry { name: String, url: Url },
    Path(Url),
}
```

With serde's externally-tagged default representation, the JSON
shapes are:

- `{"Path": "file:///..."}` (single-string variant)
- `{"Git": {"url": "https://...", "rev": "..."}}`
- `{"Registry": {"name": "crates-io", "url": "..."}}`

The plan's source-match schema mirrors these shapes exactly.
**No translation, no abbreviation, no `kind:` indirection.** The
checker's match logic is `==` after key-sorting.

(v1 used `{"kind": "Git", "repository": "..."}` — wrong field
name, would never match. v1 is archived.)

## Schema change

Add a top-level `storage_deps` field. Existing fields untouched.

```json
{
  "generated_by": "scripts/geiger-update-baseline.sh",
  "generated_at": "2026-04-28T00:00:00Z",
  "cargo_geiger_version": "0.13.0",
  "crates": { ... },
  "totals": { ... },

  "storage_deps_required": true,

  "storage_deps": {
    "redb": {
      "source": {
        "Registry": {
          "name": "crates-io",
          "url": "https://github.com/rust-lang/crates.io-index"
        }
      },
      "version": "4.1.0",
      "totals": {
        "functions":   <int>,
        "exprs":       <int>,
        "item_impls":  <int>,
        "item_traits": <int>,
        "methods":     <int>
      },
      "forbids_unsafe": false
    },
    "raft-engine": {
      "source": {
        "Git": {
          "url": "https://github.com/humancto/raft-engine",
          "rev": "e1d738d9ad1c1fc4f5b21c8c73bf605b5696f535"
        }
      },
      "version": "0.4.2",
      "totals": { ... },
      "forbids_unsafe": false
    }
  }
}
```

Notes:

- `storage_deps_required: true` is the **bypass-prevention flag**
  (S2). The checker errors with exit 3 if `storage_deps_required`
  AND any of `required_storage_deps=("redb" "raft-engine")`
  (hardcoded in the checker) is missing from `storage_deps`.
  The bootstrap PR adds the flag and both entries in one shot;
  removing either entry afterwards is a hard CI failure.
- `tolerance` field REMOVED (was per-dep in v1). Tolerance is
  hardcoded `+10 per category` in the checker (B3). The checker
  rejects `tolerance` field if present in the baseline (forces
  policy uniformity).
- `forbids_unsafe` mirrors cargo-geiger's per-package field
  (R3, diagnostic only — not a gate).
- Numbers populated by running cargo-geiger in CI (one-shot
  bootstrap PR) — NOT hand-typed from ROADMAP's "37"/"49"
  (those are `rg unsafe` counts; cargo-geiger reports
  per-category and may disagree). M6: ROADMAP line 823 gets
  edited in the same PR to point at `unsafe-baseline.json`
  rather than carry a literal number that's about to disagree
  with the gate.

## Policy

Per dep, per scan:

| Condition                                                                                                                                | Verdict                                                              |
| ---------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| `storage_deps_required: true` and any required dep absent from `storage_deps`                                                            | FAIL — exit 3 (schema)                                               |
| All categories: `current ≤ baseline + 10`                                                                                                | PASS                                                                 |
| Any single category: `current > baseline + 10`                                                                                           | FAIL — exit 6, "ADR 0002 refresh"                                    |
| `source` in scan ≠ baseline.source                                                                                                       | FAIL — exit 7, "re-pin baseline"                                     |
| Dep declared in `Cargo.toml` (per `cargo metadata`) but absent from scan                                                                 | FAIL — exit 7, "cargo-geiger / feature unification bug; investigate" |
| Dep absent from `Cargo.toml` AND from scan                                                                                               | FAIL — exit 7, "dep removed; bump baseline + ADR 0002"               |
| **Stranger detector**: scan contains a package named `redb` or `raft-engine` whose `source` does not match the configured baseline entry | FAIL — exit 7, "unexpected storage-dep source; supply-chain anomaly" |

`baseline` here means each of the five cargo-geiger categories
individually — not the sum. **Per-category +10** (B2). Matches
the existing workspace-gate semantics; defangs the
trade-exprs-for-impls bypass.

`tolerance = 10` is hardcoded in the checker, not configurable.

**Exit codes** (new): 6 = storage-dep growth, 7 = re-pin needed.
0–4 unchanged.

**No label flow.** Workspace `unsafe-growth-approved` is
"maintainer reviewed the new SAFETY comment"; storage-dep growth
requires a written ADR refresh — a process the label cannot
encode. Remediation message points at ADR 0002 §5 + updater
script. The reviewer looks for ADR diff alongside baseline diff
in the same PR.

**Positive assertion**: no future PR shall add a
`storage-growth-approved` label or any equivalent label-only
bypass. ADR refresh is the only remediation path. Documented
in `unsafe-policy.md` (N3).

## Implementation

### Files touched

1. `unsafe-baseline.json` — add `storage_deps_required: true`
   and `storage_deps: {redb, raft-engine}` with real
   cargo-geiger numbers from CI.
2. `scripts/geiger-update-baseline.sh` — see "Updater semantics"
   below.
3. `scripts/geiger-check.sh` — add `check_storage_deps` function;
   document exits 6/7.
4. `scripts/geiger-scripts-test.sh` — scenarios 16–22 (see
   "Testing" below).
5. `tests/fixtures/geiger/` — new fixtures, including at least
   one **lifted verbatim from a real CI cargo-geiger run** (M3,
   not hand-shaped). Filename:
   `storage-deps-real.json`. The other fixtures (synthetic, for
   exit-6/7 scenarios) are derived from this one.
6. `docs/unsafe-policy.md` — new "Storage-dep coverage" section
   - extend the existing exit-code table to include 6/7 (N2).
7. `.github/PULL_REQUEST_TEMPLATE.md` (or wherever the PR
   template lives) — add a checkbox for storage-dep PRs:
   "If this PR triggers exit 6 in `geiger`, ADR 0002 §5 trigger
   #8 has been refreshed in this same PR." (M5)
8. `ROADMAP.md` line 823 — replace literal "37"/"49" with
   pointer to `unsafe-baseline.json#storage_deps` (M6).
9. `.github/workflows/geiger.yml` — `paths:` filter already
   covers `Cargo.lock` and `unsafe-baseline.json`. No workflow
   change.

### Updater semantics (B1)

The updater has three modes, controlled by env vars:

| Env                         | What it rewrites                                                                                                               |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| (default)                   | `crates`, `totals`, `storage_deps.*.totals`, `storage_deps.*.forbids_unsafe`. **Never** `storage_deps.*.source` or `.version`. |
| `MANGO_GEIGER_REPIN=1`      | Above, PLUS `storage_deps.*.source` and `.version` (re-pins from current scan).                                                |
| `GEIGER_FROM_MERGED_JSON=…` | Test escape hatch: skip cargo-geiger invocation, read merged JSON from the env var. Existing.                                  |

In default mode, if the merged scan contains a storage dep
whose `source` does NOT match the baseline pin, **the updater
errors with exit 5** — does NOT silently emit a stale baseline.
This preserves the round-trip oracle: checker accepts updater
output ⟺ no source drift between scan and baseline (default
mode) OR maintainer consciously re-pinned (`MANGO_GEIGER_REPIN`
mode).

(v1 left this implicit; the expert flagged it as a real
oracle-breaking bug.)

### Checker semantics

Pseudocode for the storage-dep branch:

```
required_deps = ("redb" "raft-engine")  # hardcoded

# B5: dormancy guard. When the flag is false, the ENTIRE
# storage-dep block is skipped — stranger detector, per-dep
# loop, schema check, and cargo-metadata invocation. This is
# the single bypass and exists ONLY for the bootstrap window
# between commit 1 and commit 2 of this PR. After commit 2
# lands, `storage_deps_required` is true permanently and any
# PR that flips it back to false fails review (codified in
# unsafe-policy.md).
if not baseline.storage_deps_required:
    return  # storage-dep checks dormant; workspace gate continues normally

# B3: tolerance field rejection. Per-dep tolerance overrides
# would let a maintainer slowly weaken the gate one PR at a
# time. Hardcoded `10` is the policy.
for dep_name, dep in baseline.storage_deps.items():
    if "tolerance" in dep:
        exit 3 "tolerance is not configurable per-dep; remove the field from storage_deps.${dep_name}"

for dep_name in required_deps:
    if dep_name not in baseline.storage_deps:
        exit 3 "schema: storage_deps.${dep_name} required when storage_deps_required: true"

# Stranger detector (M1) — runs BEFORE per-dep checks.
# Enumerate ALL strangers in the diagnostic, not just the first
# (so [patch] tables and vendor manifests are obvious from one
# glance at CI output).
strangers = []
for pkg in scan.packages where pkg.name in required_deps:
    pinned = baseline.storage_deps[pkg.name]
    if pkg.source != pinned.source:
        strangers.append((pkg.name, pkg.source, pinned.source))
if strangers:
    print "unexpected storage-dep sources detected:"
    for (name, scan_src, pinned_src) in strangers:
        print "  ${name}: scan=${scan_src}  pinned=${pinned_src}"
    exit 7

# C1: cache cargo metadata once. Invoked only on the matches=0
# branch (B4 disambiguation), but we cache to avoid duplicate
# spawns if multiple deps end up in that branch.
cargo_metadata_cache = None

def cargo_metadata_has(dep_name):
    global cargo_metadata_cache
    if cargo_metadata_cache is None:
        if not command_exists("cargo"):
            # Defensive: existing checker is tolerant of cargo-geiger
            # missing locally; preserve that contract for `cargo`. We
            # cannot disambiguate B4's two cases, so default to the
            # safer message (assume feature-unification bug, prompt
            # investigation) and continue.
            return None  # caller treats None as "unknown"
        cargo_metadata_cache = `cargo metadata --no-deps --format-version=1 --offline 2>/dev/null` \
                              .packages[].name
    return dep_name in cargo_metadata_cache

for dep_name in required_deps:
    pinned = baseline.storage_deps[dep_name]
    matches = [pkg for pkg in scan.packages
               if pkg.name == dep_name and pkg.source == pinned.source]

    if len(matches) == 0:
        # Distinguish "dep removed" from "scan dropped it" (B4).
        has_dep = cargo_metadata_has(dep_name)
        if has_dep is True:
            exit 7 "${dep_name} declared in Cargo.toml but absent from scan; cargo-geiger / feature-unification bug; investigate"
        elif has_dep is False:
            exit 7 "${dep_name} removed from Cargo.toml; bump baseline and refresh ADR 0002"
        else:  # cargo not on PATH
            exit 7 "${dep_name} absent from scan and cargo metadata unavailable; cannot disambiguate. Install cargo or run in a workspace with cargo on PATH."

    # Dedup with identical-counts assertion (S3).
    counts_set = unique(m.unsafety.used for m in matches)
    if len(counts_set) > 1:
        exit 3 "${dep_name} reported with inconsistent counts across ${len(matches)} occurrences in merged scan"
    current = counts_set[0]

    for cat in (functions, exprs, item_impls, item_traits, methods):
        if current[cat] > pinned.totals[cat] + 10:
            exit 6 "${dep_name}.${cat}: ${current[cat]} > ${pinned.totals[cat]} + 10
                    Remediation:
                      1. Refresh ADR 0002 §5 trigger #8 with the new numbers.
                      2. Run MANGO_GEIGER_REPIN=1 bash scripts/geiger-update-baseline.sh
                      3. Commit ADR + baseline in the same PR."
```

`cargo metadata` invocation in the checker is the existing
pattern from `geiger-update-baseline.sh:74`; copy.

### Stranger detector — concrete predicate (M1)

A "stranger" is any scanned package whose `name` is in
`required_deps` but whose `source` does not match the baseline
pin. Triggers exit 7 with diagnostic naming both sources.

This catches:

- A maintainer accidentally adding `redb = { path = "../local" }`
  to test changes.
- A supply-chain substitution (some other crate forks redb at a
  fork URL and patches it in via `[patch]`).
- A future raft-engine fork URL change.

Cargo's resolution unifies `(name, version)` per workspace, so
the case "redb appears at two distinct sources in one scan"
cannot happen unless someone uses `[patch]` or vendoring. In
those cases, the gate fires — by design.

## Testing

### Local: `bash scripts/geiger-scripts-test.sh`

Adds scenarios 16–22 (numbered after existing 15):

- **16. storage-dep within tolerance** — current = baseline + 5
  per category, exit 0. Uses the real fixture.
- **16b. dormancy guard (B5)** — `storage_deps_required: false`
  baseline + scan contains a redb at an unexpected source
  (would normally fire stranger detector). Exit 0. This is
  the deliberate bootstrap-window bypass; documented as
  intentional. Without this scenario the dormancy guard could
  silently regress from "skip everything" to "still run
  stranger detector" and we'd miss it.
- **17. storage-dep over tolerance per-category** — exprs +11
  while other categories unchanged, exit 6, stdout includes
  ADR refresh remediation.
- **18. storage-dep category-trade attempt (B2 regression
  test)** — exprs −11, methods +11 (sum unchanged). Exit 6
  (per-category check catches this even though aggregate
  doesn't grow).
- **19. storage-dep version drift (Registry)** — redb scan
  reports 4.2.0 while baseline pins 4.1.0, exit 7.
- **20. storage-dep absent from scan, present in Cargo.toml**
  — exit 7 with "feature-unification" message.
- **21. storage-dep absent from both** — exit 7 with "dep
  removed" message.
- **22. interaction with workspace gate (M4) — full
  throwaway-repo setup (B6).** This scenario MUST mirror the
  scenario 4/5 pattern exactly: `git init --quiet repo-22`,
  base commit with workspace baseline at totals X and
  `storage_deps.redb.totals` at Y, head commit bumps the
  workspace baseline to X+1 (matches the workspace fixture's
  scan totals → workspace gate would PASS) AND keeps
  storage_deps.redb at Y. Scan reports workspace totals X+1
  AND redb at Y+11. Env: `GITHUB_EVENT_NAME=pull_request`,
  `PR_LABELS='["unsafe-growth-approved"]'`,
  `git update-ref refs/remotes/origin/main refs/heads/main`.
  Assertions: exit code is 6 (NOT 0, NOT 1), stdout contains
  the `redb.exprs` remediation text and does NOT contain the
  workspace `unsafe-growth-approved` PASS message.
  Without this scenario you have a half-test that proves
  nothing about non-bypassability.

Plus extending existing scenarios:

- **14b. updater round-trip with storage_deps** — synthetic
  merged JSON includes redb + raft-engine entries matching
  baseline source; updater (default mode) writes new totals;
  checker accepts. Then mutate the merged JSON's redb source
  to a different version; updater (default mode) exits 5
  (B1). Updater with `MANGO_GEIGER_REPIN=1` accepts and
  rewrites.
- **23. storage_deps_required + missing dep entry → exit 3
  (S2)** — baseline has `storage_deps_required: true` but
  `storage_deps.raft-engine` deleted. Exit 3 schema error.
  This is the bypass-prevention test.
- **24. nondeterminism re-pin (M7)** — same source, same
  version, baseline.totals.exprs = 30, scan reports 31.
  Updater (default mode) accepts (it's an in-tolerance
  re-anchor); checker accepts after the rewrite. No ADR
  refresh required. Documents the recovery path for
  cargo-geiger flake within the +10 budget.

Existing scenarios 1–15 must continue to pass unchanged.

### CI

Existing `paths:` trigger covers all inputs. Storage-dep gate
fires on every PR / push:main / merge_group automatically.

### Real-fixture provenance (M3)

To populate the bootstrap baseline numbers and the
`storage-deps-real.json` fixture, the bootstrap PR includes
its CI run's `geiger.json` (extracted to just the redb +
raft-engine entries) committed as the fixture. Every
subsequent bump that re-pins regenerates this fixture. The
synthetic fixtures for scenarios 17–23 are derived by editing
copies of the real fixture.

Local cargo-geiger runs are documented as **non-authoritative**
in `unsafe-policy.md` (R2): contributors should run
`gh workflow run geiger.yml` and copy from logs, not run
locally. (macOS-vs-Ubuntu drift on a 86-token surface is real.)

## Risks (carried over and re-evaluated)

1. **Renovate weekly-bump pressure on raft-engine SHA (R1).**
   Default mode rejects SHA drift → maintainer must
   `MANGO_GEIGER_REPIN=1` per Renovate PR. Habituation is real.
   **Mitigation deferred** (out of scope, see follow-ups
   below): future updater extension to auto-repin if totals
   are within tolerance and only the `rev` moved. For this
   PR, accept the friction — better than silent SHA drift.
2. **cargo-geiger nondeterminism scales with surface area
   (R2).** Documented in `unsafe-policy.md` "Reproducibility";
   storage-dep section adds an explicit "CI-only origin" note.
   **M7 — flake remediation procedure**: if cargo-geiger
   nondeterminism trips the gate within the +10 tolerance
   window (e.g., post-bootstrap, the next PR's CI reports
   redb.exprs = N+3 due to runner drift), the remediation is
   to re-pin the baseline only — no ADR refresh required for
   nondeterminism-driven flake. ADR 0002 §5 trigger #8 only
   requires refresh on real growth (>+10). Procedure: (a)
   re-run the geiger workflow, (b) if numbers persist, run
   `bash scripts/geiger-update-baseline.sh` (default mode,
   re-anchors totals only), (c) commit the diff with message
   `chore(geiger): re-anchor storage-dep baseline (cargo-geiger
flake)`. Scenario 24 covers this.
3. **`forbids_unsafe` flip is a stronger signal than +10
   (R3).** Recorded in baseline, displayed in diagnostic
   message. **Not a gate** in this PR (would change semantics
   from "+10 tolerance" to "any change"). Possible
   follow-up: gate on `forbids_unsafe: true → false`
   transitions specifically.

## Bootstrap order (N1: split commits)

To make the diff reviewable and the revert story clean:

- **Commit 1** — schema + checker + tests + fixtures, with
  `storage_deps_required: false` and **no** `storage_deps`
  section. Tests cover the absent-but-not-required path
  (PASS) AND the required-but-absent path (exit 3). Workspace
  gate semantics 100 % unchanged.
- **Commit 2** — flip `storage_deps_required: true` and add
  the `storage_deps: {redb, raft-engine}` section with real
  numbers from this PR's CI run. Update ROADMAP line 823.
  Update PR template. Update unsafe-policy.md.

Both commits build, both pass `geiger-scripts-test.sh`, both
pass CI's geiger job. After commit 2, the gate is live.

## Acceptance criteria

- [ ] `unsafe-baseline.json` has populated `storage_deps`
      section + `storage_deps_required: true`, totals from a
      real CI cargo-geiger scan.
- [ ] `bash scripts/geiger-check.sh geiger.json unsafe-baseline.json`
      exits 0 against the current tree.
- [ ] `bash scripts/geiger-scripts-test.sh` runs all 15 old
      scenarios + 8 new (16, 16b, 17–22) + 14b + 23, 24, all
      green.
- [ ] **M8 — bootstrap-ordering verification**: PR description
      includes a checkbox confirming the reviewer ran `git
  checkout HEAD~1 && bash scripts/geiger-scripts-test.sh`
      on the branch tip and saw it pass on commit 1 (with
      `required: false`) AND on commit 2 (with `required:
  true`). Without this manual check, the "every commit
      green" property is asserted but unverified.
- [ ] `docs/unsafe-policy.md` has a "Storage-dep coverage"
      section AND the exit-code table is extended with 6/7
      AND a "Flake remediation" subsection (M7 procedure).
- [ ] PR template has the storage-dep + ADR-refresh checkbox.
- [ ] ROADMAP.md line 823 no longer carries a literal token
      count; points at the baseline file instead.
- [ ] CI `geiger` job passes on the PR.
- [ ] rust-expert APPROVE on final diff.

## Out of scope (follow-ups, not blockers)

- Auto-repin updater extension for SHA-only drift within
  tolerance (R1 mitigation).
- Sanity-break job extension for the storage-dep gate
  (CI-side mutation test).
- Per-category breakdown in the GitHub step summary.
- Auto-issue-filing on exit-6 events (mirrors cargo-audit
  pattern).
- Coverage for additional deps beyond redb / raft-engine.
- Gating on `forbids_unsafe` true→false transitions.

## Branch + commit shape

- Branch: `feat/cargo-geiger-storage-baseline`
- Commit 1 (`feat(geiger):`): schema fields + checker logic +
  test scenarios + fixtures + docs (with `storage_deps_required:
false`).
- Commit 2 (`feat(geiger):`): flip required flag, add real
  numbers, ROADMAP+PR template+policy doc updates.

Each commit independently passes `bash
scripts/geiger-scripts-test.sh` and `cargo build --workspace`.

## Open questions to resolve in implementation

None of these block plan approval; they are concrete
implementation details the PR will pin down:

- Exact stdout shape of exit-6/7 messages. Follow existing
  `print_growth_summary` formatting for consistency (N4).
- Fixture file naming. Existing convention is
  `<scenario>-<descriptor>.json`; new ones follow.
- **Registry URL value verification**: the schema example uses
  `https://github.com/rust-lang/crates.io-index`; cargo-geiger
  0.13.0's actual emitted URL must be cross-checked against
  the real fixture before commit 2 lands. Some versions emit
  `https://index.crates.io/`. The bootstrap PR's first CI run
  reveals the canonical value; commit 2 uses that verbatim.
