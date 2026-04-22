# Phase 0 item 0.11 — Monotonic-clock policy

**Roadmap:** `ROADMAP.md:759`.

**Goal.** Ship a workspace doc `docs/time.md` declaring that all
protocol-relevant time math uses `std::time::Instant` (monotonic),
never `SystemTime`. `SystemTime` / wallclock is reserved for
human-facing logs, display values, and explicitly-named
cross-machine correlation cases, never for protocol decisions. The
doc is Phase 0 plumbing; no code in mango uses clocks yet. It
exists so every Raft / lease / watch / MVCC PR that lands in
Phase 2+ gets reviewed against a written rule, not a discovered
one.

## Revisions applied (post rust-expert review)

Verdict was `APPROVE_WITH_NITS`; the nits attach to the doc, not
the plan. Applying them in this revision:

- **R1 (domain list)**: cover WAL entries (timestamp-free on the
  wire), snapshot metadata (index-ordered, not time-ordered),
  watch progress cadence vs. notification-stamp split, metrics
  histograms (`Instant::elapsed()` for bucketing, Prometheus owns
  the scrape timestamp), gRPC deadline propagation (inbound
  `Duration`, not wallclock), compaction retention (revision-
  count or WAL-stored wallclock with NTP-step caveat).
- **R2 (lease failover)**: explicitly name the
  `lease-lives-up-to-2*TTL` behavior across leader failover —
  L2 cannot inherit L1's `Instant` clock, so it re-arms the
  lease from its own `Instant::now() + ttl_seconds`. Expected,
  not a bug.
- **R3 (cautionary tales)**: name ≥ 3 concrete etcd incidents in
  the "Relation to Go etcd" section — `time.Now()`-driven
  election storms pre-monotonic-reading, lease TTL surprise
  across failover, compactor-by-wallclock after NTP step, VM
  live-migration clock jumps.
- **R4 (CI test)**: drop the bespoke `scripts/test-time-policy.sh`.
  Replace with a generic `docs-lint` seam that a link-checker
  can grow into later. For **this PR**, ship no CI — the
  reviewer checklist + rust-expert diff review + future 0.15 PR
  template are the enforcement. Less moving parts rot slower.
- **R5 (enforcement handoff)**: bake the trigger directly into
  `docs/time.md` — the first Phase 2+ PR that introduces
  `Instant::now()` MUST in the same PR either (a) add a
  `clippy.toml` `disallowed-methods` entry for `SystemTime::now`
  scoped to non-display modules, (b) add a grep-based CI check,
  or (c) document why neither is viable. Concrete, testable
  handoff — not "we'll revisit."
- **Plan nits**: drop the line-count estimate (match whatever the
  content needs); pin the Phase 13 forward-reference to the
  named test `chaos-clock-skew`; put the one-sentence rule at
  the very first line of `docs/time.md` above the TL;DR table.

## North-star axis moved

**Correctness under clock perturbation** — wallclock jumps (NTP
step, admin `date -s`, VM host-clock drift, VM live migration,
leap second) must not reorder Raft events, expire leases early,
or skew MVCC revisions. Declaring the rule now prevents the
"oh, we used `SystemTime::now` for the election deadline" class
of bug that would only surface in a chaos test months later.

## Out of scope

- **Enforcement lint** today (deferred to the first Phase 2+
  `Instant` PR, with the trigger named in the doc itself — see
  R5 above). Adding a workspace-wide `disallowed-methods` ban on
  `SystemTime` now would fire on zero code.
- **`chrono` / `jiff` / `time` crate selection.** That's a
  separate decision; the policy is agnostic. `SystemTime`
  wrappers from any of those three count as "wallclock" under
  the policy.
- **Leap-second ingestion strategy.** Policy says "N/A" because
  `Instant` is by definition leap-second-free; wallclock display
  inherits whatever the OS does.
- **NTP chaos test itself.** That's Phase 13's `chaos-clock-skew`
  test (`ROADMAP.md:1103` fault injector, clock-skew line). The
  doc forward-links to the test name without implementing it.

## Non-goals

- No code changes. `crates/mango/src/lib.rs` stays empty.
- No CONTRIBUTING.md link yet. Item 0.14 adds CONTRIBUTING.md and
  will link `docs/time.md` then. Item 0.15 (PR template) adds
  the reviewer-checkbox that references this doc.

## Files

- `docs/time.md` — NEW. Policy doc, shape modelled after
  `docs/arithmetic-policy.md`:
  - **One-sentence rule on line 1.** "Protocol-relevant time uses
    `Instant`. `SystemTime` is for display, never for decisions."
    No preamble before it.
  - TL;DR table: domain → clock → rationale tag.
  - Why-each section covering every domain surfaced in R1 above,
    plus: Raft election timers, lease expiry server-side, lease
    TTL client display, watch progress cadence and notification
    stamp (split), MVCC revision timestamps (stored-`Instant`
    vs. WAL-stored-wallclock tradeoff for compaction), request
    deadlines at the gRPC boundary, metrics / histograms, log
    records (tracing subscriber timestamps as an allowed
    wallclock caller), tests and examples.
  - **Lease failover section** — calls out the `2 * TTL`
    behavior: lease state is replicated as
    `(lease_id, ttl_seconds, granted_at_revision)`; a new leader
    on election rebuilds expiry as `Instant::now() +
ttl_seconds`. A lease can effectively live up to `2 * TTL`
    across a failover. This is by design and matches Go etcd.
  - Named escape hatches (structured logging / tracing,
    TTL display in gRPC response, human-facing CLI output,
    filesystem mtime for human snapshot/retention display).
    Every call site in an allowed domain carries a one-word
    comment `// wallclock: display` to make the audit
    mechanical.
  - Reviewer checklist (5-7 items): Raft / lease / watch /
    metrics / gRPC deadlines / snapshot retention / new
    `SystemTime` callers.
  - Enforcement handoff (R5): the first Phase 2+ time PR MUST
    add the disallowed-methods entry or document why not.
  - Phase 13 forward-link by name: `chaos-clock-skew` test at
    `ROADMAP.md:1103`.
  - Relation to Go etcd — at least three concrete tales:
    pre-monotonic-reading `time.Now()` election storms, lease
    TTL surprise across failover, wallclock-driven compactor on
    NTP step, VM live-migration clock jumps. Cite etcd issue
    numbers by bug class (not exact numbers we can't verify —
    "multiple reports" language when not pinning).
- `docs/arithmetic-policy.md` — EDIT. One-line cross-link from
  the deadline-math section (`arithmetic-policy.md:80-95`) to
  `docs/time.md`. Deadline arithmetic is the overlap domain;
  the two policies reference each other.

## Test strategy

This is a doc-only PR. Per R4, no bespoke CI script. Enforcement
is:

1. **rust-expert review on the PR diff** — the correctness gate
   for the wording, escape-hatch list, and enforcement handoff.
2. **Reviewer checklist in the doc itself.** Item 0.15 (PR
   template) will copy the relevant checkbox forward so every
   subsequent PR touching `Instant` or `SystemTime` hits it.
3. **Local pass on markdown**: `prettier --check` if available,
   otherwise visual spot-check of the table and code blocks.
4. **Cross-link smoke**: the edit to `docs/arithmetic-policy.md`
   actually renders as a link (spot-check on GitHub).

No CI job added. The `docs-lint` seam is left as a future item
for when there are 3+ docs and a link-checker pays back its
complexity.

**Why this is enough.** The arithmetic-policy PR (0.4) shipped
with the same zero-CI posture and the policy has held. Adding
CI for a one-doc PR is over-fitting.

## Plan of work

1. Branch: `docs/monotonic-clock-policy`.
2. Write `docs/time.md` covering every domain in R1, the
   lease-failover section (R2), the Go-etcd tales (R3), and the
   enforcement handoff (R5).
3. Add the cross-link from `docs/arithmetic-policy.md` →
   `docs/time.md`.
4. Push branch. Open PR.
5. rust-expert review on `gh pr diff` output.
6. Revise on review; re-request APPROVE.
7. Merge with `--squash --delete-branch`.
8. Flip `ROADMAP.md:759` checkbox on main.

## Rollback plan

Doc-only PR. Revert is `git revert`; no migrations, no runtime
impact, no downstream breakage.

## Risks

- **Policy is premature** — we have no `Instant` or `SystemTime`
  in the tree yet. Mitigated by keeping the doc grounded in
  concrete Raft / lease / watch / MVCC examples that Phase 2+
  will hit, not abstract principles. The enforcement handoff
  (R5) defers the lint to where it bites, not where it doesn't.
- **Scope creep into NTP / chrony / clock-sync choice.** The
  out-of-scope list is load-bearing — defer them and stop. A
  reviewer who pushes on ntp-chaos belongs in the 0.15 PR
  template or Phase 13, not here.
- **Wording drift vs. `docs/arithmetic-policy.md`.** The TL;DR
  table, reviewer checklist, and Relation-to-Go-etcd structure
  are copied on purpose. Diff review will spot-check alignment.
- **"Relation to Go etcd" tales get sloppy.** The doc names bug
  classes, not exact issue numbers, to avoid citing numbers we
  can't verify. Reviewer enforces that the named behaviors are
  actually documented in etcd (failover 2×TTL and
  NTP-driven-compaction are well-known; the pre-monotonic-
  reading Go 1.9 era is Go-wide, not just etcd).

## Acceptance

- `docs/time.md` exists with the one-sentence rule on line 1,
  the TL;DR table, every domain in R1 covered, the lease
  failover section, the enforcement handoff, and the Phase 13
  forward-link by name.
- `docs/arithmetic-policy.md` gains a one-line cross-link to
  `docs/time.md` from its deadline-math section.
- rust-expert APPROVE on the diff.
- ROADMAP.md item 0.11 checkbox flipped on merge.
