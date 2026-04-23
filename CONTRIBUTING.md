# Contributing to mango

Mango is a Rust ground-up port of [etcd](https://github.com/etcd-io/etcd).
The [ROADMAP](./ROADMAP.md) is the source of truth for what we build
and in what order. This document is the contributor entrypoint: it
points you at the policies, conventions, and review gates that govern
every PR, and explains how to file one.

Everything below is a pointer, not a copy. The policy docs under
[`docs/`](./docs), the roadmap, and the per-crate config files
(`rustfmt.toml`, `clippy.toml`, `deny.toml`) are the authoritative
sources. If `CONTRIBUTING.md` and any of them disagree, they win and
this doc needs fixing.

## Table of contents

0. [Start here](#start-here)
1. [Before you write code](#before-you-write-code)
2. [Branch naming](#branch-naming)
3. [Commit style](#commit-style)
4. [The test bar](#the-test-bar)
5. [The north-star bar and the reviewer's contract](#the-north-star-bar-and-the-reviewers-contract)
6. [Arithmetic-primitive policy](#arithmetic-primitive-policy)
7. [Other policies](#other-policies)
8. [Running the checks locally](#running-the-checks-locally)
9. [PR description format](#pr-description-format)

## Start here

Small PRs are welcome. Typo fix, dead-link fix, single-sentence doc
clarification, dep-version nudge, CI polish — these are **plumbing**
PRs and they are a valid, encouraged first contribution. You don't
need a Criterion bench to fix a typo. You don't need a `loom` test to
rename a variable. Section 9 spells out the plumbing declaration, and
item #1 of the [Reviewer's Contract][contract] is "honestly declared
as plumbing." Use it.

Larger PRs (new feature, perf work, new public API, anything under
`crates/`) apply the full bar. The bar is higher than most Rust
projects — mango's north star is "beats etcd on every axis we care
about," not parity. Read section 5 before opening one of those, so
there are no surprises at review.

## Before you write code

Mango follows a **roadmap-driven workflow** with four gates:

1. **Pick a roadmap item.** Find the first unchecked `- [ ]` in
   [`ROADMAP.md`](./ROADMAP.md), or the one tracking the bug / feature
   you want to fix. Don't batch multiple items into one PR unless the
   roadmap explicitly groups them.
2. **Draft a plan** under `.planning/<slug>.plan.md` naming which
   north-star axis the item moves (or declaring it plumbing), the
   files it will touch, the test strategy, and a rollback plan.
3. **Get the plan reviewed by `rust-expert`** (an adversarial plan
   reviewer). Revise until the plan clears the Reviewer's Contract.
   The verdicts you'll see: `REVISE` / `APPROVE_WITH_NITS` /
   `APPROVE`.
4. **Implement, PR, get the final diff reviewed by `rust-expert`**,
   revise, merge.

Five working rules sit alongside the gates; they are paraphrased
from [`ROADMAP.md` § Working rules][working-rules]:

- One roadmap item per PR. Small, atomic, mergeable.
- Every plan declares the north-star axis + named test + measured
  number (or honestly marks plumbing).
- `cargo nextest run --workspace` green at every commit (plus
  `cargo test --doc --workspace` for doctests, which nextest
  does not cover — see [`docs/testing.md`](./docs/testing.md)).
- Every hot-path PR ships a Criterion bench and a baseline number.
- No `TODO` / `FIXME` / `unimplemented!()` / `todo!()` on `main` —
  follow-ups become new roadmap items, not comments in code.

**Crate-inventory rule.** Before adding, replacing, or proposing an
alternative to any crate listed in the workspace
[Crate inventory][inventory], file an ADR in `.planning/adr/`
justifying the deviation. Hand-rolling a subsystem the inventory
already covers (Raft, storage engine, TLS, async runtime, gRPC, …)
is an auto-`REVISE` trigger.

[working-rules]: ./ROADMAP.md#working-rules
[inventory]: ./ROADMAP.md#crate-inventory--non-rolled-stack
[contract]: ./ROADMAP.md#reviewers-contract-the-rust-expert-agent

## Branch naming

`<type>/<slug>`, where `<type>` is one of:

- `feat` — new feature or capability
- `fix` — bug fix
- `refactor` — internal restructuring, no behavior change
- `chore` — CI, tooling, deps, formatting, release plumbing
- `docs` — doc-only change

`<slug>` is kebab-case, typically matching the `.planning/<slug>.plan.md`
filename. Never work directly on `main`.

## Commit style

Conventional commits with scope: `<type>(<scope>): <description>`.
Examples from the log: `feat(mango-proto): Phase 0 skeleton …`,
`chore(ci): add cargo-deny supply-chain gate`,
`docs(time): monotonic-clock policy`. Keep commits small and atomic —
one concern per commit. When a commit is co-produced with an
assistant, include a `Co-Authored-By` trailer.

**Direct-to-`main` exception.** After a PR merges, the roadmap
checkbox flip is committed directly to `main` with message
`chore: mark <slug> done on roadmap`. This is the **only** commit
that bypasses the PR / review flow; it is a one-line checkbox change
and has no other effect. Do **not** include the checkbox flip in
your feature PR — it lands after the squash-merge, as a separate
commit on `main`.

## The test bar

"HOPE YOU'RE ALL ADDING TESTS." The project owner's rule is
non-negotiable: every change ships with tests, or with a written
verification strategy. "Trust CI" is not a test plan.

Classification is **semantic, not file-path**. A PR falls into one
of two cases:

- **Code-PR case.** The PR touches `crates/`, **or** introduces
  semantic enforcement logic (new clippy rule with real effect, new
  CI gate that blocks merges, new lint with behavioral consequences),
  **or** contains any `unsafe` block regardless of where it lives.
  The [Definition of Done test-class list][dod] applies:
  - **Unit tests** for every public function.
  - **Property tests** (`proptest`) — the default for any data
    structure, codec, or protocol op.
  - **Integration tests** for every cross-crate boundary.
  - **Crash / recovery tests** for anything that touches disk.
  - **Concurrency tests** (`loom`) for every shared-state primitive.
  - **Fuzz targets** (`cargo fuzz`) for every parser surface.
  - **Miri** runs on every test that exercises `unsafe` or pointer
    arithmetic.
  - **Watchdog**: any test > 10× baseline duration fails CI.

  Missing an applicable class is an auto-`REVISE`.

- **Docs-or-plumbing-PR case.** Pure docs, dep bump, formatting, or
  CI change with no semantic enforcement effect. The PR description
  names a **verification strategy** — a short list of reproducible
  commands the reviewer can run to confirm the change did what it
  claimed. (Example: "Ran `./scripts/verify-contributing-refs.sh`
  locally — output attached.")

Tests are mandatory in both cases; only the form differs.

[dod]: ./ROADMAP.md#definition-of-done-every-phase

## The north-star bar and the reviewer's contract

Mango's north star, verbatim from the roadmap: **"beats etcd on
every axis we care about."** Not parity. Not "good enough." Every
plan and every PR names which axis it moves and the specific
[north-star bar][north-star] it verifies, or declares itself
plumbing.

The [Reviewer's Contract][contract] is the merge gate. `rust-expert`
classifies each PR by what it touches (plumbing / perf / correctness
/ concurrency / unsafe / security / reliability / scale / new public
API) and applies the contract items for that classification.
**Items #1, #10, #11, #12 always apply** — #1 (declared axis or
declared plumbing), #10 (CI green including clippy and `cargo-deny`),
#11 (no new `TODO` / `FIXME` / `unimplemented!()` / `todo!()`), #12
(moves an axis with evidence or is honestly plumbing). The
classification-specific items (#2–#9) add evidence requirements on
top. `APPROVE` from `rust-expert` on the final diff is the merge
gate — not a maintainer sign-off, not CI green alone. Section 9
below spells out what the PR description needs to include per
classification.

[north-star]: ./ROADMAP.md#north-star-non-negotiable

## Arithmetic-primitive policy

One-sentence TL;DR: **protocol counters use `checked_*`; time
budgets use `Duration::saturating_*`; hashes and ring indices use
`wrapping_*`; `usize` slice math uses `checked_*` or carries a
`// BOUND:` comment with a narrow `#[allow]`.** The lint
`clippy::arithmetic_side_effects` is denied workspace-wide and will
fail the PR at clippy time if you use raw `+` / `-` / `*` on
anything but the test module or the exception cases. Full policy
with worked examples: [`docs/arithmetic-policy.md`][arith].

[arith]: ./docs/arithmetic-policy.md

## Other policies

- **Concurrency primitives.** Use [`parking_lot`][pl] for sync,
  [`tokio::sync`][ts] for async. `std::sync::Mutex` and
  `std::sync::RwLock` are **banned in non-test code** via
  `clippy::disallowed_types`; see [`clippy.toml`](./clippy.toml).
  Neither `parking_lot` nor `tokio::sync` poisons on panic, and
  neither holds its guard across `.await` (enforced by
  `clippy::await_holding_lock`). Any custom shared-state primitive
  (atomic-based counter, lock-free queue, handwritten guard) must
  ship with a `loom` model that exercises its ordering invariants;
  see [`docs/loom.md`](./docs/loom.md) and the template crate
  [`crates/mango-loom-demo`](./crates/mango-loom-demo/).
- **Miri.** Any crate introducing `unsafe` must also add itself to
  `[workspace.metadata.mango.miri]` in the workspace
  [`Cargo.toml`](./Cargo.toml) **in the same PR**. That table is
  the curated subset `.github/workflows/miri.yml` runs against
  (PR job: changed-crate intersection under
  `-Zmiri-strict-provenance`; full job on push/schedule: full
  subset plus `-Zmiri-tree-borrows` canary). loom verifies
  ordering; Miri verifies soundness of `unsafe` blocks — both are
  required when Phase 3+ ships `unsafe` + atomics. Full policy:
  [`docs/miri.md`](./docs/miri.md).
- **Monotonic clock.** All protocol-relevant time math uses
  `std::time::Instant` (monotonic). `SystemTime` is only for
  human-facing display (logs, lease-TTL rendering). Full policy:
  [`docs/time.md`](./docs/time.md).
- **Crash-only design.** `kill -9` at any instant is a supported
  lifecycle event; clean shutdown is an optimization, not a
  correctness boundary. Every storage / Raft / Lease PR must be
  correct under process-kill at any program point. Full policy:
  [`docs/architecture/crash-only.md`](./docs/architecture/crash-only.md).
- **Formatting.** `cargo fmt --all -- --check` gates CI. Config is
  at [`rustfmt.toml`](./rustfmt.toml),
  [`.editorconfig`](./.editorconfig), and
  [`.gitattributes`](./.gitattributes) at repo root. Most IDEs pick
  these up automatically.
- **Lints.** `cargo clippy --workspace --all-targets --locked --
-D warnings` gates CI. Workspace lint table is in the workspace
  [`Cargo.toml`](./Cargo.toml); per-crate overrides in the local
  `clippy.toml`.
- **Supply chain.** `cargo-deny` runs every PR against
  [`deny.toml`](./deny.toml) (advisories, licenses, banned crates
  including `openssl-sys`, duplicate-version policy). `cargo-audit`
  runs on push, PR, and a nightly schedule. GitHub Actions are
  SHA-pinned. Future additions tracked in Phase 0.5: `cargo-vet`,
  SBOM via `cargo-cyclonedx`.
- **MSRV.** Workspace MSRV is **1.80** (see
  `rust-version` in [`Cargo.toml`](./Cargo.toml) and the `msrv`
  CI job). MSRV bumps are deliberate, land in their own PR, and
  update this file plus `Cargo.toml` plus
  `scripts/test-msrv-pin.sh` together. Full policy, ecosystem
  floors, and the `--target x86_64-unknown-linux-gnu` workaround
  for wasi-only edition2024 transitives:
  [`docs/msrv.md`](./docs/msrv.md).
- **Benches.** Performance claims are measured on the canonical
  hardware tiers in [`benches/runner/HARDWARE.md`][hw] against the
  pinned etcd oracle in [`benches/oracles/etcd/`][oracle]. Runner
  scripts live under [`benches/runner/`][runner]; the overall bench
  layout is documented in [`benches/README.md`][bench-readme]. Every
  bench run emits a hardware signature; every "beats etcd by Nx" PR
  body includes that signature.

[pl]: https://docs.rs/parking_lot
[ts]: https://docs.rs/tokio/latest/tokio/sync/index.html
[hw]: ./benches/runner/HARDWARE.md
[oracle]: ./benches/oracles/etcd/
[runner]: ./benches/runner/
[bench-readme]: ./benches/README.md

## Running the checks locally

### Build prerequisites

`tonic-build 0.12` and `prost-build 0.13` do not vendor `protoc`.
You need it installed locally to build the workspace.

- **Linux (Debian / Ubuntu):**
  ```bash
  sudo apt-get update && sudo apt-get install -y protobuf-compiler
  ```
- **macOS:**
  ```bash
  brew install protobuf
  ```
- **Windows:** use WSL2. Native Windows is not a supported dev
  target; CI runs on `ubuntu-24.04`.

### Commands CI runs

Copy-paste this list to reproduce CI locally. Run them all before
pushing.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --all-targets --locked --profile ci
cargo test --doc --workspace --locked
bash scripts/test-watchdog.sh
cargo doc --workspace --no-deps
cargo deny check
cargo audit
rustup run 1.80 cargo check --workspace --all-targets --locked \
  --target x86_64-unknown-linux-gnu
```

Optional — run the loom model-checker suite locally. Not required on
every PR, but mandatory for any PR that adds or modifies a
shared-state primitive (see [`docs/loom.md`](./docs/loom.md)):

```bash
RUSTFLAGS="--cfg loom -C debug-assertions" \
  LOOM_MAX_PREEMPTIONS=2 LOOM_MAX_BRANCHES=10000 \
  cargo nextest run --profile ci --release -p mango-loom-demo
```

Optional — run Miri locally. Not required on every PR, but
mandatory for any PR that adds or modifies `unsafe` code
(see [`docs/miri.md`](./docs/miri.md)):

```bash
MIRI_NIGHTLY=nightly-2026-04-01   # bump per docs/miri.md; must match miri.yml
rustup install "$MIRI_NIGHTLY"
rustup component add miri rust-src --toolchain "$MIRI_NIGHTLY"
cargo "+$MIRI_NIGHTLY" miri setup
MIRIFLAGS="-Zmiri-strict-provenance" \
  cargo "+$MIRI_NIGHTLY" miri test -p mango-loom-demo --lib --tests
```

Notes:

- `cargo nextest run`: the CI test runner. Per-test-class hard
  timeouts live in `.config/nextest.toml`; policy in
  [`docs/testing.md`](./docs/testing.md). `cargo test` still
  works for local iteration — same test bodies, different
  runner — but CI only runs nextest.
- `cargo test --doc`: nextest does NOT run doctests; this step
  covers them. Required to reproduce CI.
- `bash scripts/test-watchdog.sh`: regression smoke that proves
  nextest's `terminate-after` actually kills a runaway test.
  Runs in ~30s.
- `cargo doc`: the Definition of Done requires this to be
  warning-free. `cargo doc` `-D warnings` will gate once the
  doc-lint CI job lands.
- `rustup run 1.80 …`: MSRV gate. See
  [`docs/msrv.md`](./docs/msrv.md) for the `--target` rationale.
- `cargo deny`, `cargo audit`: install with
  `cargo install cargo-deny cargo-audit` if missing.
- `cargo nextest`: install with
  `cargo install cargo-nextest --locked` (or
  `brew install cargo-nextest` on macOS) if missing.

## PR description format

The PR description format is enforced by
[`.github/pull_request_template.md`](./.github/pull_request_template.md),
which GitHub auto-populates on every new PR. The template mirrors the
nine classification cases documented below; contributors fill in or
delete the sections that apply. Every PR description picks **one or
more** classification cases below (they stack), each with the specific
evidence the Reviewer's Contract demands. Items #1, #10, #11, #12
always apply.

- **Plumbing** (contract #1) — "This PR moves no north-star axis.
  Classification: plumbing. Verification strategy: [reproducible
  commands]."
- **Perf** (contract #2) — "Moves axis #N / bar '<name>'. Before /
  after numbers: [Criterion output from
  `benches/runner/<script>.sh`]. Oracle: etcd vX.Y pinned in
  `benches/oracles/etcd/`. Hardware: [signature from
  `benches/runner/run.sh`]."
- **Correctness** (contract #3) — "Moves axis #N / bar '<name>'.
  New property test or simulator scenario: [path]. Seed committed:
  [seed value or file]."
- **Concurrency** (contract #4) — "Moves axis #N / bar '<name>'.
  Shared-state primitive introduced: [describe]. `loom` test:
  [path]." (No `loom` test is an auto-`REVISE`.)
- **Unsafe** (contract #5) — "Adds / modifies `unsafe` in: [paths].
  `// SAFETY:` comment on every block: yes. Miri output:
  [`MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly miri test
  -p <crate>` result, or a written FFI-no-Miri justification].
  Workspace `unsafe` count delta: [+N / 0]; `unsafe`-growth PR label
  applied if +N > 0."
- **Security** (contract #6) — "Moves axis #N / bar '<name>'.
  Threat mitigated: [auth / crypto / DoS / supply-chain /
  side-channel / memory]. Named test: [path]. For credential / hash
  comparisons: constant-time test using `subtle`: [path]."
- **Reliability** (contract #7) — "Moves axis #N / bar '<name>'.
  Failure mode exercised: [describe]. Test:
  [`tests/reliability/...` or `tests/chaos/...`]. Bound asserted:
  [recovery time / no data loss / bounded resource use]."
- **Scale** (contract #8) — "Moves axis #N / bar '<name>'.
  `benches/runner/<name>-scale.sh` run to completion on canonical
  hardware: [signature + duration + result]."
- **New public API** (contract #9) — on top of whatever other case
  applies: "Doctest: yes. `#[must_use]` considered: yes.
  `#[non_exhaustive]` on new enums: yes. `cargo public-api --diff`:
  [output, advisory pre-Phase-6, gating from Phase 6].
  `cargo-semver-checks`: [status, gating from Phase 6]."

Every PR also includes a **Test plan** checklist (DoD classes that
apply, as in section 4) and a **Rollback** one-liner.
