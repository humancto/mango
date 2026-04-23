# Semver compatibility policy

Mango commits to semantic versioning on every `pub` item in every
published `crates/mango-*` crate. Today no crate is published; when
Phase 6 opens (first stable public API), the rules here become
merge-blocking.

The machinery behind this policy:

- **`cargo-semver-checks`** — this doc's subject. Runs in
  [`.github/workflows/semver-checks.yml`](../.github/workflows/semver-checks.yml)
  on every PR that touches a crate's source or manifest.
- **`cargo-public-api`** (future, ROADMAP:802) — complementary
  surface-level diff tool. Not shipped yet.

`cargo-semver-checks` catches violations the public-API surface alone
doesn't reveal: tightening a trait bound, removing a
`#[derive(Clone)]`, adding a required generic parameter, flipping
`#[non_exhaustive]`. It compiles `cargo rustdoc` JSON for both the
working tree and a git baseline, and runs a library of lints over the
delta.

## Mode: advisory (pre-Phase-6), gating (Phase 6+)

The gate is **advisory today**. A semver violation in a PR:

- Shows up as a `::warning::` annotation in the PR's **Files** view
  (not buried in the Actions log), and
- Does **not** block merge.

Phase 6 flips both: the annotation becomes a hard failure, and
merge is blocked until the PR either fixes the violation or
documents it as an intentional break with a major version bump.

Why run advisory-now instead of waiting for Phase 6:

1. **Exercise the machinery before it's load-bearing.** Contributors
   hitting the gate for the first time on a Phase-6 PR have a worse
   experience if the gate has never run before.
2. **Signal is still useful today.** A refactor that accidentally
   makes a placeholder type non-`Clone` is worth flagging even when
   the type isn't published yet — it's exactly the kind of
   regression that's easy to introduce silently and painful to fix
   after a `1.0`.
3. **Install + cache + pin is the same work.** Flipping to blocking
   is a single-digit-line PR (see §"Flipping to blocking" below).

## What the gate runs

On every PR that touches `crates/**/src/**`, `crates/**/Cargo.toml`,
`Cargo.toml` (workspace root), `Cargo.lock`, the workflow itself,
the harness script, or this policy doc, the workflow:

1. Installs the pinned `cargo-semver-checks` binary (version in
   `CARGO_SEMVER_CHECKS_VERSION`, cached by `(version, os, arch)`).
2. Runs `scripts/semver-scripts-test.sh` — a structural self-test of
   the workflow file itself (see §"Harness").
3. Resolves a baseline SHA from the event (PR base / merge_group
   base / workflow_dispatch fallback to `git merge-base HEAD
origin/main`).
4. Runs `cargo semver-checks --workspace --baseline-rev <base>`.
5. If the check reports violations, emits a loud `::warning::`
   annotation pointing to this doc.

The `merge_group` trigger has no `paths:` filter — GitHub Actions
doesn't support one on merge-queue events. That's intentional
(merge-queue is the last line of defense).

The `Cargo.toml` (root) filter catches both `[workspace.dependencies]`
bumps (which CAN flip re-exported public surface) and
`[workspace.lints]` tweaks (which cannot). We accept the
false-positive on lint-only PRs — the check is advisory today and
cheap.

## Responding to a warning

If `cargo-semver-checks` flags a violation on your PR:

1. **Read the tool output.** Each lint includes a specific reason
   and a link to its reference page.
2. **Decide whether the break is intentional.**
   - **Unintentional** (common): a refactor slipped; fix the code.
     For example, if a type lost a `Clone` impl because an inner
     field stopped being `Clone`, either add `Clone` back to the
     field or make the outer type `Clone` via an explicit impl.
   - **Intentional** (rare, Phase 6+): the change is a deliberate
     breaking change that warrants a major-version bump. Document
     the rationale in the PR description. Pre-Phase-6, there's no
     `semver_exemptions` table yet; just proceed — the gate is
     advisory. When the table lands in Phase 6, intentional breaks
     will be listed there.
3. **Push a fix** (if unintentional) and the annotation goes away.

## Running locally

```bash
git fetch origin main
cargo install --locked cargo-semver-checks --version <pin>
cargo semver-checks --workspace --baseline-rev origin/main
```

The `git fetch` is load-bearing — a stale local `origin/main`
produces spurious deltas. Use the pin from
[`.github/workflows/semver-checks.yml`](../.github/workflows/semver-checks.yml)
(the `CARGO_SEMVER_CHECKS_VERSION` env var).

**Toolchain channel**: `cargo-semver-checks` uses `cargo rustdoc`,
which emits an unstable JSON format. The tool is pinned to a specific
format version, and CI runs on **stable**. Running locally on
**nightly** may produce different results; CI is authoritative.

## Bumping the pin

1. Update `CARGO_SEMVER_CHECKS_VERSION` in
   [`.github/workflows/semver-checks.yml`](../.github/workflows/semver-checks.yml).
2. Locally:
   ```bash
   cargo install --locked cargo-semver-checks --version <new> --force
   cargo semver-checks --workspace --baseline-rev origin/main
   bash scripts/semver-scripts-test.sh
   ```
3. Open a PR. rust-expert reviews per the normal workflow — new
   lint rules can surface legitimate regressions on the same PR that
   bumps the pin, which is fine (fix them or open a follow-up).

## Flipping to blocking (Phase 6 procedure)

When the first `crates/mango-*` crate gains a stable public API
(Phase 6), flip the gate in a dedicated PR:

1. In [`.github/workflows/semver-checks.yml`](../.github/workflows/semver-checks.yml):
   - Change the sentinel comment from
     `# SEMVER-CHECKS-MODE: advisory`
     to
     `# SEMVER-CHECKS-MODE: gating`.
   - Change `continue-on-error: true` to `continue-on-error: false`
     on the same step.
2. Update this doc's first section to read "gating" throughout.
3. If breaking changes are ever intentional, add a
   `[workspace.metadata.mango.semver_exemptions]` table to the
   workspace `Cargo.toml` with a documented entry per break. The
   harness will need a corresponding check. Design that when we get
   there, not now.

The harness asserts the sentinel and the `continue-on-error` flag
stay in sync. Flipping one without the other fails CI — this is the
guard against "sentinel says gating but gate still doesn't block"
drift.

## Verifying the gate works

To confirm the workflow is wired up correctly end-to-end (do this
once after shipping, and after any pin bump):

1. On a scratch branch, introduce a deliberate breaking change to
   `crates/mango/src/lib.rs` — e.g., remove `#[derive(Clone)]` from
   a public type, or tighten a trait bound.
2. Open a PR against `main` and wait for the `semver-checks`
   workflow to complete.
3. Confirm the PR's **Files** view shows the `::warning::`
   annotation (pre-Phase-6) or a red X on the check (Phase 6+).
4. **Close the scratch PR without merging. Delete the branch.** Do
   NOT squash-merge — the entire point is to avoid landing a break.

## Known caveats

- **Workspace strict lints**: `unsafe_code = "forbid"`,
  `arithmetic_side_effects = deny`, and similar
  workspace-deny lints apply when the tool rustdoc-compiles a
  _baseline_ commit. If a lint rejects code that passed `cargo
check` at the time the baseline commit was made (possible after a
  lint tightening), the baseline compile fails — producing a
  failure unrelated to semver. The tool mitigates this by passing
  `--cap-lints=warn` via rustdoc by default on recent versions. If
  you hit a lint failure on the baseline specifically, add
  `RUSTFLAGS="--cap-lints=warn"` to the `cargo semver-checks` step
  env as a last resort.
- **`publish = false` members are skipped** automatically by
  `--workspace`. Today that's `mango-proto`, `mango-loom-demo`, and
  `xtask-vet-ttl`. Verified on `cargo-semver-checks 0.47.0`.
- **Nothing is published**, so `--baseline-registry` is unusable;
  we rely exclusively on `--baseline-rev <sha>`.
- **Baseline build cost** compounds as the workspace grows. Today
  it's one placeholder crate; once Phase 1–6 land real crates, the
  gate will rustdoc-compile every publishable member for the
  baseline tree on every PR. If CI wall-clock becomes a problem,
  cache rustdoc JSON via `--baseline-rustdoc` and an `actions/cache`
  keyed on the baseline SHA. On deck, not implemented.

## Harness

[`scripts/semver-scripts-test.sh`](../scripts/semver-scripts-test.sh)
is the self-test for the workflow file. It asserts:

- Workflow file exists.
- `CARGO_SEMVER_CHECKS_VERSION` is declared as `x.y.z`.
- Checkout step sets `fetch-depth: 0`.
- `SEMVER-CHECKS-MODE` sentinel is present and consistent with
  `continue-on-error`.
- The `::warning::` annotation step exists.
- Required path filters are present.
- The deprecated `check-release` subcommand is NOT in use.
- This policy doc exists (so the workflow's links aren't broken).

It runs as a step in the workflow and can be run locally:

```bash
bash scripts/semver-scripts-test.sh
```

## File inventory

| File                                  | Role               | Hand-edited? |
| ------------------------------------- | ------------------ | ------------ |
| `.github/workflows/semver-checks.yml` | CI gate            | yes          |
| `scripts/semver-scripts-test.sh`      | workflow self-test | yes          |
| `docs/semver-policy.md`               | this doc           | yes          |

## Relationship to `cargo-public-api`

`cargo-public-api` (future ROADMAP:802) prints a human-readable diff
of a crate's public surface. It's great for PR-review triage but is
syntactic — it doesn't reason about trait-bound tightening or
semver implications. `cargo-semver-checks` is the tool that actually
**judges** whether a diff is a semver violation.

Both ship as advisory-now, gating-at-Phase-6. They're complementary,
not redundant.
