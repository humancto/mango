# Phase 0 item 0.12 — Crash-only design declaration

**Roadmap:** `ROADMAP.md:760`.

**Goal.** Ship a workspace doc `docs/architecture/crash-only.md`
declaring: storage and server layers assume the process can be
killed at any instant; clean shutdown is an optimization, never a
correctness requirement. WAL-then-apply ordering and the
`data-dir/VERSION` recovery (Phase 12) make process restart
equivalent to crash recovery. Every storage / Raft PR must satisfy
"this would also be correct if killed at any point." The doc is
Phase 0 plumbing; no storage code exists yet. It exists so every
Phase 1 storage-engine ADR and every Phase 2+ WAL/apply/snapshot
PR gets reviewed against a written rule, not discovered by the
first broken-recovery bug.

## Revisions applied (post rust-expert reviews of plan v1 & v2)

v1 verdict: `REVISE` — 10 issues (R1-R10), applied in v2 below.
v2 verdict: `APPROVE_WITH_NITS` — 7 textual nits (N1-N7), applied
in this v3 alongside the R-items. All nits are doc-wording
changes, no structural rework.

### v2 nits applied (N1-N7)

- **N1 — enforcement handoff mirrors `docs/time.md` (a)/(b)/(c)
  verbatim**. R6 named `rust-expert` as the reviewer's-contract
  enforcer; N1 binds the first PR that introduces persistent
  on-disk state to land either a grep-CI check for the PR-body
  `Crash-safety:` header or a named follow-up, per the same
  (a)/(b)/(c) structure as `docs/time.md:241-271`. "Continue per
  R6" is not acceptable without a named follow-up PR.
- **N2 — narrow R3 apply-idempotency to WAL-applied ops**. Lease
  `KeepAlive` and Watch progress attach/detach are **leader-local
  timer** / subscription state, not replay-applied. The doc
  enumerates `Put`, `Delete`, `Txn` commit, `LeaseGrant`,
  `LeaseRevoke` under the idempotency rule, and explicitly notes
  `KeepAlive` / watch-progress-attach as leader-local, governed
  by 2×TTL (lease) and re-subscribe (watch), not by replay.
- **N3 — add TL;DR row for gRPC request dedup**. Row:
  "gRPC request in-flight on crash" → invariant "at-least-once
  to the client; server-side dedupe by client request-id keeps
  re-submission idempotent" → mechanism "client-supplied
  request-id + dedupe window (Go etcd `RequestHeader.ID`
  pattern)." Covers the crash-between-WAL-and-ack case.
- **N4 — anti-patterns 11 & 12 added; item 9 reworded**.
  Item 11: "ignoring or retrying `fsync` `EIO` without treating
  the file as poisoned — see PostgreSQL fsyncgate 2018; Linux
  `fsync` clears the dirty bit on `EIO`, so retries silently
  succeed against lost data." Item 12: "reading WAL / snapshot
  data before validating its length-prefix and CRC tail;
  use-then-verify instead of verify-then-use." Item 9 reworded
  from "learner/observer reading past commit index" to
  "applying uncommitted log entries to the state machine"
  (learners legitimately **receive** uncommitted entries in
  raft-rs; they just must not **apply** them).
- **N5 — lease invariant tightened**. From "client never holds a
  lease the cluster has forgotten" to "**client never believes
  it holds** a lease the cluster has forgotten." Belief is the
  load-bearing part; retryable-error recovery protects belief.
- **N6 — escape hatches 5 & 6 added**. 5: Raft apply-queue drain
  on clean shutdown (WAL-appended-but-not-applied entries
  finish applying before exit, so restart doesn't re-do visible
  work). 6: unary RPC connection drain (in-flight Put / Txn
  calls complete before SIGTERM, matching the Watch-stream
  drain pattern). Both correctness-preserving; both
  politeness-optimizations.
- **N7 — R3 wording softened to "WAL-applied state-machine op"**
  to match N2's narrowed enumeration. Avoids the doc over-claiming
  idempotency for leader-local state.

The plan fights the same failure class as v1 of `docs/time.md`:
the doc must be usable as a review gate, not a statement of
principles.

- **R1 (Phase 13 knobs)**: `ROADMAP.md:1103` lists literally: "drop
  / delay / duplicate / reorder messages, kill processes
  mid-fsync, partition the network with one-way / asymmetric /
  flaky links, corrupt individual disk pages, return `EIO` from
  any syscall, clock skew between nodes." Only `kill mid-fsync`
  is a literal kill-point knob. The doc references the **literal
  ROADMAP:1103 knobs** by name (`kill-mid-fsync`, `EIO injection`,
  `disk-page corruption`) and then **in the same PR** edits
  `ROADMAP.md:1103` to broaden the kill-point knob to
  "kill processes mid-fsync / mid-snapshot-install /
  mid-leader-elect / mid-compaction" so the vocabulary the doc
  uses is load-bearing on Phase 13, not invented.

- **R2 (anti-pattern list)**: v1 missed the six most common
  production-etcd crash-safety footguns. v2 enumerates: (i)
  multi-file atomicity without a rename barrier; (ii) mmap writes
  under `kill -9` (MS_SYNC / msync ordering vs. fsync); (iii)
  `data-dir/VERSION` mid-migration torn state (write-new then
  rename-over, never partial); (iv) DB transaction held across a
  gRPC / `await` boundary (yielded futures get cancelled on
  process kill without transaction rollback guarantees); (v)
  "atomic batch" implemented as N independent writes with no
  commit record; (vi) learner/observer caught up to an index that
  is not yet committed on the leader (replicating from the Raft
  log before the commit barrier).

- **R3 (apply-idempotency on replay)**: the single most commonly
  violated Raft crash-only invariant is state-machine apply not
  being idempotent on WAL replay. A process that crashes _after_
  appending entries N..M to WAL but _before_ all of them are
  applied to the state machine must, on restart, re-apply the
  un-applied tail. Every state-machine operation (`Put`,
  `Delete`, `Txn`, lease `Grant`/`Revoke`/`KeepAlive`, watch
  progress attach/detach) MUST be idempotent under re-apply. v2
  makes this a first-class section and the reviewer's-contract
  triage marker.

- **R4 (lease pre-commit vs post-commit crash)**: the 2×TTL
  behavior in `docs/time.md` covers post-commit-crash (grant is
  replicated, new leader re-arms). The crash-only doc names the
  **pre-commit-crash** case distinctly: leader accepts `Grant`
  RPC → appends WAL entry → crashes before the entry is committed
  (replicated to a quorum). On restart / failover, the entry is
  either truncated (minority) or committed (majority). If
  truncated, the client's `Grant` RPC fails with a retryable
  error; the client re-grants. This is correct, but it must be
  _documented_ or it looks like a bug. v2 names both cases with a
  marker invariant: "client never holds a lease the cluster has
  forgotten."

- **R5 (FS / NVMe write-cache punt)**: crash-safety leans on
  `fsync()` actually durably persisting. Journalling FS (`ext4`
  default `data=ordered`, `xfs`) gives us the metadata/data
  ordering primitive; NVMe write-cache behavior under power loss
  is a hardware concern. v2 adds one sentence: "durability
  primitives (fsync cadence, O_DIRECT, write-barrier) are named
  by the Phase 1 storage-engine fsync ADR
  (`ROADMAP.md:~820-840`); this doc treats fsync as a boundary,
  not a mechanism."

- **R6 (reviewer's contract enforcement)**: v1 said "missing
  section is a REVISE verdict." Theater without a named enforcer.
  v2 names `rust-expert` as the reviewer agent that runs on every
  storage / Raft PR's diff, and makes the reviewer's-contract
  check a checklist item in that agent's review. Item 0.15 (PR
  template) lifts the "Crash-safety:" section into the template
  body — missing it is visible at PR-open time.

- **R7 (`docs/time.md` cross-link → subsection)**: v1 planned a
  one-line cross-link. v2 plans a **"Crash-only interaction"
  subsection** inside `docs/time.md`'s lease section that covers
  the pre-commit-crash lease case (R4) — that case is simultaneously
  a time-policy and crash-only concern; mentioning it in one doc
  only means the other doc's reader misses it.

- **R8 (Go etcd tales — 6 classes)**: v1 named 3. v2 names 6:
  (i) bbolt freelist corruption on mid-transaction crash,
  (ii) WAL CRC + data-fsync split (etcd #10346-family: CRC
  written but data not durable), (iii) leader re-election losing
  writes acknowledged by a minority, (iv) snapshot-apply torn on
  crash mid-install (temp-dir leftover), (v) `panic` during apply
  after the log entry is WAL-durable (re-panics on every restart
  — recovery wedged), (vi) lease revoke raced with client
  KeepAlive across failover (pre-commit crash per R4).

- **R9 (TL;DR rows drafted)**: v1 named domains abstractly. v2
  commits the table rows in the plan so the reviewer-on-diff can
  verify the draft matches. Concrete example rows in the "Files"
  section below.

- **R10 (escape hatches — data-dir unlock & gRPC stream drain)**:
  v1 named `mangoctl defrag` / `mangoctl snapshot save`. v2
  adds: data-dir lockfile release on clean shutdown (load-bearing
  for operator re-start on same host; process crash leaves stale
  lockfile, recovery must detect+steal by PID-check); gRPC
  bidi-stream drain for Watch subscribers (kill -9 drops the
  stream mid-frame, clients auto-reconnect — no in-flight event
  loss because Watch events are durable in the WAL, but clean
  drain is a politeness optimization).

- **Nits**: phase precision in forward-references ("Phase 2+
  compactor" → "Phase 6+ compactor/snapshot per
  `ROADMAP.md:~895-910`"); PR-template cross-reference calls
  forward to item 0.15 by line number (`ROADMAP.md:763`);
  enforcement-handoff section mirrors the structure of
  `docs/time.md:241-271` (three-option a/b/c); grep-auditable
  comment convention `// crash-safety: shutdown-required —
<reason>` at every call site where clean shutdown _is_
  load-bearing (the escape-hatch list).

## North-star axis moved

**Reliability under process kill.** Every production etcd operator
has SIGKILL'd a node at some point. "Crash-only" means recovery
from `kill -9` is the _primary_ lifecycle path, not a degraded
mode; clean shutdown is the _same path with an optimization_
(drain in-flight Raft apply, flush WAL, unlock data dir), never a
distinct code path with its own invariants. The doc pre-commits
the design team to this discipline before the first storage PR.

## Out of scope

- **`data-dir/VERSION` file format itself.** Phase 12
  (`ROADMAP.md:1080`). The policy forward-references it as the
  enforcement boundary for on-disk format versioning; it does not
  specify bytes or migration protocol. Policy **does** require
  that mid-migration torn state is invisible (rename-over barrier).
- **Specific storage engine choice** (sled / redb / rocksdb /
  hand-rolled). Phase 1. Policy is engine-agnostic.
- **`fsync()` cadence / O_DIRECT / write-barrier.** Phase 1 fsync
  ADR territory (`ROADMAP.md:~820-840`). Policy names fsync as a
  _boundary_ (post-fsync durable, pre-fsync lost); it does not
  pick the cadence.
- **Jepsen-scale chaos testing.** Phase 13 / 13.5. Policy
  forward-links to `ROADMAP.md:1103` knob names only.
- **Snapshot-install atomicity protocol.** Phase 6+ compactor /
  snapshot RPC (`ROADMAP.md:~895-910`). Policy names the invariant
  (partial snapshot files must never be readable as complete); the
  RPC protocol is out of scope.
- **systemd / init-script shutdown semantics.** Ops concern for
  Phase 12. Policy says mango does not rely on SIGTERM-before-
  SIGKILL grace periods for correctness.
- **NVMe write-cache / journalling-FS choice** — hardware /
  deployment concern. Policy relies on the Phase 1 fsync ADR to
  name the primitive and on operator docs (Phase 12) to name the
  supported FS configurations.

## Non-goals

- No code changes. `crates/mango/src/lib.rs` stays empty.
- No `CONTRIBUTING.md` link yet (item 0.14 wires it; `ROADMAP.md:762`).
- No PR-template checkbox yet (item 0.15 wires it; `ROADMAP.md:763`).
- No enforcement lint. Crash-only is a design-review property,
  not something a compiler catches. The reviewer's contract (R6)
  is the enforcement.

## Files

- `docs/architecture/crash-only.md` — NEW. Policy doc, shape
  modelled after `docs/time.md`:
  - **One-sentence rule on line 1, above any heading.**
    `kill -9 at any instant is a supported lifecycle event; clean shutdown is an optimization, not a correctness boundary.`

  - **TL;DR table**: domain → crash-safety invariant → enforcing
    mechanism. Drafted rows (plan commits to this shape so
    reviewer-on-diff can verify):

    | Domain                        | Invariant                                                  | Mechanism                                     |
    | ----------------------------- | ---------------------------------------------------------- | --------------------------------------------- |
    | WAL append                    | fsync-before-ack; partial appends invisible after recovery | fsync + length-prefixed entry with CRC        |
    | State-machine apply           | Idempotent on re-apply of already-applied entries          | Entry index check in apply loop               |
    | Raft-log vs. apply durability | WAL-durable before apply; apply-cursor is recoverable      | `applied_index` in durable state              |
    | Snapshot install              | Partial snapshot files never readable as "complete"        | `.tmp` + fsync + rename                       |
    | `data-dir/VERSION`            | Mid-migration torn state invisible                         | Write-new, fsync, rename-over, fsync parent   |
    | MVCC index                    | Rebuilt from WAL on start; never the source of truth       | In-memory only; WAL is authoritative          |
    | Lease state                   | Replicated via Raft; never single-node durable             | Lease grants go through WAL like any other op |
    | Multi-file atomicity          | Group of files either all visible or none                  | Rename-barrier file or commit record          |
    | In-memory caches              | Rebuilt from durable state on restart                      | No dirty-flag; no shutdown-flush path         |

  - **Why-each section** (one short block per domain above plus
    the three below, named failure mode first, mechanism second):
    - WAL write ordering (fsync-before-ack; partial-entry CRC
      invisible).
    - State-machine apply cadence (WAL-persist-before-apply
      ordering; `applied_index` durable).
    - **Apply-idempotency on replay** (R3) — first-class
      section. Every state-machine op (`Put`, `Delete`, `Txn`
      commit, lease `Grant`/`Revoke`/`KeepAlive`, watch progress
      attach/detach) MUST be idempotent under re-apply. Concrete
      triage question for the reviewer: "if this entry is
      applied twice, does the second application change
      observable state?"
    - Snapshot atomicity (`.tmp` + fsync + rename + parent-dir
      fsync).
    - `data-dir/VERSION` recovery — refuses unknown format,
      forward-migrates on operator command, migration is
      torn-state-invisible.
    - MVCC index rebuild from WAL on start.
    - **Lease crash semantics** (R4) — pre-commit-crash and
      post-commit-crash as distinct cases. Pre-commit: client's
      `Grant` fails retryably; cluster forgets the grant.
      Post-commit: new leader re-arms up to 2×TTL (links to
      `docs/time.md`). Invariant: **client never holds a lease
      the cluster has forgotten.**
    - Raft-log vs. state-machine-apply durability boundary —
      where the responsibility for crash recovery moves from
      Raft to the state machine.
    - Clean-shutdown-as-optimization — explicit non-requirement.

  - **Named anti-patterns** (R2). Don't write these; they'll be
    caught in review:
    1. In-memory dirty flags that only flush on shutdown.
    2. `Drop` impls that perform I/O.
    3. Temp-file without parent-directory fsync after rename.
    4. Multi-file atomic operations without a rename-barrier or
       commit record.
    5. `mmap`-written pages without `msync(MS_SYNC)` before
       `fsync()` of the backing file.
    6. `data-dir/VERSION` rewrite that overwrites in place (must
       be write-new + rename-over + parent fsync).
    7. DB transaction held across a gRPC / `await` boundary.
    8. "Atomic batch" implemented as N independent writes with
       no commit record.
    9. Learner / observer reading a Raft-log entry past the
       commit index.
    10. Any test asserting "after clean shutdown the data looks
        like X" without the `kill -9`-first variant.

  - **Reviewer's contract** (R6). Every storage / Raft PR must
    have a PR-description section titled `Crash-safety` with one
    of three markers:
    1. `kill-safe by construction — <invariant>`
    2. `kill-safe + test: <test name/module>`
    3. `not kill-safe yet — follow-up #<N>, lands before <phase-gate>`

    The `rust-expert` reviewer agent (the same one the workflow
    spawns on every PR) enforces this marker as a checklist
    item; a missing or hand-wavy section is a `REVISE` verdict.
    Item 0.15 (PR template; `ROADMAP.md:763`) will copy the
    `Crash-safety:` header into the template so authors hit it
    at PR-open time.

  - **Enforcement handoff** — mirrors `docs/time.md:241-271`.
    This doc has no Rust-level lint (crash-safety is a
    design-review property). The enforcement IS the reviewer's
    contract. If the first Phase 1+ storage PR wants a
    stricter mechanism — e.g., a required `crash-safety:` TOML
    entry in a PR-metadata file, or a grep CI check for the
    header — that PR adds it in the same PR. Otherwise,
    review-gate enforcement continues per R6.

  - **Grep-auditable comment convention** (final nit). Every
    call site where clean shutdown _is_ load-bearing (the
    escape-hatch list below) carries the comment:

    ```rust
    // crash-safety: shutdown-required — <one-line reason>
    ```

    So `rg 'crash-safety: shutdown-required'` enumerates every
    place we opted out. No such comment → implicit kill-safe.

  - **Escape hatches** (R10) — places where clean shutdown IS
    load-bearing:
    1. `mangoctl defrag` — online defrag of the data-dir; must
       be transactional-on-disk or documented "run while
       quiesced."
    2. `mangoctl snapshot save` — user-requested snapshot copy
       to a target path; quiesce or transactional.
    3. Data-dir lockfile release — clean shutdown removes
       `LOCK`; process kill leaves a stale lockfile. Recovery
       on the same host MUST PID-check and steal the stale
       lock, not refuse to start.
    4. gRPC bidi Watch stream drain — clean shutdown closes
       streams politely; kill -9 drops mid-frame. Correctness
       is preserved (Watch events are WAL-durable; clients
       reconnect and resume), so "drain" is a politeness
       optimization, not a correctness primitive.

  - **Phase 13 chaos-test forward-reference** (R1). Names the
    ROADMAP:1103 knobs directly. Same-PR ROADMAP:1103 edit
    broadens the kill-point knob language so the doc's
    "kill-mid-snapshot / kill-mid-leader-elect / kill-mid-
    compaction" scenarios are load-bearing on a named
    fault-injector knob, not invented vocabulary.

  - **Cross-links**:
    - `docs/time.md` — lease crash semantics live in both docs;
      `docs/time.md` gets a "Crash-only interaction" subsection
      (R7) covering pre-commit-crash distinct from the 2×TTL
      post-commit case.
    - `docs/arithmetic-policy.md` — WAL indices are protocol
      counters; crash-only and overflow semantics are
      orthogonal but both named.

  - **Relation to Go etcd** (R8) — six concrete bug classes:
    1. bbolt freelist corruption on mid-transaction crash (the
       original motivator for etcd's "auto-compaction + consistency
       check" plumbing).
    2. WAL CRC + data-fsync split (etcd #10346-family: CRC
       written before the data was durable).
    3. Leader re-election losing writes acknowledged by a
       minority but not durable before the minority lost
       leadership.
    4. Snapshot-apply torn on crash mid-install (leftover
       `snap.db.part` in `snap/` forcing manual cleanup).
    5. `panic` during apply after the log entry is WAL-durable
       (every restart re-panics on the same entry — wedged).
    6. Lease revoke raced with client KeepAlive across failover
       (the pre-commit-crash case from R4).

    Cited as **bug classes, not exact issue numbers**. The doc
    names the class and the shape; an operator can file a bug
    against mango if they see the shape recur.

  - **Maintenance clause** — any PR introducing a new
    crash-safety domain MUST update this doc in the same PR.
    Reviewer enforces. Linked from `CONTRIBUTING.md` (0.14) and
    the PR template (0.15).

- `docs/time.md` — EDIT. Add a **"Crash-only interaction"**
  subsection under the existing `Lease expiry — server-side`
  block (near `docs/time.md:55`) covering the R4 pre-commit-
  crash case and cross-linking to `docs/architecture/crash-only.md`.
  Not a one-liner.

- `ROADMAP.md:1103` — EDIT. Same-PR edit. Broaden "kill processes
  mid-fsync" → "kill processes mid-fsync / mid-snapshot-install
  / mid-leader-elect / mid-compaction". The added knob names
  match the vocabulary the crash-only doc uses; reviewers in
  Phase 13 find the vocabulary already committed to.

- `docs/arithmetic-policy.md` — no edit. Already cross-links
  `docs/time.md`; crash-only is orthogonal.

## Test strategy

Doc-only PR plus a one-line ROADMAP edit. Same posture as 0.11:

1. **`rust-expert` adversarial review on the PR diff**. The
   correctness gate for wording, the reviewer's-contract
   structure, the anti-pattern enumeration, and the Phase 13
   forward-reference alignment with the same-PR ROADMAP:1103
   edit.
2. **Reviewer checklist in the doc itself.** Every storage /
   Raft PR from Phase 1 onward gets reviewed against it.
3. **Cross-link smoke**: edits to `docs/time.md` and
   `ROADMAP.md:1103` render as expected on GitHub PR diff.

No bespoke `scripts/test-*.sh` — per the 0.11 REVISE learning,
one-off shell scripts for doc-shape assertions rot. Reviewer is
the enforcement.

**Why no CI job**: policy doc. The arithmetic-policy (0.4) and
time-policy (0.11) shipped zero-CI and both hold. Adding CI for
one doc is over-fitting.

## What the doc must get right

Lessons from 0.4 / 0.11 applied:

- **One-sentence rule on line 1**, before any heading.
- **Name concrete failure modes first, mechanisms second.**
  "If a WAL entry's CRC is fsynced before its data, a kill
  between the two makes a partial entry look valid on replay."
  Failure-first.
- **Apply-idempotency as its own section** (R3). Buried under
  "state-machine apply cadence" in v1; promoted in v2.
- **Enumerate anti-patterns.** Review is mechanical with a
  list; philosophical without one.
- **Reviewer's contract is the load-bearing artifact** (R6).
  Three markers (kill-safe / test / follow-up#N) or REVISE.
- **Link forward to chaos tests by literal ROADMAP knob name**
  (R1). Same-PR ROADMAP edit earns the right to invent
  knob-name vocabulary.
- **Cite ≥ 6 Go etcd bug classes** (R8) — "we've thought about
  how this breaks," not "we'll be careful."
- **TL;DR rows drafted in the plan** (R9) — reviewer on diff can
  verify the table matches; no surprises at review time.

## Plan of work

1. Branch: `docs/crash-only-declaration`.
2. `mkdir -p docs/architecture/` (first doc there).
3. Write `docs/architecture/crash-only.md` with the TL;DR rows,
   why-each (including apply-idempotency as its own section and
   lease pre/post-commit-crash), anti-patterns, reviewer's
   contract with `rust-expert` named, enforcement handoff,
   grep-auditable comment convention, escape hatches (with
   data-dir unlock + stream drain), chaos-test forward-refs by
   ROADMAP:1103 knob name, cross-links, 6 Go etcd tales,
   maintenance clause.
4. Add the "Crash-only interaction" subsection to `docs/time.md`
   under the lease-expiry section.
5. Edit `ROADMAP.md:1103` to broaden the kill-point knob.
6. Push branch. Open PR with the reviewer's-contract excerpt
   quoted in the PR body so the review has the right focus.
7. `rust-expert` adversarial review on `gh pr diff`.
8. Revise on review. Re-request `APPROVE`.
9. Merge `--squash --delete-branch` on `APPROVE`.
10. Flip `ROADMAP.md:760` checkbox on main; commit + push.

## Rollback plan

Doc-only PR plus a one-line ROADMAP edit. Revert is `git revert`;
no migrations, no runtime impact, no downstream breakage. The
ROADMAP:1103 knob-name broadening is additive — reverting it
reverts nothing Phase 13 was relying on.

## Risks

- **Policy is premature** — no storage code yet. Mitigated by
  grounding in concrete Phase 1+ surfaces (WAL append, apply,
  snapshot, VERSION, lease, multi-file) with named anti-patterns
  and drafted TL;DR rows, not abstractions. If Phase 1 discovers
  a crash-safety domain the doc doesn't cover, it's updated in
  the same PR (maintenance clause).
- **Reviewer's contract only as strong as review discipline.**
  Mitigated by (a) naming `rust-expert` explicitly as the
  enforcer, (b) 0.15 PR template lifting `Crash-safety:` into
  the template body so missing it is visible at PR-open time,
  (c) the three-marker wording forcing an explicit statement
  rather than silence.
- **Wording drift vs. `docs/time.md` / `docs/arithmetic-policy.md`.**
  Shared shape: one-sentence rule line 1, TL;DR table, why-each,
  reviewer checklist, Relation-to-Go-etcd, maintenance clause,
  enforcement handoff mirroring time.md's a/b/c shape.
- **Scope creep into fsync-cadence / engine-choice debate.**
  "Out of scope" list is load-bearing. Reviewers who push on
  fsync cadence belong in the Phase 1 fsync ADR.
- **Go-etcd tales get sloppy.** Named as bug _classes_, not
  issue numbers, so the doc ages well. Reviewer enforces that
  each named class is a real etcd-production-incident pattern,
  not invented.
- **Same-PR ROADMAP:1103 edit surprises a future Phase 13
  author.** Mitigated by keeping the edit additive and by the
  edit reading naturally — the four named kill-points are
  obvious extensions of "kill processes mid-fsync."

## Acceptance

- `docs/architecture/crash-only.md` exists with: the one-sentence
  rule on line 1; the drafted TL;DR table; apply-idempotency as
  its own why-each section; ten named anti-patterns; reviewer's
  contract with three-marker wording and `rust-expert` named;
  enforcement handoff mirroring `docs/time.md:241-271`;
  grep-auditable comment convention; four escape hatches
  including data-dir unlock and gRPC stream drain; Phase 13
  chaos-test forward-reference by ROADMAP:1103 knob name; six
  named Go-etcd cautionary bug classes; maintenance clause.
- `docs/time.md` gains a "Crash-only interaction" subsection
  under the lease-expiry block covering pre-commit-crash
  distinct from 2×TTL.
- `ROADMAP.md:1103` gains the broadened kill-point-knob wording.
- `rust-expert` `APPROVE` on the diff.
- `ROADMAP.md:760` checkbox flipped on merge.
