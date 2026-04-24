# PR-1: `crates/mango-storage` skeleton (ROADMAP:815)

## Goal

Ship the scaffolding for `crates/mango-storage` with its two load-bearing
dependencies declared but _no trait or impl code yet_:

- `redb` (KV storage engine per ADR 0002)
- `raft-engine` pinned to the `humancto/raft-engine` fork at
  SHA `e1d738d9ad1c1fc4f5b21c8c73bf605b5696f535`, with
  `default-features = false` and `features = ["internals", "scripting"]` —
  the fork removes `lz4-sys` from the build graph per ADR 0002 §W5's
  build-time mitigation.

Line 816 (trait definition) is a separate item. This PR closes line 815
only. The Cargo.toml dep entries prove the fork integrates cleanly end
to end (deny.toml + cargo-vet + cargo-geiger + workspace build); the
lib.rs is a placeholder matching the shape of `crates/mango/src/lib.rs`.

## Non-goals

- No `Backend` / `RaftLogStore` trait definition (that's ROADMAP:816, its own PR).
- No impl against redb, no impl against raft-engine.
- No benches, no property tests, no differential harness (those are
  ROADMAP:819–829, each their own PR).
- No CI wiring changes beyond what the workspace already picks up by
  virtue of the new crate being a workspace member.
- No `#![allow(unsafe_code)]` at the crate root and **no** enrollment in
  `workspace.metadata.mango.miri`. The skeleton has zero unsafe. Impl
  PRs (ROADMAP:817, :818) will enroll as part of their own diff.

## Scope — exact file list

1. **`Cargo.toml`** (workspace root):
   - Add `"crates/mango-storage"` to `[workspace].members`.
   - Add `redb = "4.1"` to `[workspace.dependencies]`.
   - Add the fork-pinned `raft-engine` entry to `[workspace.dependencies]`
     with a block comment pointing at ADR 0002 §W5 and the fork-tracking
     file. Multi-line shape to stay under 100 columns (match `subtle`
     at `Cargo.toml:91-93` as precedent).

2. **`deny.toml`**:
   - Add `"https://github.com/humancto/raft-engine"` to `allow-git` with
     a justification pointing at ADR 0002 §W5 and the retirement trigger
     (upstream PR #397 merging → drop this line).

3. **`supply-chain/config.toml`**:
   - Add `[policy.mango-storage]` stanza matching the shape of
     `[policy.mango]` / `[policy.mango-proto]` (lines 16–34): first-party
     crate, `audit-as-crates-io = false`, `criteria = "safe-to-deploy"`,
     boilerplate `notes` referencing CODEOWNERS + PR review.
   - Add `[[exemptions.<crate>]]` entries for every transitive dep that
     `cargo vet check` flags after the new deps resolve. Every exemption
     note MUST carry `review-by: 2026-10-23` (180 days out, matching the
     house convention — every existing exemption uses this date). For
     `raft-engine` itself the exemption keys on `(crate_name,
crate_version)` — the fork and upstream both ship
     `package.version = "0.4.2"` (verify against fork `Cargo.toml` HEAD
     before committing), so one `[[exemptions.raft-engine]] version = "0.4.2"`
     entry covers both while the fork is active and after retirement,
     exactly as documented in `.planning/fork-raft-engine-lz4-verification.md`
     §"Supply-chain audit posture."
   - **Fallback if cargo-vet rejects the git source outright** (not just
     "needs exemption"): add `audit-as-crates-io = true` to a dedicated
     `[policy.raft-engine]` stanza, which makes vet treat the git source
     as if it were the crates.io tarball at the matching version. Try
     plain exemption first; escalate only if vet refuses.

4. **`unsafe-baseline.json`**:
   - Run `bash scripts/geiger-update-baseline.sh` with the new crate in
     place and commit the resulting per-crate `"mango-storage": {...}`
     entry. Skeleton has zero unsafe so all counts will be 0; the entry
     keeps the baseline a source of truth.

5. **`crates/mango-storage/Cargo.toml`** (new):
   - `[package]` block with `name = "mango-storage"`, `publish = false`
     (matches `mango-proto` / `mango-loom-demo` — pre-stable crate that
     depends on a git-pinned fork that can't be published to crates.io
     anyway), `description = "Mango storage backend (Phase 1 skeleton; Backend + RaftLogStore traits land per ROADMAP.md)."`,
     and the workspace-inherited version/edition/rust-version/license/
     repository/authors fields.
   - `[dependencies]`:
     ```toml
     redb.workspace = true
     raft-engine.workspace = true
     ```
     Consumer side uses `.workspace = true` **without** `default-features`
     or `features` override — the workspace entry's full shape (including
     `default-features = false, features = ["internals", "scripting"]`)
     inherits verbatim. Writing `raft-engine = { workspace = true, features = [...] }`
     here would **add** to the workspace feature set, which is exactly the
     footgun we want to avoid — an accidental re-enable of
     `lz4-compression` would re-introduce the C dep.
   - `[lints] workspace = true` (same one-liner as `crates/mango/Cargo.toml:11-12`).

6. **`crates/mango-storage/src/lib.rs`** (new):
   - Crate-level doc comment shape (matches `crates/mango/src/lib.rs:1-4`):
     ```rust
     //! mango-storage — the storage backend crate for mango.
     //!
     //! This crate is currently a placeholder skeleton. `Backend` and
     //! `RaftLogStore` trait definitions land per ROADMAP.md (Phase 1);
     //! implementations follow in their own PRs.
     ```
   - `#![deny(missing_docs)]` at the crate root. Workspace already sets
     `unsafe_code = "forbid"`; we do **not** restate it and do **not**
     add `#![allow(unsafe_code)]` — the skeleton has no unsafe.
   - One `pub const VERSION: &str = env!("CARGO_PKG_VERSION");` with a
     doc comment matching the shape in `crates/mango/src/lib.rs:8-12`.
   - `#[cfg(test)] mod tests { ... }` with the exact `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unnecessary_literal_unwrap, clippy::arithmetic_side_effects)]`
     prologue copied verbatim from `crates/mango/src/lib.rs:16-23`
     (prophylactic — the single smoke test today doesn't trip these, but
     the next test added must not either).
   - Single test `version_matches_cargo_manifest`: `assert_eq!(VERSION, "0.1.0");`
     — literal from `Cargo.toml:96` (`[workspace.package].version = "0.1.0"`).
   - **No** watchdog smoke test here. The one in `crates/mango/src/lib.rs:39-43`
     is a single-oracle for `scripts/test-watchdog.sh`; duplicating it in
     every crate is overkill. Add a one-line comment at the top of
     `mod tests` noting "watchdog smoke lives in `mango`; not duplicated."
   - **No** README.md in the crate dir (matches `crates/mango/` /
     `crates/mango-proto/`).

## Dependency pins (load-bearing)

### `redb`

```toml
# Pure-Rust embedded key-value store. Chosen in ADR 0002.
# `"4.1.0"` in Cargo semver = `^4.1.0` = `>= 4.1.0, < 5.0.0`.
# ADR 0002 verified against 4.1.0 specifically; the pin is visible in
# the manifest for that reason. Minor/patch bumps inside the 4.x range
# arrive via Dependabot.
redb = "4.1.0"
```

### `raft-engine` (fork)

```toml
# tikv/raft-engine via the humancto/raft-engine fork.
# Fork exists to feature-gate lz4-sys (C FFI) out of the default build
# graph — see .planning/fork-raft-engine-lz4-verification.md and ADR
# 0002 §W5. `default-features = false` drops `lz4-compression`; the
# other two upstream defaults ("internals", "scripting") are re-enabled
# explicitly so the skeleton proves the no-lz4 build compiles for the
# feature set the impl PRs will actually use.
#
# Mango configs MUST set batch-compression-threshold = 0 (enforced at
# runtime by the fork's Config::sanitize when lz4-compression is off).
#
# Retirement tracked against tikv/raft-engine#397. When that merges:
# swap git URL to tikv/raft-engine at the merged SHA (steps in
# .planning/fork-raft-engine-lz4-verification.md §"Retirement plan").
# Dependabot does NOT auto-bump git rev pins; retirement is a manual
# action.
raft-engine = { git = "https://github.com/humancto/raft-engine", rev = "e1d738d9ad1c1fc4f5b21c8c73bf605b5696f535", default-features = false, features = [
    "internals",
    "scripting",
] }
```

## Edge cases and risks

1. **`cargo vet` friction on first use of both crates.**
   Neither `redb` nor `raft-engine` (nor their transitive graphs) has
   exemptions yet. Plan: run `cargo vet check` locally, enumerate every
   flagged crate, add `[[exemptions.<crate>]] version = "..." criteria = "safe-to-deploy" notes = "review-by: 2026-10-23 — <source> transitive, audit pending"`.
   This is the single largest review-surface expansion in the PR; the
   reviewer will scan the exemption list for sanity but the gate is that
   `cargo vet` goes green, not that each crate is justified individually.

2. **cargo-vet exemption keying for `raft-engine` itself.**
   `cargo vet` exemptions key on `(crate_name, crate_version)` — git
   origin and SHA are not part of the exemption identity. The fork
   ships `package.version = "0.4.2"` (same as upstream), so a single
   `[[exemptions.raft-engine]] version = "0.4.2"` entry covers both the
   active fork and the post-retirement upstream. Patch-version bumps
   break this matching and need a new `version = "0.4.3"` line. Watch
   on rebase. This is all already spelled out in
   `.planning/fork-raft-engine-lz4-verification.md` §"Supply-chain audit
   posture" — the plan just reuses that policy.

3. **`cargo-geiger` baseline for storage deps (ROADMAP:823 precursor).**
   Line 823 is a future gate: "redb 4.1.0 = 37 unsafe tokens; raft-engine
   master @ pinned SHA = 49 tokens. Either +10 over baseline trips CI."
   We do **not** wire that gate in this PR. We **do** run
   `cargo geiger --workspace` locally and record totals in the PR
   description so line 823's PR has a starting point. `unsafe-baseline.json`
   only tracks first-party totals; transitive counts live in the PR body.

4. **`deny.toml` `[sources]` gate.**
   `allow-git = []` today → `cargo deny check sources` fails immediately
   on any git dep. Adding `"https://github.com/humancto/raft-engine"` to
   the allowlist is mandatory and lands in the same commit as the
   workspace Cargo.toml edit.

5. **`[bans] multiple-versions = "deny"` + protobuf dedup.**
   `raft-engine` uses `protobuf` / `raft-proto` internally; `mango-proto`
   already uses `prost = "0.13"`. These are different crates (`rust-protobuf`
   vs `prost`), so no direct version collision. However, `cargo tree --workspace`
   run as part of PR validation will surface any transitive dupes. If
   something duplicates, the fix is a scoped `skip` entry in `deny.toml`
   `[bans]` with a tracking issue link — in this PR or a fast-follow,
   whichever the diff ends up being.

6. **Feature list accuracy on `raft-engine`.**
   Upstream fork's `default = ["internals", "scripting", "lz4-compression"]`.
   Verify against the fork's `Cargo.toml` at SHA `e1d738d` before
   committing. If the fork's `default = [...]` ever gains a new entry
   (e.g., on rebase), our `default-features = false, features = ["internals", "scripting"]`
   silently drops the new feature. Accept that and reverify on every fork
   rebase — this is the tradeoff for the explicit-allowlist feature
   approach, documented in the fork-tracking file.

7. **Workspace lint friction on an empty skeleton.**
   - `unreachable_pub = "warn"` — `pub const VERSION` is exported,
     re-exported from nothing, so fine.
   - `incompatible_msrv = "deny"` — both `redb 4.1.0` and the fork's
     transitive graph compile on Rust 1.89. Verified by the fork's CI
     matrix (`redb` publishes its MSRV; fork inherits upstream's).
   - `disallowed_types` — `std::sync::{Mutex, RwLock}`, etc. The skeleton
     doesn't name any of these. Impl PRs will wrap via `parking_lot`.
   - `#[non_exhaustive]` policy — skeleton has no `pub enum`, so no
     annotation needed yet.

8. **MSRV check at `cargo check @ 1.89`.**
   Both deps need to resolve on workspace MSRV 1.89. Blocker if not.
   Verified for redb 4.1.0; verified for the fork by its own CI. If a
   transitive dep bumps its MSRV above 1.89 between now and merge, that
   trips the `.github/workflows/msrv.yml` gate and we pin the offender
   down.

9. **Dependabot does not auto-bump git rev pins.**
   Stated upfront so nobody is surprised when no PR appears after
   upstream #397 merges. Retirement is manual per
   `.planning/fork-raft-engine-lz4-verification.md` §"Retirement plan."

## Test strategy

1. `cargo check --workspace` — proves the skeleton compiles and the
   fork git dep resolves.
2. `cargo check --workspace --all-features` — same with any features.
3. `! cargo tree -p mango-storage --edges no-dev -i lz4-sys` — inverse
   tree check exits nonzero iff `lz4-sys` is not in the graph, which is
   the assertion we want. Clean PASS/FAIL without relying on `grep`'s
   empty-output convention.
4. `cargo tree -p mango-storage --edges no-dev -i redb` and
   `cargo tree -p mango-storage --edges no-dev -i raft-engine` — both
   resolve (exit 0).
5. `cargo nextest run -p mango-storage` — runs the one version smoke test.
6. `cargo clippy --workspace --all-targets -- -D warnings` — clean.
7. `cargo fmt --check` — clean.
8. `cargo deny check sources` — green (the new `allow-git` entry covers
   the fork URL).
9. `cargo deny check bans` — green (no new multi-version violations).
10. `cargo vet check` — green (exemptions cover every transitive).
11. `bash scripts/geiger-check.sh` — green (first-party baseline unchanged;
    `mango-storage` added with all-zero counts).
12. `cargo run -q -p xtask-vet-ttl` — green (every new exemption note
    carries `review-by: 2026-10-23`).

All of these run on CI via the existing workflows (`ci.yml`, `vet.yml`,
`geiger.yml`, `msrv.yml`, `public-api.yml`). The PR just has to be green.

## Rollback plan

Revert the PR with `git revert`. Nothing in this PR depends on
migrations, state, or external systems — it's additive scaffolding and
the ADR/fork record PR (#48) already landed independently.

If the fork turns out to be unusable at consumption time (cargo-vet
blocks outright even with `audit-as-crates-io = true`, or the
no-default-features build breaks in a way the fork's CI didn't catch),
escape hatch is to temporarily accept the C dep (Option A from ADR
0002 §W5) by removing `default-features = false` — but that regresses
the pure-Rust north-star and must be explicitly documented as a
regression in ROADMAP:815's text. This should be caught by steps 3 and
8 of the test strategy above, not discovered post-merge.

## Commit plan

**Single atomic commit.** The PR is small enough that three separate
commits would only aid bisect at the cost of non-compiling intermediate
states (e.g., declaring `raft-engine.workspace = true` in a consumer
crate before the workspace entry exists fails to build). Prior art:
`mango-proto`'s landing PR was a single commit followed by fast-follow
cleanups. Match that.

Commit message shape:

```
feat(storage): add mango-storage skeleton with redb + raft-engine fork deps

Adds the Phase 1 storage crate skeleton per ROADMAP:815. This is
scaffolding only — no trait definitions yet (those land in ROADMAP:816's
own PR), no implementations.

- crates/mango-storage skeleton: publish = false, deny(missing_docs),
  single VERSION-matches-manifest smoke test
- Cargo.toml: add redb = "4.1.0" and raft-engine git-pinned to the
  humancto/raft-engine fork at e1d738d, default-features = false,
  features = ["internals", "scripting"]
- deny.toml: allowlist the fork's git URL (retires when
  tikv/raft-engine#397 merges per .planning/fork-raft-engine-lz4-verification.md)
- supply-chain/config.toml: [policy.mango-storage] first-party stanza +
  exemptions for the new transitive deps
- unsafe-baseline.json: regenerated via scripts/geiger-update-baseline.sh

Closes ROADMAP:815. Next item: ROADMAP:816 (Backend + RaftLogStore
trait definition).
```

## File list (final)

- `Cargo.toml` (workspace root): 3 edits (member + redb + raft-engine)
- `crates/mango-storage/Cargo.toml` (new)
- `crates/mango-storage/src/lib.rs` (new)
- `deny.toml`: 1 edit (`allow-git`)
- `supply-chain/config.toml`: 1 policy stanza + N exemption stanzas
- `unsafe-baseline.json`: 1 new per-crate entry

## Out of scope, explicitly

- Trait definitions (ROADMAP:816)
- Trait implementations (ROADMAP:817, :818)
- Differential bbolt oracle harness (ROADMAP:819)
- Chaos / crash-recovery / disk-full / engine-swap tests (ROADMAP:820–827)
- Benchmarks (ROADMAP:828, :829)
- CI gate wiring for cargo-geiger's +10-over-baseline rule on storage
  deps (ROADMAP:823)
- `lz4_flex` as a dep of `mango-raft` (Phase 1, but for `mango-raft`, not here)
- Dependabot rules for git rev bumps (does not exist; fork retirement
  is manual)

## PR description — must include

For the reviewer's artifact-of-record:

- `cargo geiger --workspace` totals output (not just the first-party
  numbers — the transitive ones, which seed ROADMAP:823's gate).
- `cargo tree -p mango-storage --edges no-dev -i lz4-sys` output (empty,
  proving the build-time pure-Rust north-star is intact).
- `cargo tree --workspace --duplicates` output (proves `[bans] multiple-versions = "deny"` is not tripped).
- Pointer to the fork-tracking doc and ADR 0002 §W5 for reviewer context.
