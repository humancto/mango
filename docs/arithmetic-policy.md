# Arithmetic-primitive policy

This document is the one-true-way to do integer and duration arithmetic
in mango. It exists so that `clippy::arithmetic_side_effects` (denied
workspace-wide in `Cargo.toml`) can fail the PR at clippy time
**before** a silent-overflow bug ships, and so contributors know which
variant to reach for without having to re-derive the rationale in every
review.

Every PR that introduces arithmetic is expected to comply. If a new
domain comes up that this doc doesn't cover, update the doc in the
same PR.

## TL;DR — what do I use for X?

| Domain                                                         | Use                                              | Rationale tag       |
| -------------------------------------------------------------- | ------------------------------------------------ | ------------------- |
| Raft index, term, commit index, applied index                  | `checked_*`                                      | Protocol counter    |
| MVCC revision, lease ID, watch ID                              | `checked_*`                                      | Protocol counter    |
| Any `u64` monotonic counter that's part of the wire protocol   | `checked_*`                                      | Protocol counter    |
| `usize` slice / index math (`len() - 1`, `idx + 1`)            | `checked_*` OR `// BOUND:` + narrow `#[allow]`   | Index safety        |
| Deadline math (`Instant + Duration`)                           | `Instant::checked_add`                           | Deadline ordering   |
| Timeout / backoff budget math (`Duration + Duration`, `d * n`) | `Duration::saturating_*`                         | Budget clamping     |
| Hashes, CRCs, mod-N ring indices                               | `wrapping_*`                                     | Modular semantics   |
| `AtomicU64::fetch_add` / `fetch_sub`                           | Audit at call site; helper for protocol counters | Not linted          |
| Test / example code                                            | Whatever reads cleanest                          | Allowed in test mod |

If in doubt, **use `checked_*` and propagate the `Option`/`Result`.**
Over-checking is a reviewable style concern; under-checking is a
production outage.

## Why each — the rationale

### Protocol counters — `checked_*`

A wrapped Raft index or MVCC revision is a permanently wedged cluster.
The wedge shows up months later as a tail-latency spike, a leader
election loop, or a mysteriously stuck watch. The bug is invisible in
logs until it is catastrophic.

The pattern:

```rust
pub fn advance(&mut self, by: u64) -> Result<(), OverflowError> {
    self.index = self.index.checked_add(by).ok_or(OverflowError)?;
    Ok(())
}
```

The caller decides whether an overflow means "crash the process"
(fail-stop: best for protocol counters you cannot recover from) or
"reject the request" (for request-scoped counters where the client
should see an error). The point is that the decision is **made
deliberately**, not accidentally by silent wrap.

### `usize` slice / index math — `checked_*` or bounded-with-comment

This is the largest day-to-day surface for the lint and will dominate
Raft log and MVCC code. Two acceptable patterns:

1. **Preferred**: propagate via `checked_*`:

   ```rust
   let prev = idx.checked_sub(1).ok_or(LogError::Underflow)?;
   ```

2. **Acceptable with bound comment**: when the invariant is clear from
   the surrounding loop or guard:

   ```rust
   // BOUND: self.entries.len() > 0 by guard on line above
   #[allow(clippy::arithmetic_side_effects)]
   let last = self.entries.len() - 1;
   ```

The `// BOUND:` comment is required so a future reader can audit the
invariant without hunting through git blame. A reviewer who can't
verify the bound from the comment should ask for the `checked_*` form.

### Deadline math — `Instant::checked_add`

`Instant` ordering drives timeout dispatch in Raft election timers,
lease expiry, and watch progress. A wrapped deadline silently reorders
events — exactly the shape of a correctness bug we cannot debug from
logs.

```rust
let deadline = now
    .checked_add(self.election_timeout)
    .ok_or(TimeError::DeadlineOverflow)?;
```

`Instant` saturation at `Instant::MAX` is meaningful but dangerous
(the timeout fires at the heat death of the universe), so we treat it
as an error rather than silently clamping. The caller decides.

### Timeout / backoff budget math — `Duration::saturating_*`

Durations used as budgets (retry backoff, sleep amounts) are a
different story. `Duration::MAX` is strictly better behavior than a
panic: the caller's retry loop either succeeds on the next attempt or
gives up via an independent deadline. Clamping is the right semantic.

```rust
let next_backoff = current.saturating_mul(2);
let sleep = backoff.saturating_add(jitter);
```

Do **not** use `saturating_*` on `Instant`. An instant saturated at
`Instant::MAX` is not a budget, it's a silent deadline reordering.

### Hashes, CRCs, modular arithmetic — `wrapping_*`

Wrap-around IS the semantics. `checked_*` here would be a type error:

```rust
let slot = (hash.wrapping_add(probe)) % self.capacity;
```

Includes: FNV / xxhash / any hash accumulator, CRC state, ring-buffer
indices computed mod capacity, sequence-number comparisons that rely
on modular ordering.

### Atomics — not linted, audit manually

`AtomicU64::fetch_add` is a method call, not an operator, so
`clippy::arithmetic_side_effects` does not fire on it. For
protocol-counter atomics, write a `checked_fetch_add` helper that
CAS-loops on overflow:

```rust
fn checked_fetch_add(atomic: &AtomicU64, delta: u64) -> Option<u64> {
    let mut cur = atomic.load(Ordering::Relaxed);
    loop {
        let next = cur.checked_add(delta)?;
        match atomic.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return Some(cur),
            Err(observed) => cur = observed,
        }
    }
}
```

Naked `fetch_add` is fine for stats counters and anywhere an overflow
would be cosmetic. The reviewer call is "is this a protocol counter?"
— if yes, helper; if no, naked.

## How to satisfy `?` at the API boundary

Protocol-counter methods return `Result<Self, OverflowError>` (or
`Option<Self>` for crate-private helpers). The error type is local to
the crate; at the service boundary it maps to a protocol error
(`grpc::Status::resource_exhausted` or similar).

```rust
pub struct OverflowError;

impl From<OverflowError> for RaftError {
    fn from(_: OverflowError) -> Self { RaftError::IndexExhausted }
}
```

Downstream of this, the `?` operator Just Works and the caller is
forced to either handle or propagate.

## When `#[allow]` is acceptable

Exactly four named cases. Anywhere else, reach for the explicit
variant.

**a) Loop-index arithmetic statically bounded.** `for i in 0..n`
patterns inside hot paths where the bound proof is obvious from the
surrounding code. Required: a `// BOUND:` comment on the line above
the arithmetic.

**b) `const fn` initializers.** The lint has known false positives in
const contexts even when the compiler would reject overflow at CTFE.
**Preferred workaround first**: use a `const` block with
`.checked_add(...).expect("...")`:

```rust
const MAX_ENTRIES: usize =
    const { BASE.checked_add(OFFSET).expect("MAX_ENTRIES overflow") };
```

This catches overflow at compile time and needs no `#[allow]`.
`#[allow]` is the fallback only when the const-block pattern is not
viable (non-const value, generic bound).

**c) Test and doctest code.** The test-mod inner `#![allow(...)]` in
each crate's `lib.rs` handles this. Do not add `#[allow]` on
individual test functions.

**d) `usize` slice/index math with a `// BOUND:` comment.** As
documented in the TL;DR — the comment establishes the invariant, the
`#[allow]` is narrow (one binding or one statement), the reviewer
verifies the bound.

## Reviewer checklist

Skim this when reviewing any PR that touches arithmetic:

- [ ] Every `+` / `-` / `*` on a `u64` protocol counter uses `checked_*`.
- [ ] Every deadline `Instant + Duration` uses `Instant::checked_add`.
- [ ] Every backoff / timeout `Duration` arithmetic uses `saturating_*`.
- [ ] Every hash / ring-index arithmetic uses `wrapping_*`.
- [ ] Every `usize` index arithmetic either uses `checked_*` or
      carries a `// BOUND:` comment with a narrow `#[allow]`.
- [ ] No new `#[allow(clippy::arithmetic_side_effects)]` outside test
      code and generated code without matching one of the four named
      exceptions above.
- [ ] If a new arithmetic domain appears that this doc doesn't cover,
      the PR updates the doc.

## Relation to Go etcd

Go has no equivalent of `clippy::arithmetic_side_effects`. Every
production etcd bug in this class — Raft index overflow regressions,
revision arithmetic in the MVCC compactor, lease-ID exhaustion — would
have fired this lint at PR time in mango.

The policy exists so mango does not rediscover those bugs one wedged
cluster at a time.

## Policy maintenance

This doc drifts if nobody touches it. Owners:

- Any PR introducing a new arithmetic domain MUST update this doc in
  the same PR. Reviewer enforces.
- A quarterly policy audit is filed as a future reliability-hardening
  item once `CONTRIBUTING.md` and the PR template land
  (`ROADMAP.md:761-762`).
- The policy is linked from `CONTRIBUTING.md` and the PR template so
  contributors hit it on their first PR.
