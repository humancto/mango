# Plan: `cargo-msrv` CI job (MSRV pin at 1.80)

Roadmap item: Phase 0 — "Add a `cargo-msrv` job pinning the minimum
supported Rust version (start at 1.80, bump deliberately) so we don't
accidentally raise it." (`ROADMAP.md:756`)

## Goal

Stop accidental MSRV raises at merge time. Today `Cargo.toml` already
sets `rust-version = "1.80"` in `[workspace.package]` — that's a
**declaration**, not a **gate**. A contributor using a newer stdlib
method (`Option::is_none_or`, stable 1.82; `Duration::abs_diff`,
stable 1.81) will write code that compiles locally on their `stable`
toolchain and passes CI (which also runs on `stable`), but breaks
every downstream pinned to 1.80. MSRV is never enforced until
someone actually builds with 1.80 and hits a hard error.

The gate: a new CI job that installs the pinned MSRV toolchain and
runs `cargo check --workspace --all-targets --locked` against it. If
the code can't compile on 1.80, CI fails and the PR is blocked.

## North-star axis

**Maintainability + supply-chain discipline.** MSRV is a contract
with downstream users (library consumers) and with operators
(release managers who pin their build-toolchain). An accidental
MSRV raise silently breaks everyone pinned to the old version — not
an outage, but a reputational/compatibility footgun we explicitly
rejected in the roadmap's "bump deliberately" language. Defense in
depth with `rust-version` in `Cargo.toml`: the manifest field is the
declaration; the CI job is the enforcement.

## Approach

One new CI job in `ci.yml`, placed between `test` and `deny` so the
cheap compile-only jobs cluster and the policy gates cluster. Uses
the existing `dtolnay/rust-toolchain` action with a pinned `1.80`
channel.

**Why not the `cargo-msrv` binary itself?** The `cargo-msrv` tool
(`foresterre/cargo-msrv`) is designed to **discover** the MSRV by
binary-searching toolchain versions — which is overkill and slow
(several minutes) when we already know the MSRV and just want to
enforce it. A single `cargo +1.80 check` run does the same job in
under a minute. The roadmap item is labeled "`cargo-msrv` job"
colloquially; the correct tool for the enforcement phase is the
toolchain install itself. Matches what tokio / serde / hyper /
reqwest all do for MSRV CI.

### D1. `.github/workflows/ci.yml` — new `msrv` job

```yaml
msrv:
  name: msrv (cargo check @ 1.80)
  runs-on: ubuntu-24.04
  timeout-minutes: 15
  steps:
    - uses: actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5 # v4
    # `toolchain: "1.80"` is rustup's shorthand — it resolves to
    # whatever 1.80.z rustup's manifest has at install time
    # (currently 1.80.1). That's the right amount of pinning for
    # MSRV enforcement: we want "any 1.80 patch" not "exact SHA".
    - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8 # stable (action)
      with:
        toolchain: "1.80"
    - name: verify workflow MSRV matches Cargo.toml
      # Dual source of truth: Cargo.toml declares the MSRV, this
      # workflow enforces it. Use `cargo metadata` (not grep) so we
      # parse TOML correctly and catch drift across the whole
      # workspace — not just the root Cargo.toml. `jq` + `sort -u`
      # surfaces any per-crate rust-version that drifts from the
      # workspace default.
      run: |
        manifest_msrvs=$(cargo metadata --format-version=1 --no-deps \
          | jq -r '.packages[].rust_version' | grep -v '^null$' | sort -u)
        count=$(echo "$manifest_msrvs" | wc -l | tr -d ' ')
        if [ "$count" != "1" ]; then
          echo "::error::Inconsistent rust-version across workspace packages:"
          echo "$manifest_msrvs"
          exit 1
        fi
        if [ "$manifest_msrvs" != "1.80" ]; then
          echo "::error::Cargo.toml rust-version='$manifest_msrvs' but workflow pins '1.80'. Update both deliberately."
          exit 1
        fi
    - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32 # v2
      with:
        # Include the pinned MSRV in the cache key so a future MSRV
        # bump invalidates this job's cache cleanly without waiting
        # for a manual `v0-msrv` rev.
        prefix-key: v0-msrv-1.80
        shared-key: ""
    - name: cargo fetch
      run: cargo fetch --locked
    - name: cargo check (workspace, all targets, MSRV toolchain)
      # `cargo check` — not `cargo build` or `cargo test` — because
      # MSRV cares about compilation feasibility, not artifact
      # production or runtime semantics. `--all-targets` includes
      # tests/benches/examples so the full source tree is typechecked
      # on MSRV, not just lib targets (matches tokio/serde/hyper).
      run: cargo check --workspace --all-targets --locked
```

### D2. `clippy.toml` — add `msrv = "1.80"` and enable `incompatible_msrv`

Adding `msrv` to `clippy.toml` by itself is silencing-only: it stops
clippy from **suggesting** post-1.80 APIs in its lints' `help` text.
It does NOT gate anything without also enabling `clippy::incompatible_msrv`.
Since we have strict clippy posture (deny list in `Cargo.toml`), the
consistent choice is to enable it as a deny-level lint too. This
gives a clippy-side PR-time complaint in addition to the MSRV job's
`cargo check` gate.

```toml
# clippy.toml
msrv = "1.80"
```

```toml
# Cargo.toml additions to [workspace.lints.clippy]
incompatible_msrv = { level = "deny", priority = 1 }
```

### D3. Scripted drift test, committed (`scripts/test-msrv-pin.sh`)

The rust-expert flagged that the drift-detection step exists to catch
bit-rot, but has no test of its own — ironic. This script is the
committed gate: it parses `.github/workflows/ci.yml`, extracts the
pinned toolchain literal, compares against `Cargo.toml`'s
`rust-version`, and fails if they disagree. Runs in CI as a step of
the `msrv` job (so PR-time enforcement) AND is runnable locally by a
contributor who bumps either source of truth.

```bash
#!/usr/bin/env bash
# scripts/test-msrv-pin.sh
#
# Asserts the MSRV pin in .github/workflows/ci.yml matches the
# rust-version in Cargo.toml (workspace-wide, via cargo metadata).
# Run from CI and locally. Requires: bash, cargo, jq, yq.
set -euo pipefail

workflow=".github/workflows/ci.yml"

# Extract the toolchain pinned in the msrv job.
workflow_msrv=$(yq -r '.jobs.msrv.steps[] | select(.uses? | test("dtolnay/rust-toolchain")) | .with.toolchain' "$workflow")
if [ -z "$workflow_msrv" ] || [ "$workflow_msrv" = "null" ]; then
    echo "error: could not extract msrv job toolchain from $workflow" >&2
    exit 1
fi

# Extract rust-version from every workspace package (workspace
# inheritance is resolved by cargo metadata).
manifest_msrvs=$(cargo metadata --format-version=1 --no-deps \
  | jq -r '.packages[].rust_version' | grep -v '^null$' | sort -u)
count=$(echo "$manifest_msrvs" | wc -l | tr -d ' ')
if [ "$count" != "1" ]; then
    echo "error: inconsistent rust-version across workspace packages:" >&2
    echo "$manifest_msrvs" >&2
    exit 1
fi

if [ "$workflow_msrv" != "$manifest_msrvs" ]; then
    echo "error: msrv drift detected" >&2
    echo "  ci.yml msrv job toolchain: '$workflow_msrv'" >&2
    echo "  Cargo.toml rust-version:    '$manifest_msrvs'" >&2
    echo "Bump both deliberately and rerun." >&2
    exit 1
fi

echo "ok: MSRV pin matches ($workflow_msrv)"
```

Script is chmod +x, invoked from the `msrv` job as its second step
(replacing the inline grep/jq logic in D1's snippet — the script
becomes the single source), and also invokable from `cargo test`
in a future iteration if we grow a test harness.

Make sure `yq` is available: `ubuntu-24.04` GitHub runners ship with
Python `yq` by default; if that changes, `pip install yq` in a
pre-step.

### D4. Why `cargo check`, not `cargo test` (carried forward)

- **`cargo check` verifies the code compiles on 1.80.** That's the
  MSRV contract.
- **`cargo test` actually runs tests**, adding runtime + the risk a
  test relies on dylib symbols in newer stdlib. Not the MSRV axis.
- Matches upstream Rust practice in tokio / serde / hyper / reqwest.

### D5. `--all-targets` — what it catches (carried forward)

Includes `--lib --bins --tests --benches --examples`. Without it,
the gate only protects library consumers — not test / bench /
example authors. Post-1.80 API in test code compiles on stable CI,
but a downstream running `cargo test` on 1.80 breaks. `--all-targets`
makes the gate protect all direct consumption paths.

## Files to touch

- `.github/workflows/ci.yml` — add `msrv` job (~25 lines).
- `clippy.toml` — add `msrv = "1.80"` (+ comment).
- `Cargo.toml` — add `incompatible_msrv = { level = "deny", priority = 1 }`
  to `[workspace.lints.clippy]`.
- `scripts/test-msrv-pin.sh` — NEW, ~35 lines. `chmod +x`.

No source code changes.

## Edge cases

- **Rust toolchain release cadence**: stable rolls every 6 weeks.
  1.80 released 2024-07-25; at 2026-04 it is ~21 months old,
  comfortably within distro support. No pressure to bump.
- **Edition 2024**: `Cargo.toml` is on `edition = "2021"`. 2024
  stabilized at 1.85; if someone PRs a migration, the MSRV gate
  fails correctly — editions ARE MSRV bumps.
- **`--locked`**: Same flag as other jobs. Without it, a resolver
  could silently fetch a fresher dep version requiring >1.80.
- **Deps with higher MSRV**: a workspace dep pinning
  `rust-version = "1.82"` (hypothetical) fails `cargo check` on
  1.80 cleanly. Fix: bump our MSRV, swap the dep, or pin an older
  dep version. The failure IS the feature.
- **Proc-macro MSRV**: proc-macros compile with the **host**
  toolchain (which is the MSRV toolchain in this job), not the
  target toolchain. So a proc-macro dep with `rust-version = "1.85"`
  also fails the gate — correct, and the fix path is identical to
  any other MSRV-violating dep. No proc-macro crates today.
- **`resolver = "2"`**: workspace-level, 1.80 supports it. No concern.
- **rustup toolchain shorthand `"1.80"`**: resolves to newest 1.80.z
  rustup's manifest knows about at install time (currently 1.80.1).
  That's what we want for MSRV: "minimum 1.80-family," not an exact
  SHA. Documented in the workflow comment.
- **Dual source of truth**: mitigated by `test-msrv-pin.sh` which
  runs in CI as an msrv-job step AND is committed for local use.
- **Concurrency overhead**: ~1-2 min added to CI wall-clock. Runs
  parallel to fmt/clippy/test/deny/audit. No critical path impact.

## Test strategy

Mandatory-tests rule satisfaction: `scripts/test-msrv-pin.sh` is a
committed, executable test that runs in CI on every PR, catching any
drift between the manifest's `rust-version` and the workflow's
pinned toolchain. That's the permanent gate — not "trust the CI
step." Ordinary CI-gate framing covers the rest:

1. **Existing jobs stay green** — fmt / clippy / test / deny / audit
   all still pass.
2. **New `msrv` job green on this PR** — proves 1.80 pin resolves
   and workspace compiles on 1.80.
3. **`scripts/test-msrv-pin.sh` runs green on this PR** — asserts
   the manifest and workflow agree on "1.80".
4. **Drift-detection audit** (one-off, verbatim in PR body):
   locally bump `rust-version = "1.85"` in `Cargo.toml`, run
   `bash scripts/test-msrv-pin.sh`, verify it exits 1 with the
   "msrv drift detected" error. Revert.
5. **MSRV-violation audit** (one-off, verbatim in PR body):
   locally add a function using `Option::is_none_or` (stable 1.82)
   to `crates/mango/src/lib.rs`. On the stable toolchain it
   compiles fine; on `cargo +1.80 check` it fails with a clear
   "method not found" (E0599). Revert. This fixture is chosen
   because (a) it's stable (not nightly), (b) it's one release past
   MSRV (tight gate), (c) the failure mode on 1.80 is unambiguous.
6. **Clippy `incompatible_msrv` audit** (one-off, verbatim in PR
   body): same `Option::is_none_or` function, but run `cargo clippy
--workspace --all-targets --locked -- -D warnings` on stable. With
   `incompatible_msrv = "deny"` active, clippy should also fail
   with a "msrv is 1.80, is_none_or stable in 1.82" complaint.
   Proves the clippy-side gate is live too. Revert.

## Rollback

Single squash commit. Revert → `msrv` job disappears, clippy
config disappears, script file disappears. MSRV is back to
"declaration only" in Cargo.toml. Zero runtime impact.

## Out of scope

- **`cargo-msrv` binary-search discovery** — strictly additive
  follow-up; defer.
- **Bumping MSRV past 1.80** — explicitly deferred. Separate PR,
  changelog entry, downstream-impact note.
- **Matrix MSRV on macOS / Windows** — single Linux target, matching
  cargo-deny's `[graph] targets`. Phase 6+ concern.
- **Dependabot for the MSRV pin** — MSRV is a human decision
  (contract bump), not automation-managed. Intentional.
- **ROADMAP checkbox flip** — separate commit to main per workflow.

## Risks

- **False-positive on a dep bump**: a dep's point release silently
  raises its own MSRV, failing our gate. Fix: pin an older version,
  swap the dep, or bump our MSRV deliberately. The failure IS the
  signal.
- **1.80 bitrot**: older toolchains see less testing. Review MSRV
  bump ~every 6-12 months or when a must-have API lands.
- **Rust-toolchain action churn**: `dtolnay/rust-toolchain` is
  small and stable; SHA pin kept current via (future) Dependabot.
- **Proc-macro compilation on the MSRV toolchain**: proc-macros
  compile with the HOST toolchain. In the `msrv` job, the host
  toolchain IS 1.80. A proc-macro dep with `rust-version = "1.85"`
  fails here — correctly. No proc-macro deps today.
- **`yq` availability on the runner**: `ubuntu-24.04` ships with
  Python `yq`. If upstream removes it, `pip install yq` in a
  pre-step is the fallback.
