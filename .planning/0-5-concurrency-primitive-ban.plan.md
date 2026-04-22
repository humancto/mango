# Plan: concurrency-primitive ban via `clippy::disallowed_types`

Roadmap item: Phase 0 — "Concurrency-primitive ban via
`clippy::disallowed_types`: `clippy.toml` (workspace root) declares
`disallowed-types = [{ path = "std::sync::Mutex", ... }, { path =
"std::sync::RwLock", ... }]`. Correct mechanism — `cargo-deny` bans
crates, not stdlib type uses; `clippy::disallowed_types` operates at
the type level inside any source file." (`ROADMAP.md:752`)

## Goal

Ban `std::sync::Mutex` and `std::sync::RwLock` at the type level across
the workspace, with the error message pointing contributors at the
correct replacement (`parking_lot::*` for sync contexts,
`tokio::sync::*` for async contexts). The `std` primitives have two
foot-guns:

1. **Poisoning** — a panic while holding the lock turns every future
   `.lock()` call into a `Result<_, PoisonError<_>>`. The usual
   response is `.unwrap()` (banned by PR #10) or `.expect()` (also
   banned). The correct response is "don't use a primitive that
   poisons in the first place."
2. **No async awareness** — `std::sync::Mutex` held across an `.await`
   is already caught by `clippy::await_holding_lock` (enabled in
   PR #10), but the fix ("use `tokio::sync::Mutex`") requires
   rewriting the type, which the ban drives contributors to do from
   the start instead of after a clippy-driver smackdown.

## North-star axis

**Concurrency + Reliability.** Paired with the async-lock lint from
PR #10, this closes the "I reached for `std::sync::Mutex` because it's
what I learned in rustlings" path entirely. Go etcd has no equivalent
— it uses `sync.Mutex` for everything and eats the async-context cost.
Mango's stance from day one is that the choice between sync lock,
async lock, and lock-free is explicit.

## Approach

One new file at workspace root. Roadmap text gives the exact contents.

### D1. `clippy.toml` (NEW)

```toml
# Concurrency-primitive ban. See ROADMAP.md:752 and
# docs/arithmetic-policy.md for the sibling arithmetic ban. The two
# together force contributors to choose the right primitive
# deliberately, not reach for std defaults that carry poisoning and
# async-incompatibility foot-guns.
disallowed-types = [
    { path = "std::sync::Mutex", reason = "use parking_lot::Mutex (no poisoning, faster) or tokio::sync::Mutex (async-aware); both bypass std's poisoning footgun" },
    { path = "std::sync::RwLock", reason = "use parking_lot::RwLock or tokio::sync::RwLock for the same reason" },
]
```

Format note: `clippy.toml` at workspace root is auto-picked up by
`cargo clippy` for every workspace member. The key is
`disallowed-types` (kebab-case in TOML); the lint itself is
`clippy::disallowed_types` (snake_case). Both forms are documented in
the clippy book and the kebab form is the stable TOML spelling.

### D2. Enable the lint in `Cargo.toml`

The `clippy.toml` file supplies the _config_ for the lint, but the
lint itself must be **enabled at deny level** in
`[workspace.lints.clippy]` or it silently does nothing. This is the
trap in the roadmap text that a reader might miss: `clippy.toml`
configures the list; `Cargo.toml` turns the enforcement on.

Add to `[workspace.lints.clippy]`:

```toml
disallowed_types = { level = "deny", priority = 1 }
```

Ordered in the "Concurrency / async correctness" group next to
`await_holding_lock` / `await_holding_refcell_ref`.

## Files to touch

- `clippy.toml` — NEW at workspace root (~15 lines).
- `Cargo.toml` — add `disallowed_types = { level = "deny", priority = 1 }`.

No code changes — the codebase has no uses of `std::sync::Mutex` or
`std::sync::RwLock` today. Verified by grep before commit.

## Edge cases

- **`std::sync::Arc` / `std::sync::atomic::*`** — explicitly NOT
  banned. `Arc` is the canonical shared-ownership primitive and atomics
  are lock-free and have no poisoning story. Only the poisoning
  primitives are in scope.
- **`std::sync::OnceLock` / `std::sync::LazyLock`** — NOT banned.
  These don't poison in the same way (they are one-shot init
  primitives). Using `parking_lot::OnceCell` is fine as an alternative
  but the std versions are acceptable.
- **`std::sync::Condvar`** — NOT banned explicitly today. Paired with
  the allowed primitives (there's no "banned" Mutex to pair with once
  this lands, so Condvar is effectively orphaned). A future hardening
  item can ban `Condvar` in favor of `tokio::sync::Notify` /
  channel-based signaling, but that's scope-creep for tonight.
- **Third-party crates that use `std::sync::Mutex` internally** — the
  lint operates at type references in workspace source, not in
  dependencies. A dep can use whatever it likes. That's the right
  boundary; the `[workspace.lints]` surface only controls what we
  write.
- **Test code** — unlike `arithmetic_side_effects` and `unwrap_used`,
  there is no test-specific reason to allow `std::sync::Mutex`. A
  test that needs a Mutex is better off using `parking_lot::Mutex`
  anyway (no poisoning means cleaner test teardown). **Do not** add
  this lint to the test-mod inner-allow.
- **Examples and doctests** — rustdoc doesn't run clippy on doctests
  per the PR #10 scar, so doctests could technically use
  `std::sync::Mutex` without the lint firing. The policy doc (future
  item) should mention this if doctests end up demonstrating lock
  usage.

## Test strategy

Same framing as PRs #10 and #11: config-only change, CI gate IS the
test. Two-part verification at merge time:

1. **Existing test stays green** — `cargo test --workspace
--all-targets --locked` still passes.
2. **Violation-injection audit** — add one `use std::sync::Mutex;
static _M: Mutex<u32> = Mutex::new(0);` to a scratch file, run
   `cargo clippy --workspace --all-targets --locked -- -D warnings`,
   confirm `-D clippy::disallowed-types` fires red with the configured
   reason, remove the scratch. Document the clippy error excerpt in
   the PR body — including the reason string, since the whole point
   of this lint (vs a blanket ban) is that the error message tells the
   contributor which replacement to use.
3. **CI green** — `fmt` / `clippy` / `test` all pass.

**Why no new persistent test**: CI `cargo clippy -- -D warnings` is
the gate, same pattern as PR #10 and #11. The audit proves the
mechanism works at merge time; CI proves it keeps working on every
subsequent commit. Filed the "integration script that injects a
violation and asserts the specific lint name" as a future hardening
item (same future item as PR #10 / #11's).

## Rollback

Single squash commit. Revert → `clippy.toml` disappears, `Cargo.toml`
loses one line, clippy stops flagging `std::sync::Mutex`. Zero
behavioral impact on any runtime code.

## Out of scope (explicit, do not do in this PR)

- **Add `parking_lot` / `tokio` as workspace deps** — no workspace
  crate uses a lock today. When the first Raft / storage PR introduces
  shared state, that PR picks the right primitive and adds the
  appropriate dep.
- **Ban `std::sync::Condvar`** — future hardening item.
- **Ban `Rc<RefCell<_>>` patterns** — not addressed by
  `disallowed_types` easily (it's a composition, not a type).
  Different mechanism needed; out of scope.
- **`cargo-deny` crate bans** (`ROADMAP.md:754`) — separate item
  (banning crates, not stdlib types).
- **Release-profile `overflow-checks = true`** (`ROADMAP.md:753`) —
  separate item.
- **ROADMAP checkbox flip** — separate commit to main per workflow.

## Risks

- **`clippy.toml` without the matching `Cargo.toml` entry is a no-op**
  — the trap the roadmap text hints at. The violation-injection audit
  catches this at merge time: if the lint doesn't fire, the config is
  wrong. Both files in one commit.
- **Lint level precedence** — `disallowed_types` is part of the
  `clippy::style` group at warn by default. We flip it to `deny` with
  explicit `priority = 1` per the convention established in PR #10.
  No group-priority arithmetic concerns.
- **Contributor confusion on `parking_lot` vs `tokio::sync`** — the
  reason strings in `clippy.toml` name both alternatives and point to
  "async context → `tokio::sync`" / "sync context → `parking_lot`".
  The error message carries the guidance.
