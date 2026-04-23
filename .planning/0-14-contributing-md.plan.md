# Plan: Phase 0 item 0.14 — CONTRIBUTING.md (v3)

`ROADMAP.md:762`:

> Add `CONTRIBUTING.md` covering branch naming, commit style, PR
> template, the test bar, **the north-star bar + reviewer's contract**,
> and the arithmetic-primitive policy.

**Revisions applied from rust-expert v2 REVISE (B5, B6, R4, R5, N5–N8):**

- **B5** — §9 expanded from 4 collapsed cases to **9 one-liner cases**,
  one per Reviewer's Contract classification (plumbing, perf,
  correctness, concurrency, unsafe, security, reliability, scale,
  new public API), each naming the specific evidence the contract
  item demands.
- **B6** — §4's code-PR vs docs/plumbing-PR split is now semantic,
  not file-path: any `unsafe` block moves the PR into the code-PR
  case regardless of file path; CI-workflow PRs that introduce
  semantic enforcement are code-PRs too.
- **R4** — `scripts/verify-contributing-refs.sh` anchor computation
  now matches **`#{2,6}`** headings (h2–h6), not only `##`.
- **R5** — scheme-filter for the link-rot regex spelled out
  explicitly: `grep -vE '^(https?|mailto|ftp|tel|git):'`.
- **N5** — script shebang + exit-contract conventions pinned:
  `#!/usr/bin/env bash`, `set -euo pipefail`, exit 0 clean / exit 1
  on any failure with diff-style output.
- **N6** — noted; `cargo-vet` row stays as "Future additions
  (Phase 0.5)" pointer, fine.
- **N7** — §8's `rustup run 1.80` carries an explicit
  "(update when MSRV bumps)" qualifier; single-source is a
  follow-up, not this PR.
- **N8** — §8 doc-lint parenthetical rewritten to a forward-looking
  statement ("`cargo doc` `-D warnings` will gate once the doc-lint
  CI job lands"), no self-referential "N3" tag.

**Revisions applied from rust-expert v1 REVISE (S1–S3, B1–B4, R1–R3, M1–M5, N1–N4):**

- **S1** — forward-reference list corrected to **13** in-repo
  references (9 prior plans + 3 docs + 1 benches); audit script
  walks the repo root, not just `.planning/`.
- **S2** — exact ROADMAP heading anchors named (computed from GitHub's
  lowercase + space→`-` + apostrophe-strip rule); `scripts/verify-
contributing-refs.sh` ships in this PR as the mechanical defense
  against anchor drift.
- **S3** — five commit types pinned: `feat`, `fix`, `refactor`, `chore`,
  `docs`. Direct-to-`main` exception for `chore: mark <slug> done on
roadmap` called out.
- **B1** — section 9 now ships the PR-description-format rule in prose
  verbatim (four classification cases), not a stub deferring to 0.15.
- **B2** — Linux (`apt`), macOS (`brew`), and Windows (WSL2 stance)
  build-prereq paths covered.
- **B3** — section 4 has two cases: code-PR test-class list (DoD), and
  docs/plumbing-PR verification-strategy requirement. "Tests mandatory"
  applies to both; the form differs.
- **B4** — link-rot regex handles bare (`](foo)`), `./`-prefixed, and
  anchored (`](foo#bar)`) relative links, not just one form.
- **R1** — per-section cut-line spelled out: §5 (reviewer's contract)
  gets 3-sentence classification summary, not 1-liner; §6 stays
  1-liner; §7 stays 1-line-per-row.
- **R2** — new §0 "Start here" welcomes small-PR contributors and
  surfaces the plumbing classification as an explicit escape valve.
- **M1** — §1 cross-references `ROADMAP.md#working-rules`.
- **M2** — §1 cross-references `ROADMAP.md#crate-inventory--non-rolled-stack`
  with the "ADR before code" rule.
- **M3** — `scripts/verify-contributing-refs.sh` delivered as a
  reproducible artifact, not a manual-review step.
- **M4** — §1 names the plumbing classification explicitly.
- **M5** — all config-file paths surfaced: `rustfmt.toml`,
  `.editorconfig`, `.gitattributes`, `clippy.toml`, `deny.toml`.
- **N1–N4** — cosmetic nits folded in inline.

## What the item actually asks for

`CONTRIBUTING.md` is the **single entrypoint** for a new contributor.
Everything the roadmap, policy docs, and reviewer's contract already
say in their own vocabularies gets stitched together here, with
**paraphrase + pointer** (not copy) to the source documents so the
entrypoint stays short and the source documents stay the single source
of truth.

**13 in-repo forward references** need to land in this PR. Nine come
from prior plans; four come from policy docs and `benches/README.md`
that were authored assuming this PR would reciprocate. This PR's
primary job is **aggregation and discoverability** — no policy doc is
updated in place, no new policy is invented here.

## Scope

### In

- `CONTRIBUTING.md` at repo root.
- `scripts/verify-contributing-refs.sh` — reproducible audit script
  that verifies (a) every forward reference from the 13 in-repo docs
  resolves to a backward link in `CONTRIBUTING.md`, and (b) every
  `./ROADMAP.md#<anchor>` link in `CONTRIBUTING.md` resolves to a
  real `##` heading in `ROADMAP.md`.

### Section list for `CONTRIBUTING.md`

0. **Start here** (new, from R2) — two paragraphs welcoming small PRs
   and naming the plumbing classification as the escape valve for
   typo-fix / doc-tweak / dep-bump PRs. "You don't need a Criterion
   bench to fix a typo."

1. **Before you write code** — roadmap-driven workflow (pick item →
   plan → rust-expert review → branch → implement → PR → rust-expert
   review → merge). Paraphrase of `ROADMAP.md#working-rules` (5
   bullets) with link. **Crate-inventory ADR rule** spelled out:
   before adding or replacing any crate in the inventory table, file
   an ADR in `.planning/adr/`. Links:
   - `ROADMAP.md#working-rules`
   - `ROADMAP.md#crate-inventory--non-rolled-stack`

2. **Branch naming** — `<type>/<slug>` where `<type>` is one of
   **`feat`, `fix`, `refactor`, `chore`, `docs`**. Never work on
   `main` / `master` directly. `<slug>` is kebab-case, matches the
   plan filename under `.planning/`.

3. **Commit style** — conventional commits with scope:
   `<type>(<scope>): <description>`. Small atomic commits, one
   concern per commit. `Co-Authored-By` trailer required when the
   commit was co-produced with an assistant. **Direct-to-`main`
   exception**: after a PR merges, the roadmap checkbox flip is
   committed directly to `main` with message
   `chore: mark <slug> done on roadmap` — this is the **only**
   commit that bypasses the PR / review flow; contributors filing
   their first PR must not include the checkbox flip in the feature
   PR.

4. **The test bar** — two cases, **classified semantically, not by
   file path**:
   - **Code-PR case**: PR touches `crates/`, **or** introduces
     semantic enforcement logic (new clippy rule with real effect,
     new CI gate, new lint), **or** contains any `unsafe` block
     regardless of file path. The Definition of Done test-class
     list applies — unit, property (`proptest` default), integration,
     crash/recovery, `loom` for shared state, `cargo fuzz` for parser
     surfaces, Miri for `unsafe` code, the watchdog timeout.
     Auto-`REVISE` for missing any applicable class. Link:
     `ROADMAP.md#definition-of-done-every-phase`.
   - **Docs-or-plumbing-PR case**: PR is pure docs, dep-bump,
     formatting, or CI plumbing with no semantic enforcement effect.
     The PR description names its **verification strategy** —
     reproducible commands the reviewer can run to confirm the
     change did what it claimed. "Trust CI" is not a verification
     strategy.

   "Tests are mandatory" applies to both cases; the form differs.

5. **The north-star bar + reviewer's contract** — 3-sentence summary
   (per R1 — classification is too load-bearing to flatten): mango's
   north star is "beats etcd on every axis we care about" (not parity,
   not "good enough"). Every PR declares which axis it moves and names
   the verifying test, or declares itself plumbing. The rust-expert
   classifies each PR (plumbing / perf / correctness / concurrency /
   unsafe / security / reliability / scale / new-public-API) and
   applies the contract items for that classification; **items #1,
   #10, #11, #12 always apply**. `APPROVE` is the merge gate — not
   the maintainer's sign-off, not the CI green-light alone. Links:
   - `ROADMAP.md#north-star-non-negotiable`
   - `ROADMAP.md#reviewers-contract-the-rust-expert-agent`

6. **Arithmetic-primitive policy** — one-sentence TL;DR: protocol
   counters use `checked_*`, time budgets use `saturating_*`, hashes
   / ring indices use `wrapping_*`, `usize` slice math either uses
   `checked_*` or carries a `// BOUND:` comment. Full policy:
   `docs/arithmetic-policy.md`.

7. **Other policies** (one line + link each):
   - Concurrency: `parking_lot` (sync), `tokio::sync` (async);
     `std::sync::Mutex` / `RwLock` banned by `clippy::disallowed_types`.
     See `clippy.toml`.
   - Monotonic clock: all protocol time uses `Instant`, never
     `SystemTime`. See `docs/time.md`.
   - Crash-only: `kill -9` at any instant is supported; clean
     shutdown is an optimization, not correctness. See
     `docs/architecture/crash-only.md`.
   - Formatting: `cargo fmt --check` gates; config lives in
     `rustfmt.toml`, `.editorconfig`, `.gitattributes`.
   - Lints: `cargo clippy --workspace --all-targets --locked --
-D warnings` gates; workspace lint table in `Cargo.toml`; per-
     crate `clippy.toml`.
   - Supply chain: `cargo-deny` (`deny.toml`) + `cargo-audit`;
     SHA-pinned GitHub Actions. Future additions: `cargo-vet`,
     `cargo-cyclonedx` SBOM (Phase 0.5).
   - MSRV: 1.80. Reproduce locally with `rustup run 1.80 cargo check
--workspace --all-targets --locked`.
   - Benches: hardware tiers in `benches/runner/HARDWARE.md`; bench
     oracle in `benches/oracles/etcd/`; runner scripts in
     `benches/runner/`.

8. **Running the checks locally** — the exact command list CI runs,
   copy-pasteable. With build-prereqs (from B2):
   - **Linux (Debian/Ubuntu)**: `sudo apt-get install -y
protobuf-compiler` (tonic-build 0.12 / prost-build 0.13 do not
     vendor `protoc`).
   - **macOS**: `brew install protobuf`.
   - **Windows**: use WSL2 (native Windows is not a supported dev
     target; CI runs on `ubuntu-24.04`).

   Then (all platforms):
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets --locked -- -D warnings`
   - `cargo test --workspace --all-targets --locked`
   - `cargo doc --workspace --no-deps` (DoD requires this to be
     warning-free; `cargo doc` `-D warnings` will gate once the
     doc-lint CI job lands in a future Phase 0 / 0.5 item.)
   - `cargo deny check`
   - `cargo audit`
   - `rustup run 1.80 cargo check --workspace --all-targets --locked`
     (MSRV gate; update the `1.80` version when MSRV bumps —
     single-sourcing from `Cargo.toml`'s `rust-version` is a
     follow-up.)

9. **PR description format** (per B1 — in-prose now, template
   enforcement in 0.15). Every PR description must include **one**
   of the following nine classification cases, matching the
   Reviewer's Contract items 1–9. Classifications can stack (a
   perf-claiming PR that adds a new public API declares both); items
   #1, #10, #11, #12 always apply.
   - **Plumbing** (contract #1) — "This PR moves no north-star axis.
     Classification: plumbing. Verification strategy: [reproducible
     commands]."
   - **Perf** (contract #2) — "Moves axis #N / bar '<name>'.
     Before/after numbers: [Criterion output from
     `benches/runner/<script>.sh`]. Oracle: etcd vX.Y pinned in
     `benches/oracles/etcd/`. Hardware: [signature from
     `benches/runner/run.sh`]."
   - **Correctness** (contract #3) — "Moves axis #N / bar '<name>'.
     New property test or simulator scenario: [path]. Seed
     committed: [seed value or file]."
   - **Concurrency** (contract #4) — "Moves axis #N / bar '<name>'.
     Shared-state primitive introduced: [describe]. `loom` test:
     [path]. (Auto-`REVISE` without one.)"
   - **Unsafe** (contract #5) — "Adds/modifies `unsafe` in: [paths].
     `// SAFETY:` comment on every block: yes. Miri output:
     [`MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly miri test
     -p <crate>` result, or FFI-no-Miri justification]. Workspace
     `unsafe` count delta: [+N / 0]; `unsafe`-growth label applied
     if +N > 0."
   - **Security** (contract #6) — "Moves axis #N / bar '<name>'.
     Threat mitigated: [auth / crypto / DoS / supply-chain /
     side-channel / memory]. Named test: [path]. For cred/hash
     comparisons: constant-time test using `subtle`: [path]."
   - **Reliability** (contract #7) — "Moves axis #N / bar '<name>'.
     Failure mode exercised: [describe]. Test: [`tests/reliability/...`
     or `tests/chaos/...`]. Bound asserted: [recovery time / no data
     loss / bounded resource use]."
   - **Scale** (contract #8) — "Moves axis #N / bar '<name>'.
     `benches/runner/<name>-scale.sh` run to completion on canonical
     hardware: [signature + duration + result]."
   - **New public API** (contract #9) — plus "Doctest: yes.
     `#[must_use]` considered: yes. `#[non_exhaustive]` on new enums:
     yes. `cargo public-api --diff`: [output, advisory pre-Phase-6,
     gating from Phase 6]. `cargo-semver-checks`: [status, gating
     from Phase 6]."

   Every PR must also include a **Test plan** checklist (DoD classes
   that apply) and a **Rollback** one-liner.

### Out of scope (deliberately)

- **The PR template file itself** (`.github/PULL_REQUEST_TEMPLATE.md`
  or similar) — that's ROADMAP item 0.15. Section 9 carries the
  prose rule until the template automates it.
- Code of conduct, license-grant text, CLA. Not named in the roadmap
  line.
- Any new policy authoring. Aggregation only.
- Editing the source policy docs. They already end with "linked from
  `CONTRIBUTING.md`" notes (written expecting this PR).
- A `docs/` site generator hookup (Phase 12).

## Approach

1. Write `CONTRIBUTING.md` with the 10 sections above (§0–§9).
2. Every section is **paraphrase + link**, not copy. Per-section
   budgets (from R1):
   - §0 Start here: 2 short paragraphs.
   - §1 Before you write code: 5-bullet paraphrase of working rules
     - 2-line crate-inventory note + 2 links.
   - §2 Branch naming: 4 lines.
   - §3 Commit style: 6 lines including direct-to-`main` exception.
   - §4 Test bar: 8 lines covering both cases + DoD link.
   - §5 Reviewer's contract: 3 sentences + 2 links.
   - §6 Arithmetic policy: 2 lines + link.
   - §7 Other policies: 1 line per row + link (8 rows).
   - §8 Running checks locally: 3-platform install + 7-line command
     block.
   - §9 PR description format: 9 classification cases in prose +
     the test-plan / rollback requirement.
3. **GitHub heading anchors (from S2)** — `CONTRIBUTING.md` links
   these exact anchors into `ROADMAP.md`; each one was computed from
   GitHub's markdown-anchor rule (lowercase, spaces→`-`,
   apostrophes/punctuation stripped except `-`, `&` → empty so
   doubled `-` survives):
   - `ROADMAP.md#north-star-non-negotiable`
   - `ROADMAP.md#working-rules`
   - `ROADMAP.md#crate-inventory--non-rolled-stack`
   - `ROADMAP.md#definition-of-done-every-phase`
   - `ROADMAP.md#reviewers-contract-the-rust-expert-agent`
4. Ship `scripts/verify-contributing-refs.sh` as the mechanical
   defense against drift (M3 + S2).

## Files touched

- `CONTRIBUTING.md` (new, root).
- `scripts/verify-contributing-refs.sh` (new, executable) — with
  `#!/usr/bin/env bash` shebang and `set -euo pipefail` at the top
  (N5). Exit 0 on clean, exit 1 on any failure with a diff-style
  list of the missing reciprocal / broken anchor / broken relative
  link. Steps:
  1. For each of the 13 in-repo docs known to forward-reference
     `CONTRIBUTING.md`, assert `CONTRIBUTING.md` contains a backward
     link to that doc (by path).
  2. For each relative link in `CONTRIBUTING.md` (`](./...)`,
     `](../...)`, `](...)` without scheme, and all of those with
     `#anchor` suffixes), assert the target file exists. Scheme
     filter (R5) excludes external links matching
     `grep -vE '^(https?|mailto|ftp|tel|git):'`.
  3. For each `./ROADMAP.md#<anchor>` link in `CONTRIBUTING.md`,
     assert the anchor resolves — compute the expected anchor from
     each `#{2,6}` heading in `ROADMAP.md` (R4: covers h2–h6 not
     only h2, so future deep-links to `###` reviewer-contract
     subsections don't false-negative) using the GitHub-slug rule
     (lowercase, space→`-`, apostrophe/punctuation strip, `&`→empty),
     and confirm the linked anchor is in the computed set.

That's it. No other file touched.

## Test / verification strategy

This is a docs-only PR; it still has a test bar. Per "HOPE YOU'RE
ALL ADDING TESTS" and the updated §4 docs/plumbing-PR rule above:

1. **`scripts/verify-contributing-refs.sh` runs clean.** This is the
   reproducible, committed, runnable-anywhere check that the
   aggregator actually aggregates. The script is the test.
2. **Manual checklist, listed in the PR description** (belt-and-
   suspenders — the script enforces mechanically, the checklist
   documents the intent):
   - [ ] All 13 forward references from these files reciprocate
         into CONTRIBUTING.md:
     - `.planning/0-2-rustfmt-editorconfig.plan.md`
     - `.planning/0-3-lint-hardening.plan.md`
     - `.planning/0-4-arithmetic-policy.plan.md`
     - `.planning/0-7-cargo-deny.plan.md`
     - `.planning/0-8-cargo-audit.plan.md`
     - `.planning/0-10-bench-oracle-harness.plan.md`
     - `.planning/0-11-monotonic-clock-policy.plan.md`
     - `.planning/0-12-crash-only-declaration.plan.md`
     - `.planning/0-13-mango-proto-skeleton.plan.md`
     - `docs/time.md`
     - `docs/architecture/crash-only.md`
     - `docs/arithmetic-policy.md`
     - `benches/README.md`
   - [ ] All five `ROADMAP.md#<anchor>` links resolve to real
         `#{2,6}` headings (listed above in Approach §3).
   - [ ] `cargo fmt --check`, `cargo clippy`, `cargo test`,
         `cargo deny`, `cargo audit`, MSRV check — green (sanity
         check only; docs-only change should not affect any of
         these).
3. **Link-rot regex** used by the script handles every relative-link
   form:
   - `](./path)` — `./`-prefixed
   - `](../path)` — parent-dir
   - `](path)` — bare, excluding `http`, `mailto:`, `#`-only
   - `](path#anchor)` — anchored

   Captured with a ripgrep pattern equivalent to
   `\]\(([^)#:][^)]*?)(#[^)]+)?\)`, filtered by scheme.

4. **No new rules introduced** — diff-read by the rust-expert on the
   final PR; any substantive policy text in `CONTRIBUTING.md` that
   doesn't have a matching passage in the source doc is a REVISE
   trigger (the doc-aggregator invariant).

## Rollback

Revert the merge commit. `CONTRIBUTING.md` and
`scripts/verify-contributing-refs.sh` are additive; no other file
depends on either. No CI job reads CONTRIBUTING; the verify script
is not wired into CI in this PR (a follow-up roadmap item can wire
it into the `fmt` job or a new `docs` job once the Phase 0 set
closes).

## Risks

- **Staleness of `ROADMAP.md` heading anchors.** Headings get edited;
  anchors change silently. _Mitigation_: `verify-contributing-refs.sh`
  checks every anchor on every run. If a future PR renames a heading,
  this script fails in the first CI run that invokes it — currently
  local only, but available to any contributor pre-push.
- **Scope creep.** The instinct on `CONTRIBUTING.md` is "while I'm
  here…" _Mitigation_: strict adherence to the 10-section outline.
  Anything not on the list is a follow-up.
- **Sources-of-truth duplication.** `CONTRIBUTING.md` paraphrasing
  policy text risks drift. _Mitigation_: per-section cut-line in R1
  — paraphrase sentence, link out for full rule. The rust-expert's
  final-diff review treats policy-verbatim copies as REVISE.
- **Onboarding cliff for external OSS contributors.** _Mitigation_:
  §0 "Start here" explicitly welcomes small PRs and names the
  plumbing escape valve (R2).

## Refs

- `ROADMAP.md:762` (this item — flipped on merge)
- 13 forward references to fulfill:
  - 9 prior plans (see Test plan checklist above)
  - `docs/time.md:379`
  - `docs/architecture/crash-only.md:478`
  - `docs/arithmetic-policy.md:236`
  - `benches/README.md:103`
- ROADMAP anchors CONTRIBUTING links to:
  - `ROADMAP.md#north-star-non-negotiable`
  - `ROADMAP.md#working-rules`
  - `ROADMAP.md#crate-inventory--non-rolled-stack`
  - `ROADMAP.md#definition-of-done-every-phase`
  - `ROADMAP.md#reviewers-contract-the-rust-expert-agent`
- Rust-expert plan review v1: REVISE (S1/S2/S3, B1–B4, R1–R3, M1–M5,
  N1–N4) — all applied above.
- Rust-expert plan review v2: REVISE (B5, B6, R4, R5, N5–N8) — all
  applied above.
- Rust-expert plan review v3: APPROVE_WITH_NITS (title bump +
  #10/#11/#12 enumeration in CONTRIBUTING prose at authoring time)
  — cleared to implement.
