# Public API surface policy

Mango commits to **zero silent public API changes** on every published
`crates/mango-*` crate. Today no crate is published; when Phase 6 opens
(first stable public API), every surface-level addition, removal, or
rename will be merge-visible and, in Phase 6+, merge-blocking.

The machinery behind this policy:

- **`cargo-public-api`** — this doc's subject. Runs in
  [`.github/workflows/public-api.yml`](../.github/workflows/public-api.yml)
  on every PR that touches a crate's source or manifest, printing the
  **syntactic diff** of every publishable workspace member's public
  surface against the PR base.
- **`cargo-semver-checks`** (already shipped) — complementary semantic
  lint. See [`docs/semver-policy.md`](semver-policy.md).

The two tools are complementary, not redundant. `cargo-public-api`
answers "what symbols changed?"; `cargo-semver-checks` answers "is
that change a semver violation?".

## Mode: advisory (pre-Phase-6), gating (Phase 6+)

The gate is **advisory today**. A non-empty public-API diff on a PR:

- Shows up as a `::warning::` annotation in the PR's **Files** view
  (not buried in the Actions log), and
- Does **not** block merge.

Phase 6 flips both: the annotation becomes a hard failure, and merge
is blocked until the PR either undoes the surface change or documents
it as an intentional break with a major version bump.

Why advisory-now instead of waiting:

1. **Exercise the machinery before it's load-bearing.** Contributors
   hitting the gate for the first time on a Phase-6 PR have a worse
   experience if the gate has never run before.
2. **Signal is still useful today.** A refactor that silently makes
   a type `pub` when it was meant to be crate-private is worth
   flagging even pre-publish — it's exactly the kind of surface-bleed
   that's hard to spot without tooling.
3. **Install + cache + pin is the same work.** Flipping to gating is
   a two-line PR (see §"Flipping to gating" below).

## What the gate runs

On every PR that touches `crates/**/src/**`, `crates/**/Cargo.toml`,
`Cargo.toml` (workspace root), `Cargo.lock`, the workflow itself, the
harness script, or this policy doc, the workflow:

1. Installs the pinned `cargo-public-api` binary
   (`CARGO_PUBLIC_API_VERSION`, cached by `(version, os, arch)`).
2. Installs two rust toolchains: `stable` (active) and the pinned
   nightly (`PUBLIC_API_NIGHTLY`, installed-but-not-default).
   `cargo-public-api` consumes rustdoc JSON, which is nightly-only;
   it auto-invokes the installed nightly toolchain.
3. Runs `scripts/public-api-scripts-test.sh` — a structural self-test
   of the workflow file and the publishable-member jq filter (see
   §"Harness").
4. Resolves a baseline SHA from the event (PR base / merge_group base
   / workflow_dispatch fallback to `git merge-base HEAD origin/main`).
5. Enumerates publishable workspace members via `cargo metadata` +
   `jq` (filtering out `publish = false` crates and non-path deps).
6. For each publishable member, runs
`cargo public-api --package <pkg> --omit … diff --deny all
<base>..HEAD`. The `--deny all` flag is **mandatory** — without it,
   `cargo-public-api` returns exit 0 on a non-empty diff and the
   `continue-on-error`/`::warning::` advisory wiring never fires.
7. If any crate reports a non-empty diff, emits a loud `::warning::`
   annotation pointing to this doc.

The `merge_group` trigger has no `paths:` filter — GitHub Actions
doesn't support one on merge-queue events. Matches the
`semver-checks` and `vet` precedents.

### The `--omit` flag

The workflow passes
`--omit blanket-impls,auto-trait-impls,auto-derived-impls`
instead of the `--simplified` shortcut. Same behavior today, but
explicit is:

- **Auditable.** A reviewer can see which categories are suppressed
  without checking the tool's docs.
- **Decomposable.** In Phase 6 we may want `auto-derived-impls` diffs
  visible (losing `Debug` or `Clone` on a published type matters).
  Dropping one of the three is a one-line flag edit; splitting
  `--simplified` apart is not.

## Responding to a warning

If `cargo-public-api` flags a non-empty diff on your PR:

1. **Read the tool output.** It prints added / removed / changed
   symbols grouped by crate and item kind.
2. **Decide whether the change is intentional.**
   - **Unintentional** (common): a refactor accidentally exposed a
     type, or a `pub(crate)` item regressed to `pub`. Fix the
     visibility.
   - **Intentional**: document it in the PR description. Pre-Phase-6
     there's no approval surface; proceed. Phase 6+, the review bar
     rises to "must be noted in the release notes and a major-version
     bump is scheduled."
3. **Push a fix** (if unintentional) and the annotation goes away.

## Running locally

```bash
git fetch origin main
cargo install --locked cargo-public-api --version <pin>
rustup install <nightly-pin>

# Run from repo root. --manifest-path is explicit as a guard against
# accidentally running from a crate subdirectory (where --package
# would silently resolve to a different workspace).
cargo public-api \
    --manifest-path "$(git rev-parse --show-toplevel)/Cargo.toml" \
    --package mango \
    --omit blanket-impls,auto-trait-impls,auto-derived-impls \
    diff \
    --deny all \
    origin/main..HEAD
```

Use the pins from
[`.github/workflows/public-api.yml`](../.github/workflows/public-api.yml)
— the `CARGO_PUBLIC_API_VERSION` and `PUBLIC_API_NIGHTLY` env vars.

The `git fetch` is load-bearing — a stale local `origin/main`
produces spurious deltas.

**Toolchain channel**: unlike `cargo-semver-checks` (stable-only),
`cargo-public-api` REQUIRES a nightly rustdoc. Running locally on
stable-only will fail with a rustdoc-JSON-format error. The tool
auto-invokes nightly if installed; you don't need to
`rustup default nightly`.

### Synthetic-change smoke

To confirm the gate flags real changes, on a scratch branch:

```bash
# Add a public item that wasn't there before.
echo '' >> crates/mango/src/lib.rs
echo '/// Smoke test — delete before committing.' >> crates/mango/src/lib.rs
echo 'pub fn smoke_change() {}' >> crates/mango/src/lib.rs

cargo public-api \
    --manifest-path "$(git rev-parse --show-toplevel)/Cargo.toml" \
    --package mango \
    --omit blanket-impls,auto-trait-impls,auto-derived-impls \
    diff --deny all origin/main..HEAD
# Expect: non-empty diff, exit code 1, printed line
# "+pub fn mango::smoke_change()".

git checkout -- crates/mango/src/lib.rs
```

## Bumping the pin

`cargo-public-api` pins **two** versions that must move together:

1. **Tool version** — `CARGO_PUBLIC_API_VERSION` in the workflow.
2. **Nightly toolchain** — `PUBLIC_API_NIGHTLY` in the workflow.

Upstream's compatibility matrix (linked from the
[cargo-public-api README](https://github.com/cargo-public-api/cargo-public-api))
lists the minimum nightly each tool version requires. When bumping:

1. Pick the new tool version. Read its compat-matrix entry.
2. Pick a recent nightly **at or after** the matrix-listed minimum.
   Fixed date (not floating `nightly`) for determinism.
3. Update both env vars in
   [`.github/workflows/public-api.yml`](../.github/workflows/public-api.yml).
4. Locally:
   ```bash
   cargo install --locked cargo-public-api --version <new> --force
   rustup install <new-nightly>
   # Re-run the local reproducer command above.
   bash scripts/public-api-scripts-test.sh
   ```
5. Open a PR. The self-test asserts both pins are well-formed and
   present.

**Skip either pin and the self-test fails** — bumping the tool
without bumping the nightly is the most common regression mode (new
tool versions require newer nightly rustdoc JSON formats).

## Flipping to gating (Phase 6 procedure)

When the first `crates/mango-*` crate gains a stable public API
(Phase 6), flip the gate in a dedicated PR:

1. In
   [`.github/workflows/public-api.yml`](../.github/workflows/public-api.yml):
   - Change the sentinel comment from
     `# PUBLIC-API-MODE: advisory` to `# PUBLIC-API-MODE: gating`.
   - Change `continue-on-error: true` to `continue-on-error: false`
     on the same step.
2. `--deny all` is **already** in the invocation from day one — do
   NOT re-add it. (This is what makes the flip two lines instead of
   three.)
3. Update this doc's first section to read "gating" throughout.

The harness asserts the sentinel, `continue-on-error`, and `--deny`
stay in sync. Flipping one without the others fails CI — guard
against "sentinel says gating but gate still doesn't block" drift.

## Verifying the gate works

To confirm the workflow is wired up correctly end-to-end (do this
once after shipping, and after any pin bump):

1. On a scratch branch, introduce a deliberate surface change to
   `crates/mango/src/lib.rs` — e.g., `pub fn new_item() {}`.
2. Open a PR against `main` and wait for the `public-api` workflow
   to complete.
3. Confirm the PR's **Files** view shows the `::warning::`
   annotation (pre-Phase-6) or a red X on the check (Phase 6+).
4. **Close the scratch PR without merging. Delete the branch.** Do
   NOT squash-merge — the entire point is to exercise the gate, not
   to ship a noise change.

## Known caveats

- **Workspace strict lints**: `unsafe_code = "forbid"`,
  `arithmetic_side_effects = deny`, and similar workspace-deny
  lints apply when the tool rustdoc-compiles a _baseline_ commit
  via git-worktree. If the baseline fails to compile under
  tightened lints (possible after a lint bump), the job fails for
  reasons unrelated to the API surface.
  `cargo-public-api` inherits `cargo-semver-checks`'s workaround:
  `RUSTFLAGS="--cap-lints=warn"` on the step env as a last resort.
  This is cross-linked from
  [`semver-policy.md §"Known caveats"`](semver-policy.md#known-caveats)
  rather than duplicated here.
- **`publish = false` members are skipped** by the jq filter
  (`select(.publish != [])`). Today that's `mango-proto`,
  `mango-loom-demo`, and `xtask-vet-ttl`. When a member flips to
  publishable in Phase 6+, `cargo metadata` picks it up
  automatically — no workflow edit needed.
- **Zero publishable members edge case.** If `mango` flips to
  `publish = false`, the loop body never runs and the job passes.
  An empty public surface can't regress, so this is correct.
- **New crate added in this PR** (HEAD has `mango-foo`, BASE
  doesn't). `cargo public-api diff BASE..HEAD --package mango-foo`
  fails to rustdoc-compile the baseline (crate doesn't exist
  there). The workflow intersects the publishable-member set
  between BASE and HEAD: crates present only in HEAD emit a
  `::notice::` annotation ("new publishable crate — no baseline
  to diff against") and are skipped. Their diff gets reviewed by
  human reviewers, which is the right granularity for a
  new-crate PR anyway.
- **Tool error vs non-empty diff.** Even with `--deny all`, exit 1
  could mean "diff present" OR "tool crashed internally". Advisory
  mode can't tell them apart without parsing output. We accept
  this — advisory is advisory, failure surfaces both ways as a
  `::warning::`. Gating mode (Phase 6) will need a structured mode
  upstream if one ships; out of scope today.
- **Baseline build cost** compounds as the workspace grows. Today
  it's one placeholder crate; Phase 1–6 will add more. If CI
  wall-clock becomes a problem, cache rustdoc JSON per-baseline-SHA
  (same trick `semver-policy.md` earmarks for its tool). On deck,
  not implemented.
- **Cache-miss rebuild cost.** First run on a fresh runner builds
  `cargo-public-api` from source (~90s). Steady state ~30-45s cold
  compile + ~60-90s first-ever run on a fresh cache. If this
  becomes a regular pain point in Phase 6, switch install to
  `taiki-e/install-action` for the prebuilt binary. Current runner
  cache hit rate is high enough to keep the `cargo install --locked`
  precedent for now.

## Harness

[`scripts/public-api-scripts-test.sh`](../scripts/public-api-scripts-test.sh)
is the self-test for the workflow file. It asserts:

- Workflow file exists (skipped gracefully when absent, so the
  script can land in a commit before the workflow does).
- `CARGO_PUBLIC_API_VERSION` is declared as `x.y.z`.
- `PUBLIC_API_NIGHTLY` is declared as `nightly-YYYY-MM-DD`.
- Checkout step sets `fetch-depth: 0`.
- `PUBLIC-API-MODE` sentinel is present.
- **Tri-consistency**: sentinel ↔ `continue-on-error` ↔ `--deny` all
  agree. If sentinel says `advisory`, `continue-on-error` is true
  AND `--deny` is present. If `gating`, `continue-on-error` is
  false AND `--deny` is present. `--deny` is mandatory in both
  modes (without it, exit-code semantics mean the whole gate is a
  silent no-op).
- The `::warning::` annotation step exists.
- Required path filters are present.
- Both `stable` and nightly `dtolnay/rust-toolchain` uses: steps
  are present.
- The publishable-member jq filter works on a synthetic fixture:
  given three packages (one `publish: null`, one `publish: []`,
  one `publish: ["crates-io"]`), it emits exactly the two
  publishable names.
- This policy doc exists (so workflow cross-links aren't broken).

It runs as a step in the workflow and can be run locally:

```bash
bash scripts/public-api-scripts-test.sh
```

## File inventory

| File                                 | Role               | Hand-edited? |
| ------------------------------------ | ------------------ | ------------ |
| `.github/workflows/public-api.yml`   | CI gate            | yes          |
| `scripts/public-api-scripts-test.sh` | workflow self-test | yes          |
| `docs/public-api-policy.md`          | this doc           | yes          |

## Relationship to `cargo-semver-checks`

`cargo-public-api` prints a human-readable syntactic diff of a
crate's public surface. `cargo-semver-checks` **judges** whether
such a diff is a semver violation (trait-bound tightening, lost
derives, added required generics, etc.). Both are useful and both
ship as advisory-now, gating-at-Phase-6. They run as sibling
workflows because:

- Each pins a different tool (and `cargo-public-api` additionally
  pins a nightly toolchain, which `cargo-semver-checks` doesn't
  need).
- GitHub's required-check surface benefits from one status row
  per tool — reviewers see both signals independently.
- Phase 6 may stage the flips (semver-checks first, public-api
  second) depending on upstream UX.

See [`docs/semver-policy.md`](semver-policy.md) for the sibling
tool's policy.
