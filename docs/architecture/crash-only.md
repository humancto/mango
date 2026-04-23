**`kill -9` at any instant is a supported lifecycle event; clean shutdown is an optimization, not a correctness boundary.**

# Crash-only design declaration

This document is the one-true-way to reason about process lifecycle
in mango. Storage and server layers assume the process can be
killed at any instant — by `kill -9`, by OOM, by a kernel panic,
by a pulled power cord. Clean shutdown is the _same path with an
optimization_ (drain in-flight apply, flush WAL, release the
data-dir lock, close streams), never a distinct correctness path.
Every storage / Raft PR satisfies the rule above or explicitly
documents why it does not.

No mango storage or Raft code exists yet (Phase 0 is pre-code).
This doc ships the rule _before_ the first WAL append so the
first storage PR has a policy to conform to, not to invent.

## TL;DR — the crash-safety contract

| Domain                          | Invariant                                                                  | Enforcing mechanism                             |
| ------------------------------- | -------------------------------------------------------------------------- | ----------------------------------------------- |
| WAL append                      | `fsync`-before-ack; partial appends invisible on replay                    | length-prefix + CRC tail; fsync before reply    |
| State-machine apply             | WAL-durable before apply; `applied_index` durable                          | apply-cursor advanced after state-machine write |
| Apply on replay                 | Every WAL-applied op is idempotent; re-applying changes nothing observable | entry-index check in apply loop                 |
| Snapshot install                | Partial snapshot files never readable as "complete"                        | `.tmp` + fsync + rename + parent-dir fsync      |
| `data-dir/VERSION` migration    | Mid-migration torn state invisible                                         | write-new + fsync + rename-over + parent fsync  |
| MVCC index                      | Rebuilt from WAL on start; never the source of truth                       | in-memory only; WAL is authoritative            |
| Lease state                     | Replicated via Raft; never single-node durable                             | lease ops go through WAL like any other op      |
| Multi-file atomicity            | A group of files is either all visible or none                             | rename-barrier file or explicit commit record   |
| In-memory caches                | Rebuilt from durable state on restart                                      | no dirty flag; no shutdown-flush path           |
| gRPC request in-flight on crash | At-least-once to the client; re-submission is idempotent                   | client request-id + server-side dedupe window   |

If in doubt, **assume `kill -9` is about to fire on the next
line.** If the code would be correct under that assumption, ship
it. If not, add the fsync / rename / replay-idempotency that
makes it correct.

## Why each — failure mode first, mechanism second

### WAL append — fsync-before-ack

**Failure mode.** The leader accepts an entry on the wire, appends
it to the WAL, writes the CRC, but crashes before `fsync` returns.
A client that read the ack sees the write succeed; the cluster
on restart sees no entry. Split brain between client expectation
and durable state.

**Mechanism.** No ack is sent until `fsync` on the WAL file has
returned. Every entry is length-prefixed and CRC-tailed; on
replay, a partial append — length prefix written, payload or CRC
not — is detected by CRC mismatch at the tail and truncated. The
boundary is explicit: post-fsync durable, pre-fsync lost, no
third state.

### State-machine apply — WAL-durable before apply

**Failure mode.** An entry is applied to the state machine, made
visible to reads, and then the WAL append fails or the process
crashes before the WAL entry is durable. On restart the state
machine forgets the operation; a client that read the result
sees regressed state.

**Mechanism.** WAL append fsyncs before apply runs. `applied_index`
is durable (either in a separate fsynced cursor file or as part of
the state-machine's own durable storage). Apply cadence is: WAL
fsync → state-machine write → advance `applied_index` → ack. A
crash at any point in that chain leaves the cluster in a state
the replay path handles.

### Apply on replay — idempotency

**Failure mode.** Process crashes after WAL-durable append of
entries N..M but before all of them have been applied. On restart,
the state machine must re-apply the tail. If any op is
non-idempotent — `Put` that doubles on re-apply, `LeaseGrant` that
issues a second lease-id, `Txn` that commits twice — the state
machine diverges from quorum.

**Rule.** Every WAL-applied op is idempotent under re-apply. The
current op set:

- `Put(key, value)` — last-writer-wins on the revision; re-apply
  writes the same revision with the same value. Idempotent.
- `Delete(key, range)` — tombstone at the given revision.
  Re-apply writes the same tombstone. Idempotent.
- `Txn(cmp, then, else)` — the commit record carries the chosen
  branch and the revision. Re-apply repeats the chosen branch
  against the pre-commit state; because revisions are
  monotonic and the outcome is recorded, the re-apply
  produces the same ops. Idempotent.
- `LeaseGrant(lease_id, ttl)` — `lease_id` is chosen by the
  leader at grant time and written in the log entry. Re-apply
  is a no-op if the lease already exists at the same id.
  Idempotent.
- `LeaseRevoke(lease_id)` — remove lease; re-apply is a no-op
  if already gone. Idempotent.

**Reviewer's triage question**: "if this entry is applied twice,
does the second application change observable state?" If yes,
the op is wrong — redesign so the log entry carries enough
information that re-apply is a no-op.

**Not covered by this rule**: leader-local state that is not
WAL-applied. Lease `KeepAlive` is a leader-local timer refresh
(it does not go through Raft). Watch progress attach / detach is
leader-local subscription state. Both are governed by different
mechanisms — 2×TTL re-arming for leases (see
[docs/time.md](../time.md)), client re-subscribe for watches —
not by replay idempotency. A new leader rebuilds both from the
replicated durable state, which the idempotency rule protects.

### Snapshot install — rename-over-with-fsync

**Failure mode.** A leader ships a snapshot; the follower
writes `snap.db.part`, crashes mid-write, and on restart finds a
half-file that looks like a complete snapshot.

**Mechanism.** Snapshot install writes to a `.tmp`-suffixed path,
fsyncs the file, renames over the final name, fsyncs the parent
directory. Recovery treats any `.tmp` file as garbage and deletes
it. A file at the canonical name is definitionally complete;
there is no "complete" flag to forget to set.

### `data-dir/VERSION` — mid-migration torn state invisible

**Failure mode.** The operator runs a format migration; the
migrator writes the new VERSION file over the old one in-place,
crashes mid-write, and the data dir is left with a VERSION that
matches neither format. Recovery refuses to start; operator is
stuck.

**Mechanism.** Migration writes `VERSION.tmp`, fsyncs, renames over
`VERSION`, fsyncs the parent directory. A process reading
`VERSION` sees the old value or the new value, never a partial
one. (Phase 12 specifies the file format itself — this doc only
specifies that the write is torn-state-invisible.)

### MVCC index — rebuilt from WAL

**Failure mode.** The MVCC in-memory btree is treated as
authoritative and lazily flushed. A crash between flushes loses
writes the WAL already committed.

**Mechanism.** The MVCC index is **not durable state**. It is
rebuilt on start by replaying the WAL up to `applied_index`. An
in-memory btree is a cache on top of the durable log + the
state-machine's own store; cache rebuild on start is a recovery
primitive, not a failure.

### Lease state — replicated, not single-node durable

**Failure mode.** A lease grant succeeds on a leader that crashes
before replicating the grant to a quorum. On failover, the
cluster forgets the grant; the client still thinks it holds the
lease.

**Mechanism.** Lease grants go through the Raft log like any
other state-machine op. "Success" on the client wire means the
log entry is committed (replicated to a quorum and applied),
not merely appended. Two crash-regimes:

1. **Pre-commit-crash** — leader appends the `LeaseGrant` log
   entry locally, crashes before the entry reaches a quorum. On
   election, the new leader either has the entry (and commits
   it) or does not (and the minority tail is truncated). If
   truncated, the client's `Grant` RPC fails with a retryable
   error (connection reset / `Unavailable`); the client
   re-grants. The cluster has no record of the forgotten grant;
   the client has no record of believing it succeeded.
2. **Post-commit-crash** — leader acks the grant, then crashes.
   The entry is durable on a quorum; the new leader has it and
   re-arms expiry as `Instant::now() + ttl_seconds`. The lease
   can effectively live up to 2×TTL across the failover. This
   is [monotonic-clock policy](../time.md), not crash-only, but
   it's the user-visible consequence of the crash.

**Invariant.** **A client never believes it holds a lease the
cluster has forgotten.** The retryable-error contract on
pre-commit-crash is what enforces this: the client's belief is
re-synchronized with cluster state via the error path, not via
persisted wallclock expiry.

### Raft-log vs. state-machine-apply durability boundary

The Raft log and the state machine have separate durability
stories. The WAL is authoritative for "what operations have been
agreed on." The state machine is authoritative for "what the
current value of key X is." `applied_index` is the handoff point.

- Below `applied_index` — state machine is durable; WAL can be
  compacted (eventually truncated by snapshot).
- Above `applied_index`, up to `commit_index` — WAL is durable;
  state machine has not yet seen these entries. On restart the
  apply loop replays them.
- Above `commit_index` — WAL entries exist but are not yet
  agreed-on; on restart the leader re-commits or the new leader
  truncates.

Every PR touching this boundary — apply loop, `applied_index`
advancement, snapshot-install, WAL truncation — answers the
reviewer's question: "what does the boundary look like mid-
operation on `kill -9`?"

### Multi-file atomicity — rename barrier or commit record

**Failure mode.** An operation writes three files that must all
become visible together (e.g., a snapshot manifest + data file +
index file). A crash after the first two renames leaves the third
uncommitted and the manifest references a missing file.

**Mechanism.** One of:

- **Rename barrier** — all-but-last file written to final paths;
  the last file is written to `.tmp` and renamed last, parent
  fsynced. Recovery sees the barrier file present or absent; if
  absent, the partial state is garbage-collected.
- **Commit record** — a small `COMMIT` file is the last thing
  written and the only thing recovery trusts. Without `COMMIT`,
  recovery deletes the in-progress directory.

### In-memory caches — not authoritative

**Failure mode.** A cache with a dirty-flag + shutdown-flush path
loses dirty entries on `kill -9`. The code-review-side tell:
`Drop` impls that flush, shutdown hooks that persist, "final
fsync on exit."

**Mechanism.** Caches are rebuilt from durable state on start.
The durable store (WAL + state-machine) is the source of truth;
caches are derived.

### gRPC request in-flight on crash — at-least-once + dedupe

**Failure mode.** Client sends `Put(k, v)`; server appends to
WAL, commits, applies; crashes before the ack reaches the client.
Client retries; server applies `Put(k, v)` a second time. For
idempotent ops this is fine. For ops with side effects (lease
grant creating a new id on each attempt, Txn with external
effects) a retry produces divergence.

**Mechanism.** Clients attach a request-id to every logical RPC.
The server keeps a short dedupe window (in-memory, bounded by
applied-index delta). A request-id seen within the window returns
the cached response instead of re-applying. This is the Go etcd
`RequestHeader.ID` pattern; mango matches it by default.

### Clean shutdown — optimization, not requirement

Clean shutdown does four things, in order:

1. Stop accepting new RPCs.
2. Drain in-flight ones (see escape hatches).
3. Flush / fsync any pending WAL.
4. Release the data-dir lockfile.

None of the four are required for correctness. The WAL is already
fsynced-before-ack (any entry that got acked is durable). The
data-dir lockfile steal-on-stale-PID path handles a leftover lock
on crash. Clean shutdown is faster startup on the next boot and
politer to clients; it is not a distinct correctness path.

## Named anti-patterns

Don't write these. They'll be caught in review.

1. **In-memory dirty flags that only flush on shutdown.** Dirty
   state must be persisted incrementally, not at exit.
2. **`Drop` impls that perform I/O.** `Drop` runs on panic unwind
   and on clean shutdown but not on `kill -9`. Anything load-
   bearing in `Drop` will not run on crash.
3. **Temp-file rename without parent-directory fsync.** The rename
   may not be durable after a crash; recovery sees the old name.
4. **Multi-file atomic operations without a rename-barrier or
   commit record.** A crash between renames leaves partial state
   that recovery can't distinguish from complete.
5. **`mmap`-written pages without `msync(MS_SYNC)` before
   `fsync()` of the backing file.** `fsync` on a file with
   modified mapped pages does not guarantee the pages are written
   back; `msync(MS_SYNC)` first, then `fsync`.
6. **`data-dir/VERSION` rewrite that overwrites in place.** Must
   be write-new + rename-over + parent fsync. In-place writes
   leave torn state.
7. **DB transaction held across a gRPC / `await` boundary.** A
   yielded future can be dropped on process kill; depending on
   the DB driver this may or may not roll back cleanly. Finish
   the transaction before yielding.
8. **"Atomic batch" implemented as N independent writes with no
   commit record.** A crash after write 1 and before write 2
   leaves the database in a state the code thinks is impossible.
9. **Applying uncommitted log entries to the state machine.**
   Learners and observers legitimately receive uncommitted
   entries from the leader; they must not apply them until the
   commit index catches up. The state machine only ever sees
   committed entries.
10. **Tests that assert "after clean shutdown the data looks like
    X" without the `kill -9`-first variant.** Every storage /
    Raft test with a shutdown-then-restart assertion also needs
    a `kill -9`-then-restart assertion. If the latter doesn't
    exist, the test is only validating the optimization path.
11. **Ignoring or retrying `fsync` `EIO` without treating the file
    as poisoned.** On Linux, `fsync()` returning `EIO` clears the
    dirty bit — a retry returns `0` against data that is not
    durable. PostgreSQL's fsyncgate (2018) is the canonical
    incident. `EIO` on a WAL / state-machine fsync must crash the
    process and force operator intervention; a silent retry is a
    correctness bug.
12. **Reading WAL / snapshot data before validating its
    length-prefix and CRC tail.** Verify the checksum first, then
    use the bytes. Use-then-verify can crash the parser on
    malformed input or return ghost data that later fails the
    CRC.

## Reviewer's contract

Every storage / Raft PR — anything that touches the WAL, the apply
loop, snapshots, `data-dir/VERSION`, lease durability, MVCC
storage, multi-file writes, or the apply-queue — has a section in
its PR description titled `Crash-safety:` with one of three
markers:

1. `kill-safe by construction — <invariant>.`
   Example: `WAL fsync-before-ack; partial-entry CRC invisible.`
2. `kill-safe + test: <test path or module>.`
   Example: `kill-safe + test: crates/mango-wal/tests/kill_mid_fsync.rs`
3. `not kill-safe yet — follow-up #<N>, lands before <phase-gate>.`
   Example: `follow-up #42, lands before Phase 2 gate.`

The `rust-expert` reviewer agent enforces this as a checklist
item on every PR review. A missing or hand-wavy `Crash-safety:`
section is a `REVISE` verdict; the reviewer does not approve
without one of the three markers. The PR template (Phase 0 item
0.15, `ROADMAP.md:763`) copies `Crash-safety:` into the template
body so authors hit it at PR-open time.

## Enforcement handoff — the first persistent-state PR

This policy has no Rust-level lint because "crash-safety" is a
design-review property, not something a compiler catches. The
reviewer's contract above is the enforcement.

For an extra belt-and-suspenders when code actually starts
touching durable state: **the first PR that introduces persistent
on-disk state** (WAL segment file, state-machine store, snapshot
write, `VERSION` file) MUST in the same PR do one of:

a. Add a grep-based CI step that fails any PR-body without a
`Crash-safety:` section containing one of the three marker
shapes above. Simple and obvious; no type-system cost.

b. Add a `scripts/check-crash-safety-marker.sh` hook run in CI
against `gh pr view --json body` parsing. More forgiving of
non-English prefixes, but more moving parts.

c. If neither (a) nor (b) is viable in the current codebase
shape, the PR MUST describe the alternate enforcement
**mechanism** (not merely the rationale) AND name the
follow-up PR or roadmap item that will land (a) or (b) within
one phase. "We'll revisit" is not an acceptable (c). The
reviewer who accepts a (c) without a named follow-up owns
the gate failure.

This mirrors the monotonic-clock policy's enforcement shape
(`docs/time.md:241-271`). It is a hard gate, not a nice-to-have:
the reviewer's contract is only as strong as review discipline,
and discipline drifts at volume.

## Named escape hatches — where clean shutdown IS load-bearing

Correctness does not depend on clean shutdown. These are the
places where _operator UX_ or _politeness_ do. Every call site
here carries the grep-auditable comment on the line above:

```rust
// crash-safety: shutdown-required — <one-line reason>
```

A `rg 'crash-safety: shutdown-required'` enumerates every opt-out.
No such comment → implicit kill-safe.

1. **`mangoctl defrag`** — online defrag of the data-dir.
   Transactional-on-disk or documented "run while quiesced."
   Without clean-shutdown coordination, a crash mid-defrag can
   leave garbage temp files that the operator must clean up by
   hand.
2. **`mangoctl snapshot save`** — user-requested snapshot copy
   to a target path. Same shape as defrag: transactional or
   quiesced.
3. **Data-dir lockfile release.** Clean shutdown removes `LOCK`
   in the data-dir. Process kill leaves the stale lockfile.
   Recovery on the same host MUST PID-check and steal the stale
   lock (PID gone → steal), not refuse to start. The lockfile
   itself is a politeness optimization; correctness is handled
   by the stale-steal path.
4. **gRPC Watch bidi-stream drain.** Clean shutdown closes
   streams politely with a `Status::unavailable` pointing at a
   peer. Kill drops mid-frame; clients auto-reconnect and resume
   from the last-seen revision. Watch events are WAL-durable;
   no event loss. Drain is politeness.
5. **Raft apply-queue drain.** Clean shutdown lets the apply
   loop finish draining WAL-appended-but-not-applied entries
   before exit, so restart doesn't re-do work the network
   already observed through the wire. Correctness is preserved
   either way (the replay path handles it) — this is a startup-
   latency optimization.
6. **Unary RPC connection drain.** In-flight `Put` / `Txn` /
   `LeaseGrant` calls complete before SIGTERM, matching the
   Watch-stream drain pattern. Kill drops them mid-call; clients
   retry against the new leader with their request-id intact
   (see "gRPC request in-flight on crash" above). Correctness
   is the request-id dedupe; drain is politeness.

## Phase 13 chaos-test forward-reference

`ROADMAP.md:1103` broadens the kill-point fault-injector knob (in
the same PR as this doc) to:

> kill processes mid-fsync / mid-snapshot-install / mid-leader-
> elect / mid-compaction

Each of the four is a knob the Phase 13 simulator drives against
the deterministic fault injector. Every storage / Raft PR's
`Crash-safety:` marker names a test under the relevant knob
whenever marker (2) is chosen.

Other ROADMAP:1103 knobs relevant to crash-only:

- `EIO injection` on any syscall — the path for anti-pattern 11
  (fsync `EIO` handling).
- `disk-page corruption` — the path for anti-pattern 12
  (verify-before-use on WAL / snapshot reads).

## Relation to Go etcd

Go etcd has hit every one of these crash-safety bug classes in
production. Cited as classes, not exact issue numbers, so the doc
ages well:

1. **bbolt freelist corruption on mid-transaction crash.** The
   original driver of etcd's `--experimental-bbolt-freelist-type`
   and the auto-compaction + consistency check plumbing. A
   torn freelist page can make the database unopenable.
2. **WAL CRC + data-fsync split.** The `#10346`-family: CRC
   written before the payload was durable, so recovery accepted
   a partial entry as valid. Mechanism: atomic CRC + payload
   write with a single fsync barrier.
3. **Leader re-election losing writes acknowledged by a minority
   but not durable before the minority lost leadership.** Classic
   Raft gotcha when the apply path predates WAL-durability
   guarantees; solved by ack-after-quorum-fsync.
4. **Snapshot-apply torn on crash mid-install.** Leftover
   `snap.db.part` files in `snap/` that forced manual cleanup
   before the node would start. Solved by `.tmp`-rename-fsync
   discipline and a garbage-sweep on startup.
5. **`panic` during apply after the log entry is WAL-durable.**
   Every restart re-panics on the same entry; the cluster is
   wedged. Solved by apply-path hardening (no panics on input
   from the log; treat parse failures as corruption) and by a
   "poison entry" escape hatch the operator can invoke manually.
6. **Lease revoke raced with client KeepAlive across failover.**
   The pre-commit-crash case. A lease revoke that does not reach
   quorum before the leader crashes can race with a client's
   next KeepAlive on the new leader. The invariant "client never
   believes it holds a lease the cluster has forgotten" is
   precisely what guards this.

Mango starts with these rules in writing so we do not rediscover
them one wedged cluster at a time.

## Policy maintenance

This doc drifts if nobody touches it. Owners:

- Any PR introducing a new crash-safety domain MUST update this
  doc in the same PR. Reviewer enforces.
- The enforcement handoff (the (a)/(b)/(c) grep-CI gate) is
  triggered by the first persistent-state PR — that PR is the
  policy's first real test.
- The policy is linked from `CONTRIBUTING.md` (Phase 0 item 0.14,
  `ROADMAP.md:762`) and the PR template (item 0.15,
  `ROADMAP.md:763`) so contributors hit it on their first PR.
- Cross-linked with [docs/time.md](../time.md) — the
  lease-failover case (2×TTL on post-commit-crash, retryable
  error on pre-commit-crash) lives in both docs.
- Cross-linked with [docs/arithmetic-policy.md](../arithmetic-policy.md)
  — WAL indices and `applied_index` are protocol counters; the
  arithmetic policy's overflow rules apply.
