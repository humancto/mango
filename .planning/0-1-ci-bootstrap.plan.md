# Plan: bootstrap GitHub Actions CI

Roadmap item: Phase 0 — "Set up CI (GitHub Actions): `cargo fmt --check`,
`cargo clippy -D warnings`, `cargo test --workspace`, on push and PR"

Status: **revised after rust-expert review** (verdict: REVISE → all five
revise-level items applied below; nits 6, 7, 9, 10 also folded in; nits
1, 2, 3, 5, 8 deferred or accepted as documented).

## Goal

Every push to `main` and every PR runs format, lint, and test checks. A
red CI run blocks merge. CI completes in under 5 minutes on a cold cache,
under 90 seconds on a warm cache. CI builds are reproducible (`--locked`
everywhere) and supply-chain-hardened (third-party actions SHA-pinned,
minimal token permissions).

## Approach

### Preconditions

- `Cargo.lock` **must** be committed (already is, from the bootstrap
  commit). `.gitignore` does not list `Cargo.lock` (verified — it lists
  `Cargo.lock.bak` only). `--locked` in CI is now meaningful.

### Workflow shape

Single workflow `.github/workflows/ci.yml` with three parallel jobs:
`fmt`, `clippy`, `test`.

Workflow-level config:

```yaml
name: ci
on:
  push:
    branches: [main]
  pull_request:

permissions:
  contents: read

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: ${{ github.ref != 'refs/heads/main' }}

env:
  CARGO_TERM_COLOR: always
  CARGO_INCREMENTAL: "0"
  CARGO_NET_GIT_FETCH_WITH_CLI: "true"
```

Notes:
- **No `RUSTFLAGS: -D warnings`** — that nukes warnings in dependency
  builds too and bakes a chronic-pain footgun into the foundation.
  Workspace-own warnings are denied via clippy's `-- -D warnings` only,
  which respects `--cap-lints=warn` on deps.
- `cancel-in-progress` is scoped off `main` so back-to-back merges don't
  cancel each other's CI run (which would leave required-checks
  unsatisfied).
- `permissions: contents: read` is the minimum needed; widen per-job
  later if/when a job needs to write.
- `CARGO_NET_GIT_FETCH_WITH_CLI=true` is a cheap preventative for the
  flaky libgit2 fetch path; matters once Phase 5 may pull a git dep.

### Jobs

All three jobs run on `ubuntu-24.04` and have `timeout-minutes: 15`.

**Common steps** (every job):
1. `actions/checkout@v4` (SHA-pinned).
2. `dtolnay/rust-toolchain@stable` (SHA-pinned, action ref). The
   *toolchain* still floats on `stable`; that is intentional. We pin
   the *action* because GitHub recommends SHA-pinning third-party
   actions for supply-chain hygiene, but do not freeze the toolchain
   version (no MSRV enforcement until a dedicated MSRV job lands as a
   roadmap follow-up — see "Out of scope").
3. `Swatinem/rust-cache@v2` (SHA-pinned) with **distinct `prefix-key`
   per job** so clippy and test don't fight over the same `target/`
   cache (clippy doesn't fully codegen; test does).

**Job 1: `fmt`**
- Components: `rustfmt`.
- No cache (formatter only; cache is wasted overhead here).
- `cargo fmt --all -- --check`.

**Job 2: `clippy`**
- Components: `clippy`, `rustfmt` (rustfmt not strictly needed but the
  toolchain action installs both for free).
- `cache prefix-key: v0-clippy`.
- `cargo fetch --locked` (fail-fast on registry / network errors;
  separates them from compile errors in CI logs).
- `cargo clippy --workspace --all-targets --locked -- -D warnings`.
- **No `--all-features`** — defer until features actually exist (Phase 6+);
  add a `cargo hack --feature-powerset` job at that point. Avoids
  pre-baking a wrong default.

**Job 3: `test`**
- `cache prefix-key: v0-test`.
- `cargo fetch --locked`.
- `cargo test --workspace --all-targets --locked --no-fail-fast`.
- `--no-fail-fast` here means "report all failing test binaries in one
  CI run", **not** the matrix-strategy `fail-fast: false` (no matrix
  exists). When a matrix lands in Phase 12, that knob will be set
  separately at the `strategy:` level.
- `--all-targets` so bench/example *compilation* breakage is caught.

### Action SHA-pinning

Pinned at write time using the highest available release on the action's
`v*` ref. Renovate / Dependabot can bump these later; not in scope for
this PR. Specific SHAs to be looked up at implementation time:
- `actions/checkout@v4` → SHA of `v4` tip.
- `dtolnay/rust-toolchain@stable` → SHA of `stable` branch tip.
- `Swatinem/rust-cache@v2` → SHA of `v2` tip.

If looking up SHAs at implementation time becomes a yak shave, fall back
to tag refs (`@v4`, `@stable`, `@v2`) and file a follow-up roadmap item
to SHA-pin them. Document whichever choice is made in the workflow
itself with a comment.

## Files to touch

- `.github/workflows/ci.yml` — new file, the entire workflow.
- `README.md` — add CI status badge near the top:

  ```markdown
  [![ci](https://github.com/humancto/mango/actions/workflows/ci.yml/badge.svg)](https://github.com/humancto/mango/actions/workflows/ci.yml)
  ```

That's it. No code changes, no new crates.

## Edge cases

- **Empty workspace later** — if a future commit drops the placeholder
  crate before adding real ones, `cargo test --workspace` would fail
  with "no members". Mitigated by always keeping at least one
  buildable crate.
- **Clippy pedantic friction** — `Cargo.toml` enables `clippy::pedantic`
  at warn level. Predictable pain points (per rust-expert review):
  `must_use_candidate`, `needless_pass_by_value`,
  `cast_possible_truncation`, `cast_sign_loss`, `similar_names`. Policy
  for handling: any pedantic allow added in a feature PR is a one-line
  `[workspace.lints.clippy]` change with a `# justification: ...`
  comment in the same PR — no separate PR required. This makes the
  friction predictable instead of surprising. Documented here so future
  PR authors know the policy.
- **Cache poisoning** — `Swatinem/rust-cache@v2` keys on `Cargo.lock`
  hash + rustc version, so a toolchain bump or a dep update naturally
  invalidates the cache. No manual cache-bust strings needed.
- **MSRV drift** — `rust-version = "1.80"` is set in
  `[workspace.package]`. CI uses `stable`. MSRV is not actively
  enforced in this PR; tracked as a roadmap follow-up.
- **Path filters vs required checks** — *not* adding `paths-ignore` in
  this PR. The robust pattern (skipped-then-required gate job) is more
  config than a Phase-0 PR should carry; revisit when README-only
  changes become frequent. Documented here so the next person who
  reaches for `paths-ignore` knows the trap.

## Test strategy

- Open the PR; observe CI runs and all three jobs go green.
- Locally, run the same commands the workflow runs and confirm parity:
  ```
  cargo fmt --all -- --check
  cargo fetch --locked
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo test  --workspace --all-targets --locked --no-fail-fast
  ```
- Push a deliberately broken commit to a throwaway branch (extra
  whitespace) to confirm `fmt` fails red, then revert. (Optional
  pre-merge sanity check; not part of the PR.)

## Rollback

The workflow is additive and pure-config. Roll back by reverting the
single commit that introduced `.github/workflows/ci.yml` and the README
badge.

## Out of scope (explicit, do not do in this PR)

- `cargo-deny` job — its own roadmap item later in Phase 0.
- Multi-OS / multi-arch matrix — Phase 12.
- Code coverage upload — not on the roadmap.
- Caching `~/.rustup` — `dtolnay/rust-toolchain` re-uses the runner's
  preinstalled toolchain when versions match; not worth optimizing.
- Required-status-checks branch protection — repo admin setting, not a
  code change. Note for the user separately.
- Adding a dedicated MSRV job — file as a roadmap follow-up.
- `cargo doc --no-deps -D warnings` job — cheap and useful but adding a
  fourth job in this PR is scope creep. File as a roadmap follow-up.
- Path filters (`paths-ignore`) — see Edge Cases above; defer.

## Disagreements with reviewer (and why)

None substantive. All five REVISE-level items adopted verbatim. Of the
nits:
- Pre-emptively allowing the predictable pedantic lints (Risk #1 in the
  review) — **not doing in this PR**. Reason: don't pollute lint config
  with allows for lints that haven't fired yet; do it in the PR that
  causes them to fire. The policy is documented above so the friction
  is predictable.
- `cargo doc` job (Missing #8) — **deferred** as a roadmap follow-up,
  see Out of Scope. Reason: scope, one PR one item.
