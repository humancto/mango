# 0.15 — Add a PR template (plan v2)

**Roadmap item**: `ROADMAP.md:763` — "Add a PR template that forces
every PR description to declare which north-star axis the change
moves, names the verifying test, and records the measured number
(or honestly marks as plumbing)."

**Classification**: Plumbing (Reviewer's Contract item #1). No
north-star axis moves. The PR ships a GitHub PR template file and
updates `CONTRIBUTING.md` §9 to reference it. Verification
strategy: `bash scripts/verify-contributing-refs.sh` plus a
throwaway-branch draft-PR smoke test (see Verification strategy
below).

## Goal

Ship a PR template that:

1. Auto-populates the PR body on `gh pr create` (and the web UI).
2. **Forces** the contributor to pick a north-star classification
   by presenting all nine cases and a "pick one or more" instruction.
3. Is **the authoring format** for section 9 of `CONTRIBUTING.md` —
   which currently says "Until `ROADMAP.md:763` adds a real PR
   template, the description format is enforced in prose."
4. Includes the test-plan checklist, rollback one-liner, and refs
   section that every shipped PR in Phase 0 has used.

## Non-goals

- Do **not** add GitHub Actions that mechanically validate PR body
  shape (e.g. regex on the description). That's enforcement on
  the bot, not on the human — reviewer catches mis-classifications.
  Future item once Phase 6 ships public API.
- Do **not** add multiple templates (bug/feature/docs variants).
  Mango's classification is orthogonal to "is this a feature" — a
  docs PR can be plumbing or reliability; a feature PR can be perf
  or correctness. One template covers all. Cases **stack** per
  CONTRIBUTING §9; a "Perf + New public API" PR checks both boxes
  and fills both sub-blocks.
- Do **not** retroactively edit the bodies of PR #21 / PR #22.
  GitHub PR templates only apply to PRs opened after the template
  lands. Those two shipped with an ad-hoc prose format that
  satisfies §9 and are grandfathered.
- Do **not** rewrite CONTRIBUTING §9 prose wholesale. Only the
  forward-reference note gets replaced (see "CONTRIBUTING §9
  update" below).
- Do **not** add `Co-Authored-By: Claude` to the template body.
  That trailer belongs in commit messages (git trailer), not the
  PR body.
- Do **not** add a CHANGELOG field yet. Once Phase 6 ships a
  public API, a follow-up item adds the CHANGELOG slot.

## Files

New:

- `.github/pull_request_template.md` — the template GitHub renders
  on every new PR. Lowercase filename per
  [GitHub's canonical docs](https://docs.github.com/en/communities/using-templates-to-encourage-useful-issues-and-pull-requests/creating-a-pull-request-template-for-your-repository).
  Markdown with HTML comments for instructions. **Caveat**: HTML
  comments are stripped from the rendered markdown but remain in
  the PR body edit buffer, email notifications, and `gh pr view`
  (raw text). Contributors will see them; reviewers viewing the
  rendered PR on github.com will not.

Modified:

- `CONTRIBUTING.md` §9 — update the forward-reference note (one
  sentence diff shown below) + add a link to the new template
  path.
- `scripts/verify-contributing-refs.sh` — no script changes
  needed; Check 2 already resolves any new relative link added to
  CONTRIBUTING.md via `sed -n 's|.*\](\([^)]*\)).*|\1|gp'` and
  checks `[ -e "$path" ]`. `.github/pull_request_template.md`
  exists as a file → `-e` passes trivially. (Verified in plan
  review.)

## Template structure

One section per piece of evidence. Contributors fill in or delete
what doesn't apply.

````markdown
## Summary

<!-- 1-3 sentences. What does this PR do and why. -->

## Classification

<!--
Pick one or more of the nine cases below. Cases stack — if
your PR is Perf + New public API, check both and fill both
sub-blocks. Delete cases that do not apply.

#1/#10/#11/#12 always apply: #1 below is the plumbing case
(or you move a named axis); #10/#11/#12 are satisfied via the
Test plan checklist below (CI green, no new TODO/FIXME/
unimplemented!, axis-or-plumbing declared in this section).

Reviewer's Contract:
./ROADMAP.md#reviewers-contract-the-rust-expert-agent
-->

- [ ] **Plumbing** (#1) — no north-star axis moves.
      Verification strategy: <reproducible commands>
- [ ] **Perf** (#2) — axis #N / bar `<name>`.
      Before: <...> After: <...>
      Oracle: etcd v<X.Y> (pinned in `benches/oracles/etcd/`)
      Hardware tier + signature: <Tier 1 single-node / Tier 2
      multi-node fleet; signature from `benches/runner/run.sh`>
- [ ] **Correctness** (#3) — axis #N / bar `<name>`.
      Property test: <path>; seed committed: <value or file>
- [ ] **Concurrency** (#4) — axis #N / bar `<name>`.
      `loom` test: <path>. (No loom => REVISE.)
- [ ] **Unsafe** (#5) — paths: <...>.
      `// SAFETY:` on every block: yes
      Miri: `MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly
      miri test -p <crate>` — <output / FFI-no-Miri justification>
      Workspace `unsafe` count delta: <+N / 0>;
      `unsafe`-growth PR label applied if +N > 0
- [ ] **Security** (#6) — axis #N / bar `<name>`.
      Threat: <auth / crypto / DoS / supply-chain / side-channel
      / memory>
      Named test: <path>
      Constant-time `subtle` (credentials / hashes): <path>
- [ ] **Reliability** (#7) — axis #N / bar `<name>`.
      Failure mode: <...>; Test: `tests/reliability/...` or
      `tests/chaos/...`
      Bound asserted: <recovery time / no data loss / bounded
      resource use>
- [ ] **Scale** (#8) — axis #N / bar `<name>`.
      `benches/runner/<name>-scale.sh` ran to completion on
      <Tier 1 / Tier 2> canonical hardware: signature + duration
      + result
- [ ] **New public API** (#9) — stacks on top of whatever case
      above applies.
      Doctest: yes
      `#[must_use]` considered: yes
      `#[non_exhaustive]` on new enums: yes
      `cargo public-api --diff`: <output, advisory pre-Phase-6,
      gating from Phase 6>
      `cargo-semver-checks`: <status, gating from Phase 6>

## Test plan

<!-- DoD classes from CONTRIBUTING.md §4 that apply. These
checkboxes also satisfy Reviewer's Contract items #10 (CI
green), #11 (no new TODO/FIXME/unimplemented!), and #12
(axis-or-plumbing declared above). -->

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
````

## CONTRIBUTING §9 update

The current CONTRIBUTING.md §9 opens with:

> Until [`ROADMAP.md:763`][pr-template-item] adds a real PR template,
> the description format is enforced in prose.

After this PR, the same paragraph reads:

> The PR description format is enforced by
> [`.github/pull_request_template.md`](./.github/pull_request_template.md),
> which GitHub auto-populates on every new PR. The template mirrors
> the nine classification cases documented below; contributors fill in
> or delete the sections that apply.

No other prose in §9 changes. The nine-case enumeration that follows
remains the documentation source of truth; the template is the
authoring format.

## Verification strategy

Per CONTRIBUTING §4 docs/plumbing-PR case, the verification is:

1. `bash scripts/verify-contributing-refs.sh` — all three checks pass
   after CONTRIBUTING.md §9 links `.github/pull_request_template.md`.
2. **Template-load smoke test**: push the feature branch, then run
   `gh pr create --web --draft --title "0.15 test" --body ""` to
   open a browser PR-create form. GitHub picks up the template and
   pre-populates the body; verify visually, then close the browser
   tab (does not create a PR). Alternative: open a real draft PR
   with the template-populated body and immediately close it. **`gh
   pr create --dry-run` is not a real flag** (confirmed against
   `gh` CLI); `--web` is the supported non-destructive path.
3. `cargo fmt --all -- --check`, clippy, test, doc — standard CI
   commands. No Rust code changes, so these are green trivially;
   they're in the checklist for Reviewer's Contract #10 coverage.

## Risks

1. **Template becomes stale** as CONTRIBUTING §9 evolves. Mitigation:
   the verify script ensures the template path is linked from
   CONTRIBUTING; the inverse (template fields match §9 cases) is
   not mechanical. A future item can add a `verify-pr-template.sh`
   that diffs the case names, but that's out of scope here.
2. **Template friction** discourages plumbing PRs. Mitigation: §0
   of CONTRIBUTING explicitly welcomes small PRs; the template
   leads with "Plumbing" as the first checkbox so typo-fixers
   aren't staring at a wall of perf / correctness / concurrency
   fields.
3. **GitHub PR template location ambiguity**:
   `.github/pull_request_template.md` vs
   `.github/PULL_REQUEST_TEMPLATE.md` vs root
   `PULL_REQUEST_TEMPLATE.md`. GitHub supports all three, but the
   canonical path is `.github/pull_request_template.md` (lowercase).
4. **HTML-comment leakage**: `<!-- ... -->` strips from rendered
   markdown but remains in the PR edit buffer, email notifications,
   and `gh pr view`. Contributors will see them. Accepted — the
   alternative (instructions outside comments) clutters the final
   rendered PR body.

## Test plan

This PR is **docs/plumbing**. No Rust code changes. Per CONTRIBUTING
§4: "trust CI" is not a test plan. The reproducible verification
commands are listed under "Verification strategy" above. The
`gh pr create --web` smoke test is the mechanical proof the
template actually loads on GitHub.

Following the user's standing rule — "HOPE YOU'RE ALL ADDING
TESTS" — the test here is the script check plus the
template-load smoke test. There is no Rust test to add; the
template is a markdown file consumed by GitHub's PR-create flow,
not by any Rust code.

## Rollback

Revert the merge commit. `.github/pull_request_template.md` and
the CONTRIBUTING.md forward-reference update are additive; no
other file depends on either. GitHub silently falls back to an
empty PR body if the template is removed.

## Refs

- `ROADMAP.md:763` (this item — flipped on merge)
- `CONTRIBUTING.md` §9 (PR description format — the template is
  the authoring form of this section)
- `scripts/verify-contributing-refs.sh` (Check 2 audits the new
  template path once CONTRIBUTING links it)
- `.planning/0-14-contributing-md.plan.md` (prior item; CONTRIBUTING
  §9 originated there)

---

## Revisions applied from rust-expert v1 review (APPROVE_WITH_NITS)

- **B1**: `gh pr create --dry-run` is not a real flag. Replaced with
  `gh pr create --web --draft` smoke test; documented non-destructively.
- **B2**: Unsafe case now spells out the exact Miri command
  `MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly miri test -p
  <crate>` inline in the template; matches CONTRIBUTING §9 exactly.
- **B3**: Scale case now references the canonical hardware **tier**
  (Tier 1 / Tier 2) from `benches/runner/HARDWARE.md`, mirroring §9.
- **R1**: Added explicit "#10/#11/#12 always apply via the Test plan
  checklist" line in the Classification comment block, and added
  "No new `TODO` / `FIXME` / `unimplemented!` introduced" to the
  Test plan checkbox list so #11 is visibly present.
- **R2**: Classification comment now explicitly says cases **stack**
  and instructs the author to fill each checked case's sub-block.
- **R3**: Non-goal added — PRs #21 / #22 are grandfathered; the
  template only affects future PRs.
- **M1**: Non-goal added — `Co-Authored-By: Claude` goes in commit
  trailers, not the PR body; template does not include it.
- **M2**: Non-goal added — no CHANGELOG field until Phase 6 public
  API lands; noted as future-growth edit.
- **M3**: "CONTRIBUTING §9 update" section added with the exact
  one-sentence replacement diff.
- **M4 (informational)**: Confirmed the existing
  `verify-contributing-refs.sh` Check 2 regex and `-e` test
  resolve `.github/pull_request_template.md` correctly; no script
  change needed.
- **Nits**: "pick one+" → "pick one or more"; plan's Classification
  block up top now names its Verification strategy; HTML-comment
  caveat documented; template (when written) ends with trailing
  newline.
