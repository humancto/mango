# Plan: workspace lint hardening (13 clippy denies)

Roadmap item: Phase 0 — "Lint hardening: workspace `Cargo.toml`
`[workspace.lints.clippy]` table denies [12 lints] in non-test code;
`#[cfg_attr(test, allow(...))]` at test-module boundaries"
(`ROADMAP.md:750`).

Status: **revised after rust-expert review** (verdict: REVISE; showstopper
on compile-fail doctest was addressed — the test strategy no longer
relies on a mechanism that clippy does not run against. Missing
`await_holding_refcell_ref` sibling lint folded in, making the count
13 denies; explicit `priority = 1` adopted for future-proofing;
`unnecessary_literal_unwrap` added to test-allow.)

## Goal

Lift the dozen-plus most common Rust foot-guns from "caught in review,
maybe" to "fails CI, always". The denies are the load-bearing mechanism
behind several north-star bars:

- **Reliability** (no `panic!`/`unwrap`/`expect`/`todo`/`unimplemented`
  in non-test code → no silent `unreachable` → no tail-latency spike
  from an `unwrap` on a `None` in a handler).
- **Correctness** (`indexing_slicing` forces `.get()` which returns
  `Option` that must be handled).
- **Concurrency** (`await_holding_lock` + `await_holding_refcell_ref`
  catch the #1 async/sync-state footguns, paired with the later
  `clippy::disallowed_types` ban on `std::sync::Mutex`).
- **Operability** (`print_stdout`/`print_stderr`/`dbg_macro` force all
  diagnostic output through `tracing`, making logs structured by default).

`clippy::arithmetic_side_effects` is **intentionally excluded** from
this PR — the roadmap note at `ROADMAP.md:750` explicitly says so, and
the next item (workspace arithmetic-primitive policy) must land first
to avoid an `#[allow]` retrofit wave.

## Approach

Two file touches. No code logic changes.

### 1. `Cargo.toml` — `[workspace.lints.clippy]`

Current state:

```toml
[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
pedantic = { level = "warn", priority = -1 }
module_name_repetitions = "allow"
missing_errors_doc = "allow"
missing_panics_doc = "allow"
```

Add 13 denies. Each is spelled with explicit `priority = 1` so the
intent cleanly beats the group-level `priority = -1`. That is hedged
future-proofing per rust-expert nit — plain `"deny"` also works today,
but explicit priority removes any ambiguity if cargo's precedence rules
ever nudge. Six of the 13 (`unwrap_used`, `expect_used`, `panic`,
`unimplemented`, `todo`, `dbg_macro`, `print_stdout`, `print_stderr`,
`await_holding_lock`, `await_holding_refcell_ref`) are in the
`restriction` group which isn't included in `all`/`pedantic`, so
priority doesn't technically matter for them; keeping it uniform
across all 13 is a legibility call.

```toml
[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
pedantic = { level = "warn", priority = -1 }
module_name_repetitions = "allow"
missing_errors_doc = "allow"
missing_panics_doc = "allow"

# Hardening — foot-guns denied workspace-wide. Test modules override
# the four that tests legitimately need (unwrap_used, expect_used,
# panic, indexing_slicing) via an inner `#![allow(...)]` at the test-
# mod boundary. The rest stay denied even in tests — no reason to
# `todo!()`, `dbg!()`, or hold a lock across `.await` in a test.
#
# `arithmetic_side_effects` is intentionally NOT here — it requires
# the workspace arithmetic-primitive policy (next roadmap item) to
# land first. Turning it on earlier would force every counter
# increment to carry an `#[allow]` and defeat the lint's purpose.
unwrap_used = { level = "deny", priority = 1 }
expect_used = { level = "deny", priority = 1 }
panic = { level = "deny", priority = 1 }
unimplemented = { level = "deny", priority = 1 }
todo = { level = "deny", priority = 1 }
indexing_slicing = { level = "deny", priority = 1 }
cast_possible_truncation = { level = "deny", priority = 1 }
cast_sign_loss = { level = "deny", priority = 1 }
dbg_macro = { level = "deny", priority = 1 }
print_stdout = { level = "deny", priority = 1 }
print_stderr = { level = "deny", priority = 1 }
await_holding_lock = { level = "deny", priority = 1 }
await_holding_refcell_ref = { level = "deny", priority = 1 }
```

### 2. `crates/mango/src/lib.rs` — test-mod override

The placeholder crate's test mod currently contains only `assert_eq!`
which doesn't trip any of the denies today. Proactively add the
standard test-mod override _now_ so the first future test that writes
`foo.unwrap()` doesn't also have to add a one-line lint-escape PR.

The roadmap text prescribes `#[cfg_attr(test, allow(...))]` as the
pattern. Inside a `#[cfg(test)] mod tests { ... }` that is tautological
(the whole mod only exists when `test` holds, so the `cfg_attr`
predicate is always true there). Use the cleaner inner-attr form:

```rust
#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::unnecessary_literal_unwrap,
    )]
    ...
}
```

Five lints allowed:

- `unwrap_used`, `expect_used`, `panic`, `indexing_slicing` — standard
  test-mod fare (roadmap item literal).
- `unnecessary_literal_unwrap` — proactive; clippy's `pedantic` group
  fires this on `Some(x).unwrap()` patterns common in test setup. Added
  now to save a one-line follow-up PR when the first test with a
  literal-wrapped unwrap lands.

The rest (`unimplemented`, `todo`, `dbg_macro`, `print_stdout`,
`print_stderr`, `await_holding_lock`, `await_holding_refcell_ref`, the
two casts) stay denied — no legitimate test calls `todo!()` or leaves a
`dbg!()` in, and holding a lock/refcell across `.await` is a correctness
bug regardless of cfg.

Divergence from the literal roadmap text (outer `cfg_attr` vs inner
`allow`) noted in the PR body so the reviewer can object if they want
the literal form.

## Files to touch

- `Cargo.toml` — append 13 priority-explicit denies to
  `[workspace.lints.clippy]`.
- `crates/mango/src/lib.rs` — add inner-`allow` block to the `tests`
  mod.

No new files. No workflow changes.

## Edge cases

- **Existing placeholder crate triggers a lint** — manually audited.
  `pub const VERSION: &str = env!("CARGO_PKG_VERSION");` uses `env!`,
  not any denied macro. `assert_eq!` does expand to a panic but
  `clippy::panic` fires on literal `panic!()` calls, not `assert*!`
  expansions (confirmed by rust-expert's dry-run on current stable
  clippy). No violations expected.
- **Priority arithmetic** — rust-expert's dry-run confirmed that
  `priority = 1` on each individual deny cleanly overrides `all` /
  `pedantic` groups at `priority = -1`. For the restriction-group
  lints (`unwrap_used`, `expect_used`, `panic`, `unimplemented`,
  `todo`, `dbg_macro`, `print_stdout`, `print_stderr`,
  `await_holding_lock`, `await_holding_refcell_ref`), there is no
  group-level entry to override — they're activated from scratch.
- **Doctests** — `cargo test --doc` compiles doctests with rustdoc
  (rustc, not clippy-driver), so clippy workspace denies do NOT run on
  doctests by default. `cargo clippy --doc` (stabilized 1.75+) does,
  and can be added to CI as a separate step once doctests exist with
  real logic. Placeholder crate has no doctests; noted for future
  doctest-bearing PRs.
- **Integration tests under `tests/`** — each integration test crate
  inherits `[workspace.lints]` via the owning crate's
  `[lints] workspace = true` block. The same inner-`allow` pattern
  at the integration-test file top-level works.
- **Future-crate bootstrap discipline** — any new `crates/*/Cargo.toml`
  MUST contain `[lints] workspace = true`, or it silently opts out of
  every workspace lint including all 13 of these. Documented in
  `CONTRIBUTING.md` when that item (`ROADMAP.md:761`) lands. A CI
  grep-check can enforce it mechanically; filed as a future item,
  not scope-creep for tonight.
- **Build scripts** — `build.rs` inherits workspace lints. No
  `build.rs` today; `mango-proto` (`ROADMAP.md:760`) will add one with
  `tonic-build` and that PR will handle any generated-code allows at
  the generated-module boundary.
- **Generated code from tonic-build / prost** — typically emitted to
  `OUT_DIR` outside the workspace source tree; workspace lints don't
  reach there. If a future plan generates into `src/generated/`, that
  PR will add `#[allow]` on the generated module or a `rustfmt.toml`
  `ignore = [...]` entry.

## Test strategy

The change _is_ a CI gate, and the honest answer is that CI's
`cargo clippy --workspace --all-targets --locked -- -D warnings` job
is the persistent regression gate. A lint-config regression (someone
flipping a `deny` back to `warn`) would not trip any test by itself,
but _any subsequent commit that introduces a violation_ would fail
clippy in CI — which is exactly when the protection matters.

Three-part verification for this PR specifically:

1. **Existing test stays green** — `cargo test --workspace
--all-targets --locked` passes, proving the placeholder isn't
   accidentally caught by a new deny.
2. **Local violation injection** — before committing, temporarily add
   one violation of each of the 13 denied lints to a scratch file
   under the crate, run `cargo clippy --workspace --all-targets
--locked -- -D warnings`, confirm each fires red with the expected
   lint name, then remove. Document in the PR body which violations
   were tested with verbatim excerpts of the clippy output. This is
   a one-off manual audit; its purpose is proving the mechanism works
   _today_ at the moment of merge.
3. **CI green** — `fmt` / `clippy` / `test` all pass on the PR.

An earlier plan draft proposed a `compile_fail` doctest as a
_persistent_ regression test. **Dropped after rust-expert review**:
doctests are compiled by rustdoc, not clippy-driver, so
`[workspace.lints.clippy]` denies never fire on them. The doctest
would never fail-to-compile from a clippy lint, which means
`compile_fail` would mark the test red on every commit regardless of
lint state — providing zero regression-detection signal. The CI
clippy job remains the real persistent gate; a richer mechanism test
(integration script that injects a violation, runs clippy, asserts
the specific lint name appears) is filed as a future hardening item,
not scope for tonight.

This test plan is the documented exception to the "every PR ships with
a persistent test" rule: the persistent gate is CI clippy itself,
not a new test binary. The one-off manual audit is the evidence that
the gate is configured correctly at merge time.

## Rollback

Additive, pure config + one test-mod allow block. Revert the commit;
clippy returns to the previous (group-warn-only) posture.

## Out of scope (explicit, do not do in this PR)

- `clippy::arithmetic_side_effects` — deferred per roadmap text; lands
  after the arithmetic-primitive policy doc (next roadmap item).
- `trybuild` dev-dep for richer compile-fail coverage — defer to the
  first Phase 1 crate that has >1 lint-enforcement test.
- `clippy::disallowed_types` / `clippy::disallowed_macros` bans —
  separate roadmap item at `ROADMAP.md:752`.
- `[profile.release] overflow-checks = true` — separate roadmap item
  at `ROADMAP.md:753`.
- `cargo clippy --doc` CI step — defer until doctests with real logic
  exist.
- Per-crate `[lints] workspace = true` CI-grep check — file as a
  future hardening item.
- README / ROADMAP touches in this PR — the checkbox flip happens in
  a separate commit to main per the workflow rule.

## Disagreements with reviewer

None. All four rust-expert action items were folded in. The manual
one-off audit (test-strategy item 2) was already in the plan; the
showstopper fix was dropping item 3, not defending it.
