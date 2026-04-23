## Summary

<!-- 1-3 sentences. What does this PR do and why. -->

## Classification

<!--
Pick one or more of the nine cases below. Cases stack — if your PR is
Perf + New public API, check both and fill both sub-blocks. Delete
cases that do not apply.

#1/#10/#11/#12 always apply: #1 is the plumbing case below (or you
move a named axis); #10/#11/#12 are satisfied via the Test plan
checklist further down (CI green, no new TODO/FIXME/unimplemented!,
axis-or-plumbing declared in this section).

Reviewer's Contract:
./ROADMAP.md#reviewers-contract-the-rust-expert-agent
-->

- [ ] **Plumbing** (#1) — no north-star axis moves.
      Verification strategy: <reproducible commands>
- [ ] **Perf** (#2) — axis #N / bar `<name>`.
      Before: <...> After: <...>
      Oracle: etcd v<X.Y> (pinned in `benches/oracles/etcd/`)
      Hardware tier + signature: <Tier 1 single-node / Tier 2 multi-node
      fleet; signature from `benches/runner/run.sh`>
- [ ] **Correctness** (#3) — axis #N / bar `<name>`.
      Property test: <path>; seed committed: <value or file>
- [ ] **Concurrency** (#4) — axis #N / bar `<name>`.
      `loom` test: <path>. (No loom => REVISE.)
- [ ] **Unsafe** (#5) — paths: <...>.
      `// SAFETY:` on every block: yes
      Miri: `MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly miri
      test -p <crate>` — <output / FFI-no-Miri justification>
      Workspace `unsafe` count delta: <+N / 0>;
      `unsafe`-growth PR label applied if +N > 0
- [ ] **Security** (#6) — axis #N / bar `<name>`.
      Threat: <auth / crypto / DoS / supply-chain / side-channel / memory>
      Named test: <path>
      Constant-time `subtle` (credentials / hashes): <path>
- [ ] **Reliability** (#7) — axis #N / bar `<name>`.
      Failure mode: <...>; Test: `tests/reliability/...` or
      `tests/chaos/...`
      Bound asserted: <recovery time / no data loss / bounded resource
      use>
- [ ] **Scale** (#8) — axis #N / bar `<name>`.
      `benches/runner/<name>-scale.sh` ran to completion on <Tier 1 /
      Tier 2> canonical hardware: signature + duration + result
- [ ] **New public API** (#9) — stacks on top of whatever case above
      applies.
      Doctest: yes
      `#[must_use]` considered: yes
      `#[non_exhaustive]` on new enums: yes
      `cargo public-api --diff`: <output, advisory pre-Phase-6, gating
      from Phase 6>
      `cargo-semver-checks`: <status, gating from Phase 6>

## Test plan

<!-- DoD classes from CONTRIBUTING.md §4 that apply. These checkboxes
also satisfy Reviewer's Contract items #10 (CI green), #11 (no new
TODO/FIXME/unimplemented!), and #12 (axis-or-plumbing declared above). -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --all-targets --locked`
- [ ] `cargo doc --workspace --no-deps`
- [ ] `cargo deny check`
- [ ] `cargo audit`
- [ ] `rustup run 1.80 cargo check --workspace --all-targets --locked`
- [ ] No new `TODO` / `FIXME` / `unimplemented!` introduced
- [ ] rust-expert adversarial review (final gate)

## Rollback

<!-- One line: what to do if this breaks prod. -->

## Refs

<!-- ROADMAP item, plan file, related issues/PRs. -->

- `ROADMAP.md:<line>`
- `.planning/<slug>.plan.md`
