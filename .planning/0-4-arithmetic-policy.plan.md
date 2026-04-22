# Plan: workspace arithmetic-primitive policy

Roadmap item: Phase 0 — "Workspace arithmetic-primitive policy doc at
`docs/arithmetic-policy.md`: defaults to `checked_*` for protocol-relevant
counters (Raft index, revision, term, lease ID), `saturating_*` for
timeouts and backoff timers, `wrapping_*` for hashes and explicit modular
arithmetic. Once the policy lands and is reviewed,
`clippy::arithmetic_side_effects` is turned on workspace-wide as a
follow-up PR (counted as part of this item)." (`ROADMAP.md:751`)

## Goal

Establish the one-true-way to do integer arithmetic in this codebase
**before** the first Raft / revision / lease code lands, so that:

1. The #1 production failure mode of Go etcd (silent integer overflow in
   Raft index or MVCC revision math, visible only as a wedged cluster
   three months later) cannot happen here — every protocol counter is
   `checked_*`, and the `?` operator on a `Result` / `None`-on-overflow
   forces the caller to either handle the overflow or propagate it.
2. Timeout / backoff math (where saturating at `Duration::MAX` is the
   _correct_ behavior, not a bug) doesn't pollute the PR review with
   "why isn't this `checked_add`" churn — the policy pre-authorizes
   `saturating_*` for the timeout domain.
3. Hash / modular math (where wrap-around is the point) doesn't force a
   `#[allow(clippy::arithmetic_side_effects)]` on every line — the
   policy pre-authorizes `wrapping_*` for that domain.
4. `clippy::arithmetic_side_effects` can be turned on workspace-wide
   without a wave of retrofit `#[allow]` escapes, because the policy
   tells contributors to reach for the explicit variant from the start.

This item has **two deliverables bundled**, per the roadmap's explicit
"counted as part of this item":

- **D1**: `docs/arithmetic-policy.md` — the written policy.
- **D2**: workspace clippy config turns on
  `clippy::arithmetic_side_effects = "deny"` with the explicit
  `priority = 1` used throughout PR #10.

Both land in a single PR. The policy exists for the lint; the lint is
meaningless without the policy.

## North-star axis

**Reliability + Correctness.** Catches the exact foot-gun class
(silent integer overflow on a protocol counter) that has bitten every
production etcd-class system at least once. The lint fails at PR time;
without the policy, contributors wouldn't know which arithmetic variant
to reach for and would `#[allow]` out of frustration.

## Approach

Three file touches. One new doc. One `Cargo.toml` edit. One test-mod
attribute refresh in `crates/mango/src/lib.rs`.

### D1. `docs/arithmetic-policy.md` (new)

Structure:

1. **TL;DR table** — "what do I use for X":
   - Raft index, term, commit index, applied index → `checked_*`
   - MVCC revision, lease ID → `checked_*`
   - Any `u64` monotonic counter that's part of the protocol → `checked_*`
   - **`usize` slice / index arithmetic** (`len() - 1`, `idx + 1`,
     `end - start`) → `checked_*` with `OverflowError` propagation,
     OR a `// BOUND:` comment proving the bound plus a narrow
     `#[allow]`. This is the largest day-to-day category in Raft log
     / MVCC code.
   - **Deadline math** (`Instant + Duration`) → `Instant::checked_add`.
     A wrapped deadline silently changes ordering and is a correctness
     bug.
   - **Timeout / backoff budget math** (`Duration + Duration`,
     `Duration * n`) → `saturating_*`. `Duration::MAX` is better
     behavior than a panic; the caller's retry loop succeeds on the
     next attempt or bails on an independent deadline.
   - Hashes, CRCs, modular arithmetic (mod N ring index) → `wrapping_*`
   - **Atomic `fetch_add` / `fetch_sub`** — not covered by the lint
     (the lint fires on `+` / `-` / `*` operators, not method calls).
     Audit at the call site: if the atomic is a protocol counter,
     wrap in a `checked_fetch_add`-style helper that CAS-loops on
     overflow.
   - Test / example code → whatever reads cleanest; the test-mod
     `#[allow]` handles it.
2. **Why each** — short paragraph per domain. For protocol counters the
   argument is "a wrapped Raft index is a permanently wedged cluster,
   and the wedge shows up months later as a tail-latency spike or a
   leader election loop." For backoff budgets it's "a `Duration::MAX`
   backoff is strictly better behavior than a panic; the caller's retry
   loop will either succeed on the next attempt or give up via an
   independent deadline." For deadlines it's "`Instant` ordering drives
   timeout dispatch — a wrapped deadline reorders events silently, which
   is the exact shape of a correctness bug we cannot debug from logs."
   For hashes / modular arithmetic it's "wrap-around IS the semantics;
   `checked_*` would be a type error."
3. **How to satisfy `?`** — pattern: protocol counter methods return
   `Result<Self, OverflowError>` or propagate `None` via `.ok_or(...)`,
   the caller decides crash-vs-reject at the protocol boundary.
4. **When `#[allow]` is acceptable** — exactly four cases:
   a) Loop-index arithmetic that's statically bounded
   (`for i in 0..n` patterns inside hot paths where the bound proof
   lives in a `// BOUND:` comment above the arithmetic).
   b) `const fn` initializers — the lint has known false positives in
   const contexts even when the compiler would reject overflow at
   CTFE. **Preferred workaround**: use a `const` block with
   `.checked_add(...).expect("...")`, e.g.
   `const { A.checked_add(B).expect("overflow at compile time") }` —
   this catches overflow at compile time and needs no `#[allow]`.
   Only escape to `#[allow]` when the const-block pattern isn't
   viable (non-const value, generic bound).
   c) Test and doctest code — the test-mod allow block handles this.
   d) `usize` slice/index math where a `// BOUND:` comment establishes
   the invariant (e.g., `BOUND: len > 0 by loop invariant on line N`).
   Anywhere else, the PR is expected to use the explicit variant.
5. **Reviewer checklist** — one-line bullets a PR reviewer can skim:
   "every `+` / `-` / `*` on a `u64` protocol counter uses a checked
   variant", "every deadline `Instant + Duration` uses `checked_add`",
   "every backoff / timeout duration arithmetic uses `saturating_*`",
   "every hash / ring-index arithmetic uses `wrapping_*`", "every
   `usize` index arithmetic either uses `checked_*` or carries a
   `// BOUND:` comment with a narrow `#[allow]`", "no new
   `#[allow(clippy::arithmetic_side_effects)]` outside test code and
   generated code without a named exception in this doc".
6. **Relation to Go etcd** — one paragraph: Go has no equivalent lint;
   every production etcd bug in this class (Raft index overflow
   regressions, revision arithmetic in the MVCC compactor) would have
   fired this lint at PR time in mango. The policy exists so mango
   doesn't rediscover those bugs.

Tone: terse, scannable, a contributor can find the answer in 30 seconds.
Target length: 120–180 lines of markdown.

### D2. `Cargo.toml` — enable the lint

Add one line to `[workspace.lints.clippy]`:

```toml
arithmetic_side_effects = { level = "deny", priority = 1 }
```

Ordering convention: keep it in the "Numerics" group next to the two
cast lints, per the comment groupings already established in PR #10.
The comment block above the deny line points to the policy doc:
`# arithmetic policy: see docs/arithmetic-policy.md` so future readers
follow the trail rather than re-deriving the rationale from scratch.

### D3. `crates/mango/src/lib.rs` — test-mod allow refresh

Tests routinely write ad-hoc arithmetic — `counter += 1`, `idx + 1` on
a slice, `Duration::from_millis(100) * n` — and shouldn't be forced to
pick a variant for throwaway assertion setup. The lint's value is at
protocol-counter boundaries in production code, not in test
scaffolding. Add `clippy::arithmetic_side_effects` to the test-mod
inner-allow so the first future test with any arithmetic doesn't also
have to file a one-line lint-escape PR.

Updated test-mod allow block:

```rust
#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::unnecessary_literal_unwrap,
        clippy::arithmetic_side_effects
    )]
    ...
}
```

Also update the `Cargo.toml:24-27` comment that rust-expert flagged as
a nit in PR #10 (says "the four that tests legitimately need" but the
allow block has five — now it will have six). Drop the explicit list
entirely so it can't drift again: "test modules override the lints
that tests legitimately need via an inner `#![allow(...)]`; see
`crates/mango/src/lib.rs`."

## Files to touch

- `docs/arithmetic-policy.md` — NEW (target ~150 lines markdown).
- `Cargo.toml` — add `arithmetic_side_effects` deny; fix the nit comment.
- `crates/mango/src/lib.rs` — add `clippy::arithmetic_side_effects` to
  the test-mod inner-allow.

No other code changes — the policy is forward-looking and the placeholder
crate has no arithmetic to retrofit.

## Edge cases

- **Existing code triggers the lint** — the only arithmetic today is in
  `assert_eq!(VERSION, "0.1.0")` which is a string comparison, no
  integer math. Verified by grep: zero `+` / `-` / `*` / `/` / `%`
  operators on integer types in `crates/mango/src/`. Manual audit
  before commit; re-run `cargo clippy --workspace --all-targets
--locked -- -D warnings` as proof.
- **`env!("CARGO_PKG_VERSION")`** — no arithmetic, just a compile-time
  string. Safe.
- **Doctests** — same story as PR #10: rustdoc compiles doctests, not
  clippy-driver, so workspace denies don't reach them. No doctests
  with arithmetic exist today. Filed as part of the future
  `cargo clippy --doc` CI step.
- **Const generics / const fns** — `arithmetic_side_effects` fires in
  `const fn` bodies even when the compiler would reject overflow at
  CTFE. None in the codebase today. The policy's `#[allow]` rule (b)
  steers future contributors to a const-block `.checked_add(...)
.expect("...")` pattern first (catches at compile time, no
  `#[allow]` needed); `#[allow]` only when the const-block pattern
  isn't viable.
- **`usize` slice-index math in Raft log / MVCC** — the lint's biggest
  day-to-day surface will be `len() - 1`, `idx + 1`, `end - start`.
  None in the codebase today. The policy covers this explicitly with
  the `// BOUND:` + narrow `#[allow]` escape (rule d) so the first
  such PR isn't blocked on rediscovering the pattern.
- **`Instant + Duration` deadline math vs `Duration + Duration` budget
  math** — both flow through operator `+`, both trip the lint, but
  they need different variants. Deadline math uses
  `Instant::checked_add`; budget math uses `Duration::saturating_*`.
  The TL;DR table and the reviewer checklist call out the split
  separately.
- **Atomics** — `AtomicU64::fetch_add` is a method call, not an
  operator; `arithmetic_side_effects` does not fire on it. The policy
  tells contributors to audit atomic-counter call sites manually,
  wrapping in a `checked_fetch_add` helper for protocol counters. No
  atomics today; filed for the first Raft / MVCC PR that uses them.
- **Build scripts** — no `build.rs` today. `mango-proto` will add one
  and any generated-code overflow noise is handled at the generated-
  module boundary per PR #10's plan.
- **Dependency crates** — `[workspace.lints]` only applies to workspace
  members, not external crates. A dep's internal arithmetic is its own
  problem; we'd surface it via a `checked_arith` review of deps in the
  concurrency-primitive-ban PR (next roadmap item) if we find one.

## Test strategy

Two-part verification. Follows the same playbook as PR #10: the change
is mostly a CI gate and a doc, so the CI gate IS the persistent test,
and a one-off audit confirms the mechanism works at merge time.

1. **Existing test stays green** — `cargo test --workspace --all-targets
--locked` passes, proving we didn't accidentally break the
   placeholder test by flipping the lint on.
2. **Local violation injection for the one NEW lint** — scratch file
   with exactly one `let x: u64 = a + b;` on variable operands, run
   `cargo clippy --workspace --all-targets --locked -- -D warnings`,
   confirm `-D clippy::arithmetic-side-effects` fires red, then remove
   the scratch. Document the clippy error excerpt in the PR body.
3. **CI green** — fmt / clippy / test all pass on the PR.

**Why no new persistent test**: the CI `cargo clippy -- -D warnings`
job is the persistent regression gate, same as PR #10. A richer
mechanism test (script that injects a violation and asserts the lint
name fires) is filed as a future hardening item, not scope for this
PR. Documented explicitly in the PR body; the "tests mandatory"
discipline is satisfied by "the CI gate IS the test" for config-only
changes.

**Doc-specific verification**: read the written policy end-to-end as
if I were a new contributor, check that the TL;DR table answers "what
do I use for a Raft index?" in under 15 seconds. If the answer isn't
obvious from the table alone, the doc has failed.

## Rollback

Single squash commit. Revert the commit; clippy returns to the pre-PR
posture (no `arithmetic_side_effects` enforcement), the doc disappears,
test-mod allow shrinks by one entry. Zero behavioral impact — no code
depends on the policy doc's presence.

## Out of scope (explicit, do not do in this PR)

- **Retrofitting existing arithmetic** — there is no arithmetic to
  retrofit; the codebase has zero integer ops today.
- **A `checked-arith` helper crate** — the policy tells contributors
  to reach for stdlib `checked_*` / `saturating_*` / `wrapping_*`. A
  helper crate would be premature abstraction; revisit if three crates
  end up with identical boilerplate.
- **Concurrency-primitive ban** (`ROADMAP.md:752`) — next roadmap item.
- **Release-profile `overflow-checks = true`** (`ROADMAP.md:753`) —
  separate roadmap item; it's the runtime check, orthogonal to the
  compile-time lint.
- **`CONTRIBUTING.md` cross-reference** — the arithmetic policy will
  be linked from `CONTRIBUTING.md` when that lands (`ROADMAP.md:761`);
  this PR only ships the policy doc itself.
- **ROADMAP checkbox flip** — separate commit to main per the workflow.

## Risks

- **"Policy drift"** — the policy doc grows stale as the codebase
  evolves. Mitigation: the policy is linked from `CONTRIBUTING.md` and
  the PR template (both landing in this phase), and every PR that
  introduces arithmetic is reviewed against it. A quarterly-ish audit
  is filed as a future reliability-hardening item.
- **False positives in `const fn`** — observed historically on stable
  clippy. If it fires on a `const fn` we'll add the narrowest possible
  `#[allow]` per rule (b) in the doc.
- **Contributor friction** — the lint forces explicit arithmetic
  variants, which feels verbose. That IS the point: the cost is one
  keystroke at write time, the benefit is zero silent overflow bugs in
  production. Documented in the "Why each" section so reviewers can
  point to it.
