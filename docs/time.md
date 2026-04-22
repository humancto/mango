**Protocol-relevant time uses `Instant`. `SystemTime` is for display, never for decisions.**

# Monotonic-clock policy

This document is the one-true-way to do time math in mango. Wallclock
jumps (NTP step, admin `date -s`, VM live migration, leap second)
must not reorder Raft events, expire leases early, or skew MVCC
revisions. The rule above — and the table below — exist so every
Raft / lease / watch / MVCC PR gets reviewed against a written
policy, not a discovered one.

No mango code uses clocks yet (Phase 0 is pre-code). This doc ships
the rule _before_ the first `Instant::now()` call so the first time
PR has a policy to conform to, not to invent.

## TL;DR — which clock for which domain?

| Domain                                             | Use                               | Rationale tag          |
| -------------------------------------------------- | --------------------------------- | ---------------------- |
| Raft election timers, heartbeats, commit deadlines | `Instant` + `Duration`            | Protocol decision      |
| Lease expiry **server-side**                       | `Instant::now() + Duration`       | Protocol decision      |
| Lease TTL on the wire (Grant, KeepAlive response)  | `Duration` (seconds-remaining)    | Protocol wire format   |
| Lease "expires at <wallclock>" in gRPC response    | `SystemTime` (display only)       | Display                |
| Watch progress **cadence** (interval between)      | `Instant` + `Duration`            | Protocol decision      |
| Watch notification **stamp** (if any) for client   | `SystemTime` (display only)       | Display                |
| MVCC revision ordering                             | revision number, not clocks       | Wire-level total order |
| MVCC revision creation timestamp (in-memory)       | `Instant`                         | Local ordering         |
| Compaction retention "older than N hours"          | See "Compaction retention" below  | Mixed                  |
| gRPC inbound deadline propagation                  | `Duration` → `Instant::now() + d` | Protocol decision      |
| Request-latency metrics / histograms               | `Instant::elapsed()`              | Duration measurement   |
| Prometheus scrape timestamp                        | Owned by Prometheus               | Not our concern        |
| Structured logging / tracing timestamps            | `SystemTime` (display only)       | Display / correlation  |
| Snapshot metadata ordering                         | snapshot **index**, not time      | Wire-level total order |
| Snapshot file retention human display              | filesystem `mtime` (wallclock)    | Display                |
| WAL entry wire format                              | No timestamp on the wire          | Wire format rule       |
| CLI output, error `Status::details` wallclock      | `SystemTime` (display only)       | Display                |
| Tests / examples                                   | Whatever reads cleanest           | Allowed in test mod    |

If in doubt, **use `Instant`.** Wallclock on a code path that
influences any decision at all — including "which of these two
things happened first" — is a bug.

## Why each — the rationale

### Raft election timers, heartbeats, commit deadlines

`Instant` ordering drives election timeout dispatch, follower
heartbeat detection, and commit timeouts. A wrapped or stepped
wallclock deadline silently reorders elections — exactly the shape
of a correctness bug we cannot debug from logs. Build deadlines as
`Instant::now().checked_add(timeout)` (see
[docs/arithmetic-policy.md](arithmetic-policy.md)'s deadline-math
section) and compare `Instant`s directly.

### Lease expiry — server-side

Leader `L1` holding a lease stores its expiry as an `Instant` — a
value meaningful only inside `L1`'s process. Lease state _on the
wire_ is `(lease_id, ttl_seconds, granted_at_revision)`. The
`Instant` never leaves `L1`.

When `L1` fails and `L2` takes over, `L2` has its own `Instant`
clock with a different zero. `L2` rebuilds each active lease's
expiry on leadership acquisition as `Instant::now() + ttl_seconds`.
This means **a lease can effectively live up to `2 * ttl_seconds`
across a failover**: once nearly full-TTL under `L1`, then re-armed
for another full TTL under `L2`.

This is **by design** and matches Go etcd. Do not "fix" it by
persisting wallclock expiry across leaders — that would trade a
benign upper-bound for a clock-skew correctness bug. Client TTL
precision is bounded below by leadership-churn frequency, not by
`Instant` resolution.

### Lease TTL — wire and display split

Wire: `ttl_seconds` as a `Duration`-shaped integer. Protocol.
Server-side expiry: `Instant::now() + ttl_seconds`. Protocol.
Client-facing "expires at <wallclock>": `SystemTime`. **Display
only.** A gRPC response including `expires_at: 2026-04-23T18:00Z`
is honest display; the server never read that value back for a
decision.

The three are separate fields — mixing them (e.g., persisting an
`expires_at: SystemTime` to storage and computing expiry by
subtracting from `SystemTime::now()`) is the ban.

### Watch progress — cadence vs. notification stamp

The **interval** between watch progress notifications is a
`Duration` driven off `Instant`:

```rust
if last_progress.elapsed() >= progress_interval { send_progress(); }
```

The **notification itself** MAY carry a wallclock field for client
display ("server saw this at 2026-04-23T18:00Z"). That's fine. The
server never reads the notification-stamp back for a decision.

A Phase 5 watch PR will ship both a cadence timer and a response
field; the reviewer's rule is: cadence uses `Instant`, the stamp
(if any) uses `SystemTime`, and they do not cross.

### MVCC revisions — counter, not clock

MVCC revisions are a monotonic `u64` counter. The cross-node "what
happened first" relation is **revision order**, not time.
Comparing revisions across nodes works. Comparing `Instant`s
across nodes does not — `Instant` is per-process.

An in-memory `Instant` _may_ be stored alongside a revision in the
MVCC index for local-only purposes (LRU eviction, local metrics).
That's fine because the `Instant` doesn't cross the process or
participate in protocol decisions.

### Compaction retention — the hard one

"Compact revisions older than N hours" has three viable shapes,
in decreasing safety order:

1. **Revision-count retention**: compact all but the last
   `N` revisions. No clock at all. **Default and preferred.**
2. **Revision-duration retention**, `Instant`-keyed: store each
   revision's creation `Instant` in-memory; compact revisions
   whose `Instant::elapsed()` exceeds `N`. Clean on a running
   process, but `Instant` state does not survive restart — the
   compactor has to treat process start as "all revisions are
   age-zero" until it can recompute. Acceptable if documented.
3. **Revision-duration retention**, wallclock-keyed: store each
   revision's `SystemTime` at creation in the WAL; compact when
   `SystemTime::now() - created > N`. **Durable across
   restart**, but an NTP step can accelerate or delay compaction
   by the step magnitude. Call out the tradeoff in the
   compactor's doc comment.

The policy does not pick one; the Phase 6+ compactor PR picks one
and justifies it in its ADR. What the policy rules out: silently
using `SystemTime::now()` without choosing.

### gRPC inbound deadline propagation

gRPC deadlines arrive on the wire as a `Duration` (seconds
remaining before the client gives up). On receipt the server
converts once: `let deadline = Instant::now() + remaining;`.

The server **never** reconstructs the deadline from a wallclock
header. This is the most common Go-to-Rust porting mistake in
this class — Go's idiomatic `time.Now().Add(remaining)` was
ambiguous about monotonic readings pre-Go-1.9 and many `etcd`
patterns predate that.

### Request-latency metrics and histograms

Request-latency bucketing MUST use `Instant::elapsed()`:

```rust
let start = Instant::now();
handle_request().await?;
histogram.observe(start.elapsed().as_secs_f64());
```

`SystemTime` subtraction here is a classic bug — an NTP step
mid-request produces a negative or wildly large duration, poisoning
the histogram. The Prometheus **scrape** timestamp is wallclock and
owned by Prometheus; we don't touch it.

### Structured logging / tracing — wallclock allowed

`tracing-subscriber`'s default formatter stamps events with
`SystemTime`. **This is fine and allowed.** The stamp is for
human correlation across machines — exactly the case wallclock is
designed for. Tracing events emitted before any `Instant` baseline
exists in the process are similarly fine.

### Snapshots — index-ordered, mtime for display

Raft snapshot metadata is `(term, index, conf_state)`. No time.

Snapshot file retention ("delete snapshots older than N") uses
snapshot **index** for correctness ordering (newer index wins)
and filesystem **mtime** for human display only. Never decide
"which snapshot to delete" by wallclock.

### WAL entries — timestamp-free on the wire

WAL entries do not carry a wallclock timestamp on the wire. An
in-memory debug timestamp on an entry is an `Instant` and is
stripped before serialization. A `SystemTime` field in a wire
entry would be either (a) a correctness bug on any node with a
stepped clock, or (b) pure display that shouldn't be durable —
in both cases, wrong.

### Tests and examples

Whatever reads cleanest. The test-mod inner `#![allow(...)]` in
each crate's `lib.rs` means you can compare `SystemTime`s for
golden-value assertions if the alternative is awkward. Production
code gets the strict rule.

## Named escape hatches

The _only_ domains that may call `SystemTime::now()` (or any
wallclock equivalent — `chrono::Utc::now()`, `jiff::Timestamp::now()`,
`time::OffsetDateTime::now_utc()`, etc.):

1. **Structured logging / tracing subscribers** (per
   `tracing-subscriber` default behavior).
2. **Lease TTL display in gRPC responses** — the `expires_at`
   wallclock field. Never read back for decisions.
3. **Watch progress notification stamps** — optional wire field,
   display only.
4. **gRPC error `Status::details`** wallclock fields for client
   correlation.
5. **Filesystem `mtime` reads** for human-facing snapshot /
   retention CLI output.
6. **Human-facing CLI output** — timestamps printed to terminals.

Every call site in an allowed domain carries a
`// wallclock: display` comment on the same line or the line above,
so a grep audit is mechanical. Anywhere else, `SystemTime::now()`
is presumed disallowed and the reviewer says "why not `Instant`?".

## Enforcement handoff — the first Phase 2+ time PR

This policy ships with no Rust-level enforcement because there is
no `Instant` code to lint yet. The **first PR that introduces
`Instant::now()`** in any non-test module MUST in the same PR
do one of:

a. Add a `clippy.toml` entry disallowing `SystemTime::now` (and
equivalents) outside the six escape-hatch call sites, wired
via `#[allow(clippy::disallowed_methods)]` on each allowed
call site with the `// wallclock: display` comment. Refer to
[this doc](time.md) by the rule number in the `#[allow]`
justification comment.

b. Add a grep-based CI step that fails on any `SystemTime::now`
without an accompanying `// wallclock: display` comment on
an adjacent line. Simpler; loses the IDE integration.

c. Document in the PR description why neither (a) nor (b) is
viable in the current codebase shape, and what the alternate
enforcement is.

This is a hard gate, not a nice-to-have. Phase 2+ without
enforcement means the first Raft PR might ship
`SystemTime::now()` in a hot path and we'd only catch it at
Phase 13's `chaos-clock-skew` test — the wrong place.

## Reviewer checklist

Skim this when reviewing any PR that touches clocks:

- [ ] Every Raft timer (election, heartbeat, commit) uses
      `Instant`, not `SystemTime`.
- [ ] Every lease-expiry computation uses `Instant::now() +
    Duration`. Lease state _on the wire_ is
      `(lease_id, ttl_seconds, granted_at_revision)` only — no
      persisted wallclock expiry.
- [ ] Every watch progress **cadence** uses `Instant`; any wire
      **stamp** is wallclock display-only.
- [ ] Every gRPC inbound deadline is parsed as `Duration` →
      `Instant::now() + d`, never reconstructed from a wallclock
      header.
- [ ] Every request-latency metric / histogram uses
      `Instant::elapsed()`. No `SystemTime` subtraction.
- [ ] Every new `SystemTime::now()` call site is one of the six
      named escape hatches and carries `// wallclock: display`.
- [ ] If the first `Instant::now()` in non-test code lands here,
      the enforcement handoff (`clippy.toml` or grep CI) is in
      the same PR.
- [ ] If a new time-adjacent domain appears that this doc
      doesn't cover, the PR updates this doc.

## NTP / leap-second / VM-clock stance

- **Leap seconds: N/A.** `Instant` is by definition leap-second-
  free. `SystemTime` handling inherits whatever the OS does; we
  do no correction.
- **NTP step tolerance.** Protocol decisions use `Instant`, which
  does not step on NTP adjustments. Wallclock display jumps with
  NTP — correct, by design, not a bug. The Phase 13 chaos test
  (`chaos-clock-skew`, see `ROADMAP.md:1103` fault injector) will
  exercise ±5s clock jumps and assert no Raft election storms,
  no early lease expiry, no MVCC revision inversions.
- **VM live migration.** Both VMware vMotion and EC2 live
  migration can jump wallclock by minutes. `Instant` behavior on
  migration is platform-dependent — Linux `CLOCK_MONOTONIC` on
  KVM stays monotonic across migration; other hypervisors may
  pause and resume. The Phase 13 chaos test should exercise both
  flavors. Until it does, operators running on exotic hypervisors
  are on their own.

## Relation to Go etcd

Go has no strict monotonic/wallclock type separation. `time.Time`
carries both a wallclock and (since Go 1.9) a monotonic reading;
`time.Since` / `time.Until` use the monotonic reading when
present, but porting-era code often discards it via
`.Round(0)` or wallclock comparisons. Multiple classes of etcd
time bugs trace to this ambiguity:

- **Election timers driven by `time.Now()` in pre-Go-1.9
  patterns** — any operation that round-tripped a `time.Time`
  through a wallclock comparison could reorder events on NTP
  step. Most have been mopped up, but the category existed for
  years.
- **Lease TTL "surprise" across leader failover** — the
  `2 * TTL` upper bound is documented in etcd's lease behavior
  as a known property but has been filed as a surprise multiple
  times. Mango inherits the same property _by choice_ (persisting
  wallclock expiry would trade it for a worse bug); the doc
  above names it up front so nobody re-discovers it.
- **Compactor-by-wallclock on NTP step** — retention-by-time
  using wallclock means an NTP step can collapse or inflate the
  retention window. Mango's compactor (Phase 6+) picks its
  retention shape deliberately; see "Compaction retention"
  above.
- **VM live-migration clock jumps** — production operators
  running etcd on vMotion'd VMs have reported spurious election
  churn on wallclock jumps. Mango's `Instant`-based timers are
  immune; the chaos test in Phase 13 verifies.

Mango starts with these rules in writing so we do not rediscover
the above one incident at a time.

## Policy maintenance

This doc drifts if nobody touches it. Owners:

- Any PR introducing a new time-adjacent domain MUST update this
  doc in the same PR. Reviewer enforces.
- The enforcement handoff is triggered by the first `Instant::now()`
  in non-test code — that PR is the policy's first real test.
- The policy is linked from `CONTRIBUTING.md` (Phase 0 item 0.14)
  and the PR template (item 0.15) so contributors hit it on their
  first PR.
- Cross-linked with [docs/arithmetic-policy.md](arithmetic-policy.md)
  — deadline arithmetic is the overlap domain.
