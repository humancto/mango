# Plan: release-profile overflow checks

Roadmap item: Phase 0 — "Release-profile overflow checks:
`[profile.release] overflow-checks = true` in workspace `Cargo.toml`.
Catches arithmetic panics in production, not just debug. Documented
trade-off (~1-3% perf hit) accepted." (`ROADMAP.md:753`)

## Goal

Turn on runtime overflow checks in release builds. Rust's default is
debug-only — release builds silently wrap, which is exactly the
silent-corruption path the Phase 0 arithmetic work (PRs #11) was built
to prevent. The clippy-compile-time lint catches it at PR time; this
runtime flag catches anything that slipped past the lint (a
`#[allow]`-marked site, a macro-generated arithmetic, an atomic
`fetch_add` that's not linted).

Defense in depth: compile-time discipline via `arithmetic_side_effects`
lint + policy doc, runtime backstop via `overflow-checks = true`.

## North-star axis

**Reliability + Correctness.** The compile-time lint is the primary
gate; this is the secondary gate. In Go etcd, arithmetic overflow is a
wrap in both debug and release (Go has no overflow-check mode at all).
In mango, with this flag on, an overflow that slips past the lint
panics the process instead of silently producing a wrong Raft index or
revision — and **that panic is a recoverable event** because the
crash-only design declaration (`ROADMAP.md:759`, future item) says
process restart is equivalent to crash recovery. A process that
overflows is a process that panics and restarts into a consistent
state, not one that silently emits garbage.

## Approach

One `Cargo.toml` edit. Total: one line added, one comment block.

### `Cargo.toml` — add to `[profile.release]`

Current state (`Cargo.toml:45-49`):

```toml
[profile.release]
lto = "thin"
codegen-units = 1
panic = "abort"
strip = "symbols"
```

Adds one line:

```toml
[profile.release]
lto = "thin"
codegen-units = 1
panic = "abort"
strip = "symbols"
overflow-checks = true
```

Plus a `#` comment block above the profile block explaining the
trade-off (~1-3% perf hit on arithmetic-heavy hot paths, accepted for
correctness, revisit in Phase 14 perf work if a specific hot path is
bottlenecked).

**`panic = "abort"` interaction**: already set. An overflow panic in
release therefore aborts the process. Combined with systemd / k8s
restart policy (future operability item), the process comes back
clean. This is the desired behavior — a wedged cluster from silent
overflow is worse than a momentary unavailable node.

## Files to touch

- `Cargo.toml` — add `overflow-checks = true` to `[profile.release]`
  with a comment explaining the trade-off.

No code changes. No doc changes (the arithmetic policy doc already
names this as the runtime backstop, implicitly).

## Edge cases

- **Perf impact**: the Rust language reference puts the cost at
  typically 1-3% on arithmetic-heavy hot paths, higher on extreme
  micro-benchmarks (SIMD-dense loops). Mango's hot paths are storage
  I/O, serialization, and network — arithmetic is not the bottleneck.
  The cost is real but not a gate on shipping; any future hot-path
  arithmetic that measurably regresses can `#[inline(always)]` a
  `wrapping_*` block with a `// BOUND:` comment per the arithmetic
  policy's `#[allow]` rule (a).
- **Interaction with `clippy::arithmetic_side_effects`**: orthogonal.
  The clippy lint catches at PR time; `overflow-checks` catches at
  runtime. A site that's `#[allow]`-marked at compile time (e.g., a
  bounded loop index) will still get the runtime check. If the bound
  proof in the `// BOUND:` comment is wrong, the runtime check
  converts a silent wrap into a panic — which is the right failure
  mode.
- **`wrapping_*` intrinsics are exempt**: `u64::wrapping_add` and
  friends do NOT trip overflow-checks at runtime. That's their
  contract. Hash / CRC / modular code that legitimately needs
  wrap-around uses these explicit intrinsics and is unaffected.
- **`saturating_*` intrinsics are exempt**: same story. They clamp
  rather than wrap; no panic.
- **`checked_*` intrinsics are exempt**: they return
  `Option`/`Result`; no panic.
- **`debug_assert!` and `debug_assertions` cfg**: `overflow-checks` is
  a separate flag from `debug-assertions`. The profile already has
  `debug-assertions` at its default (on in dev, off in release); this
  PR only touches `overflow-checks`.
- **Workspace inheritance**: `[profile.release]` at the workspace root
  applies to every workspace member crate per cargo's profile
  inheritance rules. No per-crate override.
- **Dependencies**: `overflow-checks` in the workspace profile also
  applies to deps compiled with this workspace. Upstream crates that
  rely on release-mode wrap (rare but non-zero) would panic. Mitigation:
  if a specific dep proves to need wrap, a targeted
  `[profile.release.package.<name>] overflow-checks = false` override
  is the surgical fix. No known case today; revisit if CI fails on a
  future dep bump.

## Test strategy

Config-only change. Same framing as PRs #10, #11, #12.

1. **Existing test stays green** — `cargo test --workspace
--all-targets --locked` passes (debug build; unaffected).
2. **Release build compiles and runs** — `cargo build --release
--workspace` succeeds, `cargo test --release --workspace --locked`
   passes (proves the setting parses and doesn't break the release
   compilation path).
3. **Behavioral audit** — one-off write-and-discard: a scratch function
   that deliberately overflows a `u64` at runtime, compiled with
   `--release`, confirms the process panics instead of wrapping. The
   scratch is removed before commit; the point is to prove the flag is
   live, not to ship a persistent test (a persistent test would depend
   on process-abort semantics + cfg gates that add scope for zero
   additional gate value).
4. **CI green** — existing `fmt`/`clippy`/`test` jobs all pass. The
   test job runs in debug mode by default, which is unaffected; the
   release behavior is audited manually per step 3.

**Why no new persistent test**: the CI test matrix doesn't run release
builds today (that's a future CI-hardening item — see `ROADMAP.md:748`
flow but not explicitly listed for `--release`). Adding a release-mode
test job is scope-creep for this PR; it's filed as a future CI
hardening item. The one-off audit proves the flag is live at merge
time, which is the bar established by PRs #10/#11/#12 for config
changes.

## Rollback

Single squash commit. Revert → `overflow-checks` returns to its
default (`false` in release). Zero behavioral impact on runtime
correctness (the compile-time lint remains the primary gate; you'd
just lose the runtime backstop).

## Out of scope (explicit, do not do in this PR)

- **Release-mode CI job** — filed as future CI hardening; not scope.
- **Per-dep overrides** — none needed today; add surgically if a dep
  bump exposes one.
- **`debug-assertions = true` in release** — separate flag, separate
  trade-off, not named in the roadmap item. Out of scope.
- **ROADMAP checkbox flip** — separate commit to main per workflow.

## Risks

- **A dep crate relies on release-mode wrap** — see edge cases;
  surgical per-dep override is the fix. Not known to happen today;
  watch for CI fallout on the first post-Phase-0 dep bump.
- **Measurable perf regression in a future hot path** — mitigation is
  named in the arithmetic policy (narrow `wrapping_*` with
  `// BOUND:`). No hot paths today.
