# Plan: `cargo-audit` CI job (push + PR + nightly, auto-file issue)

Roadmap item: Phase 0 — "Add `cargo-audit` CI job (RustSec advisories)
running on push, PR, and a nightly schedule. **Nightly schedule
auto-files a GitHub issue on any new advisory**, even if no PR is
open, so we don't sit on advisories between PRs." (`ROADMAP.md:755`)

## Goal

Two distinct modes, one tool:

1. **Push + PR mode** — `cargo audit` (via `rustsec/audit-check@v2.0.0`)
   runs on every push to main and every PR touching the dep graph.
   The action fails the check **on any `critical` advisory**
   (advisories with `informational: null` in the RustSec DB). It
   does NOT fail on yanked / unmaintained / unsound — those remain
   the strict gate of `cargo-deny check advisories` (PR #14). This
   is a deliberate asymmetry; see **Relationship to `cargo-deny`**.
2. **Nightly mode** — the same action runs at a daily cron. On a
   finding it **auto-files a GitHub issue** (no labels — the action
   creates bare issues; label policy is a follow-up), so an advisory
   published against already-merged code surfaces in the issue
   tracker without anyone needing to open a PR.

## Relationship to `cargo-deny`

`cargo-deny check advisories` (PR #14) already consults the RustSec
DB at PR time with strict posture: `yanked = "deny"`,
`unmaintained = "all"`, `unsound = "all"`. Why a second tool?

- **Posture asymmetry is intentional.** `cargo-deny` is the strict
  gate — any advisory class fails the PR. The `rustsec/audit-check`
  action is narrower in its push/PR mode (critical-only) but buys us
  the thing `cargo-deny` does not: **auto-filing a GitHub issue on a
  schedule trigger**. That's the whole reason this PR exists.
- **The nightly job is the differentiator.** Between PRs, a newly
  published advisory against `tokio` (hypothetically) would NOT be
  surfaced by `cargo-deny` in CI until the next PR runs.
  `cargo-audit` on a cron catches it the next morning and files an
  issue, regardless of PR activity.
- **Defense in depth on PRs is a bonus, not the goal.** When a PR
  does bring in a critical advisory, both gates fire — `cargo-deny`
  with stricter classes, `cargo-audit` with a second independent DB
  read. If one tool's DB cache is stale, the other catches it.

## North-star axis

**Security + Supply-chain.** The posture: an advisory against a dep
mango uses is never allowed to sit undiscovered for more than 24
hours. Go etcd has no equivalent — the Go ecosystem relies on
`govulncheck` (advisory-only, not blocking) and maintainers watching
the Go security mailing list. mango treats advisories as CI-gated,
tracker-visible, and time-bounded.

## Approach

One new workflow file (`audit.yml`) — separate from `ci.yml` so that
`issues: write` permission scope stays surgical and isn't granted to
the fmt / clippy / test / deny jobs that don't need it.

### D1. `.github/workflows/audit.yml` (NEW)

```yaml
# Mango RustSec advisory gate.
#
# Four triggers:
#   - push to main       — re-check after every merge.
#   - pull_request       — block merge on critical advisories
#                          (path-filtered to dep-graph changes to
#                          avoid wasted CI on docs-only PRs).
#   - schedule (nightly) — the differentiator: advisories published
#                          AFTER a merge still surface here.
#                          rustsec/audit-check auto-files a GitHub
#                          issue on the nightly trigger only.
#   - workflow_dispatch  — lets us manually fire the scheduled path
#                          post-merge to prove the issue-creation
#                          mechanism end-to-end, without waiting up
#                          to 24h for the first cron firing.
#
# Posture note: rustsec/audit-check v2.0.0 fails the check only on
# `critical` advisories (informational: null). Yanked, unmaintained,
# and unsound remain the strict gate of `cargo-deny check advisories`
# in ci.yml. Intentional asymmetry — see the plan doc.
#
# Why a separate workflow from ci.yml: the nightly job needs
# `issues: write`, which the fmt/clippy/test/deny jobs must not have.
# Split keeps the permission scope surgical.

name: audit

on:
  push:
    branches: [main]
  pull_request:
    paths:
      - "**/Cargo.toml"
      - "**/Cargo.lock"
      - ".github/workflows/audit.yml"
  schedule:
    # 07:23 UTC daily — offset from the on-the-hour queue GitHub
    # throttles. After most US/EU advisory-publication windows,
    # before the working day in both.
    - cron: "23 7 * * *"
  workflow_dispatch:

permissions:
  contents: read
  issues: write # rustsec/audit-check files an issue on schedule findings

concurrency:
  # Pin the schedule trigger to its own group so two nightlies cannot
  # overlap (the action's dedup is a GitHub search — eventually-
  # consistent — so concurrent runs could both decide "no existing
  # issue" and double-file). Push/PR keep the default ref-scoped group.
  group: ${{ github.workflow }}-${{ github.event_name == 'schedule' && 'schedule' || github.ref }}
  cancel-in-progress: ${{ github.ref != 'refs/heads/main' }}

jobs:
  audit:
    name: cargo-audit
    runs-on: ubuntu-24.04
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5 # v4
      # On schedule / workflow_dispatch, checkout defaults to the
      # default branch tip (main). On push, the pushed SHA. On PR,
      # the merge commit. All three are correct for an audit run.
      - uses: rustsec/audit-check@69366f33c96575abad1ee0dba8212993eecbe998 # v2.0.0
        with:
          token: ${{ secrets.GITHUB_TOKEN }}
```

### D2. `pull_request` path filter — rationale

A PR that only touches docs or CI-config doesn't change the dep
graph, so `cargo audit` is pure wasted CI time. Limit the
`pull_request` trigger to paths that can change what `cargo audit`
sees: `Cargo.toml`, `Cargo.lock`, and this workflow file itself.

Push-to-main, schedule, and workflow_dispatch have no path filter —
all three need to run unconditionally.

### D3. SHA pins (verified, not placeholder)

- `actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5` — v4, same pin as `ci.yml`.
- `rustsec/audit-check@69366f33c96575abad1ee0dba8212993eecbe998` — v2.0.0 tag, verified against `gh api repos/rustsec/audit-check/git/refs/tags/v2.0.0`.

**Pre-merge check**: confirm `gh api repos/rustsec/audit-check/git/refs/tags/v2.0.0 --jq .object.sha` still returns `69366f33c96575abad1ee0dba8212993eecbe998`. Documented as a PR-body checkbox.

## Files to touch

- `.github/workflows/audit.yml` — NEW, ~50 lines with comments.

No other files. `Cargo.toml`, `deny.toml`, and the existing `ci.yml`
are all unchanged.

## Edge cases

- **First-run noise**: zero transitive deps today. `cargo audit` on
  an empty dep tree exits 0. Verified locally. No first-PR fix-up
  needed.
- **Advisory overlap with `cargo-deny`** on a critical advisory:
  both tools fire, PR doubly-blocked. Different output formats in
  the CI log; self-documenting.
- **Yanked / unmaintained / unsound not caught by `rustsec/audit-check@v2.0.0`**:
  the action's `reporter.ts::reportCheck` only sets `failure` when
  `stats.critical > 0`. It does NOT fail on other advisory classes.
  `cargo-deny` remains the strict gate for those classes via `deny.toml`'s
  `yanked = "deny"`, `unmaintained = "all"`, `unsound = "all"`.
- **`--ignore` surgical knob**: `cargo audit --ignore RUSTSEC-####-####`
  exists, but is not wired into the action's inputs today. If a
  false-positive advisory fires, the fix is either to upgrade the
  dep or to add the `--ignore` to `.cargo/audit.toml` in the repo
  root. Empty today.
- **Schedule skew**: GitHub cron runs fire in a 5-30 minute window.
  07:23 offset avoids the top-of-hour queue. Skew is immaterial for
  "discover an advisory within 24h."
- **Issue-creation mechanism (not spam-prone, but has a subtle
  escape hatch)**: per `reporter.ts::alreadyReported`, the action
  searches `"{advisoryId} in:title repo:{owner}/{repo}"` — if **any
  issue OR PR with that advisory ID in the title already exists
  (open OR closed)**, it skips creation and makes no edit. Two
  implications:
  1. Long-lived advisories do NOT re-file every night. Good.
  2. **If an issue is closed without fixing the advisory, the next
     nightly will not re-file it.** Operators must either upgrade
     the dep or add an `.cargo/audit.toml` `ignore` entry with a
     dated removal target — closing the issue is not a valid
     "dismiss." Documented here; will be reinforced in
     `CONTRIBUTING.md` when that lands.
- **Fork PR advisory gate gap**: fork PRs get a read-only
  `GITHUB_TOKEN`, so `reporter.ts`'s check-creation falls back to
  logging (not failing) per its `GITHUB_HEAD_REF` branch. Net effect:
  **a fork PR bringing in a critical advisory will PASS CI's audit
  job, only logging findings.** The push-to-main run after merge
  catches it. Acceptable for a small-contributor attack surface;
  reinforced by `cargo-deny`'s independent gate which does fail on
  the same PR.
- **Permission scope**: `issues: write` is scoped to this workflow
  file only. `ci.yml` stays `contents: read`.
- **RustSec DB availability**: the action fetches the advisory DB
  from crates.io / GitHub on every run. If the source is down, the
  job fails closed. Correct posture.
- **Advisory content trust boundary**: advisory `description` /
  `title` / links are rendered into issue bodies via Nunjucks with
  autoescape on most fields. GitHub sanitizes issue markdown, so
  XSS is not the risk; phishing-shaped markdown links
  (`[click](evil)`) rendered in a repo-owned issue and emailed to
  watchers is. Mitigation is upstream — we trust RustSec's
  advisory-db maintainers' content moderation. This is named, not
  solved.

## Test strategy

Config-only change, but because the scheduled auto-file-issue path
is the whole reason this PR exists, the test plan must actually
exercise it — "trust the cron" is not a test.

1. **Existing jobs stay green** — `fmt` / `clippy` / `test` / `deny`
   all still pass. Workflow is additive.
2. **Workflow YAML parses** — `actionlint` (via `rhysd/actionlint`
   or local binary) catches YAML / schema errors before push. `gh
workflow list` after push shows `audit` as a registered workflow.
3. **`cargo audit` runs locally on the clean tree** — `cargo install
cargo-audit --locked` once, then `cargo audit` from workspace
   root. Exits 0.
4. **SHA pin verification** — `gh api repos/rustsec/audit-check/git/refs/tags/v2.0.0 --jq .object.sha`
   returns `69366f33c96575abad1ee0dba8212993eecbe998`. Recorded in
   the PR body.
5. **Violation-injection audit (one-off, captured verbatim in PR body)** —
   inject `time = "=0.1.45"` (explicit pin — `0.1` floats and a
   future 0.1.46 patch could break the reproducer; 0.1.45 is
   affected by RUSTSEC-2020-0071 and stable forever) into
   `crates/mango/Cargo.toml`; run BOTH gates locally:
   - `cargo audit` → must fail with RUSTSEC-2020-0071.
   - `cargo deny check advisories` → must also fail with the same
     ID (proves the two-gate overlap claimed in the plan).
   - Capture both verbatim outputs in the PR body.
   - Revert before commit.
6. **Paths-filter sanity check** — after push, a docs-only PR should
   not trigger the audit workflow. One sentence in the PR body
   noting the expected "no run" behavior for paths that don't touch
   `Cargo.*` or the workflow itself. (Or: after merge, look at the
   next docs-only PR's checks list to confirm absence.)
7. **Issue-creation path tested end-to-end post-merge** — use the
   `workflow_dispatch` trigger to manually fire a run against main
   after merge. Because clean main has no advisories, this proves
   the workflow runs green under the dispatch path; to prove the
   issue-creation path concretely, a short-lived follow-up branch
   that temporarily injects `time = "=0.1.45"` on a fork and runs
   `workflow_dispatch` there will produce a fork-local test issue.
   Document the run URL (or fork-repo run URL) in the PR body as
   evidence. This is the closest we can get to proving the
   scheduled path in a PR context without waiting 24h.
8. **CI run on the PR succeeds** — the `audit` job is green on this
   PR (clean dep tree has no critical advisories).

## Rollback

Single squash commit. Revert → `audit.yml` disappears, nightly stops,
push/PR gate stops, `workflow_dispatch` disappears. Zero runtime
impact.

## Out of scope (explicit, do not do in this PR)

- **`cargo-msrv`** — separate roadmap item (`ROADMAP.md:756`).
- **Dependabot config** — adjacent but orthogonal; future item.
- **Default-label-on-issue-creation** — the action does not apply
  labels. A follow-up (tiny second step using `actions-ecosystem/action-add-labels`
  or a GitHub repo "default labels" policy) can layer labels on
  later; not blocking this PR. Same story for issue-template hooks.
- **`--ignore` entries in `.cargo/audit.toml`** — none needed today;
  add surgically with a dated removal target when a real advisory
  fires against a dep we can't quickly swap.
- **Slack / email notification on nightly findings** — GitHub issue
  is the canonical notification; watchers are already notified on
  issue creation. Scope creep to add more channels.
- **Cross-compile advisory scan** — RustSec advisories are not
  target-specific. No equivalent to `[graph] targets` needed.
- **Making `audit` also fail PRs on unmaintained/unsound/yanked** —
  that's the cargo-deny gate's job. Don't duplicate policy.
- **ROADMAP checkbox flip** — separate commit to main per workflow.

## Risks

- **Third-party action SHA drift** — `rustsec/audit-check` pinned
  SHA must be kept current. Dependabot is the future fix.
- **Advisory DB false positive** — fire against a dep whose attack
  surface doesn't apply to mango's use. Fix: `.cargo/audit.toml`
  `ignore` with a PR-reviewed justification and dated removal
  target.
- **Closed-issue-without-fix escape hatch** — named in edge cases;
  CONTRIBUTING.md will be the durable reminder.
- **Fork-PR advisory gate gap** — named in edge cases; `cargo-deny`
  is the mitigating independent gate.
- **RustSec DB content trust** — we trust advisory-db maintainers'
  content to be safe to render into repo-owned issues. Named, not
  solved.
- **Action Node-runtime deprecation** — v2.0.0 uses Node 20. When
  GitHub sunsets Node 20, the action will produce deprecation
  warnings until a v2.0.1 or v3 ships. Tracked via future
  Dependabot; no action today.
- **Local dev UX** — contributors need `cargo install cargo-audit`
  locally to reproduce CI failures. Documented in `CONTRIBUTING.md`
  when that lands (`ROADMAP.md:761`); until then, CI log is the
  signal.
