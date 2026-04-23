# Loom policy

[loom](https://github.com/tokio-rs/loom) is mango's concurrency
model-checker. Any shared-state primitive that manipulates atomics,
`UnsafeCell`, or lock-free data structures **must** ship with a loom
model exercising its ordering invariants before it can land on a
Phase 3+ branch.

This doc is the policy. The canonical template lives at
[`crates/mango-loom-demo/`](../crates/mango-loom-demo/); the CI
enforcement is [`.github/workflows/loom.yml`](../.github/workflows/loom.yml).

## Why loom

Concurrent code passes `cargo test` by luck on x86. The TSO memory
model of x86 hides a wide class of ordering bugs that only surface
on weaker architectures (ARM, RISC-V, POWER) or under aggressive
compiler reordering. loom is a sequentially-consistent model checker
that _deliberately_ explores interleavings permitted by the C++20
memory model — the same model `std::sync::atomic` compiles down to.

Under `--cfg loom`:

- `std::thread::spawn` is replaced by `loom::thread::spawn`, which
  yields control to the model scheduler at every synchronization
  primitive.
- `std::sync::atomic::*` types are replaced by loom's, which track
  happens-before chains and flag causality violations.
- `std::cell::UnsafeCell` is replaced by `loom::cell::UnsafeCell`,
  which records accesses and flags data races the compiler can't.

If a loom model fails, it prints an interleaving that violates the
property. Fix the ordering until the model passes under
`LOOM_MAX_PREEMPTIONS=2`. This is the hard bar for merging any
lock-free primitive.

## Activation — `RUSTFLAGS`, not a Cargo feature

Loom is activated with `RUSTFLAGS="--cfg loom"`, not a Cargo feature.
This is the tokio / crossbeam / bytes convention. Cargo features
unify across the dependency graph, which would be a disaster for
loom — a downstream crate enabling `loom` would flip the activation
for every dep that uses it, recompiling the world into the model
layer even for builds that aren't loom runs.

Cfg gates are local to the crate that declares them. The demo
crate's `Cargo.toml` wires loom as a target-cfg dep so it's only
linked when `--cfg loom` is set:

```toml
[target.'cfg(loom)'.dependencies]
loom.workspace = true
```

Default builds pay zero loom cost. Under `--cfg loom`, loom is linked
and the demo's `#[cfg(loom)]` imports resolve.

## Canonical invocation

```bash
RUSTFLAGS="--cfg loom -C debug-assertions" \
  LOOM_MAX_PREEMPTIONS=2 \
  LOOM_MAX_BRANCHES=10000 \
  cargo nextest run --profile ci --release -p mango-loom-demo
```

Every flag is load-bearing:

| Flag                      | Why                                                                                                                                                                                                                   |
| ------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--cfg loom`              | Activates the `#[cfg(loom)]` gates in the crate. Without it, loom is not linked and the tests compile as ordinary threads — meaningless.                                                                              |
| `-C debug-assertions`     | Loom's aliasing and causality checks are gated on `debug_assertions`. `--release` silently neuters them otherwise — tests "pass" without actually checking anything.                                                  |
| `LOOM_MAX_PREEMPTIONS=2`  | Caps preemption points per model run. `2` is the tokio/crossbeam-standard smoke value — enough to catch realistic interleaving bugs without schedule-space explosion. Raise temporarily when chasing a suspected bug. |
| `LOOM_MAX_BRANCHES=10000` | Caps total branches explored per `loom::model(…)` call. Protects against a runaway model.                                                                                                                             |
| `--release`               | Loom itself recommends release mode; optimization can change Drop order and inlining, which can shift observable scheduling. `-C debug-assertions` keeps loom's internal assertions alive.                            |
| `--profile ci`            | Inherits the `30m` per-test watchdog from the `test(~loom)` filter in [`.config/nextest.toml`](../.config/nextest.toml).                                                                                              |
| `-p mango-loom-demo`      | Scopes to the demo crate. Phase 3+ additions will add more `-p <crate>` entries; the full-matrix invocation will eventually live in a Makefile recipe.                                                                |

Do not edit any of these flags without updating this doc and
[`.github/workflows/loom.yml`](../.github/workflows/loom.yml) in
the same commit.

## Loom environment variables (reference)

These are loom's runtime knobs. The defaults in the canonical
invocation above are tuned for CI; raise them during local debugging.

| Var                        | Default        | Purpose                                                                                                                                  |
| -------------------------- | -------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `LOOM_MAX_PREEMPTIONS`     | `2` (ours)     | Max preemptions per model. Higher → more interleavings, super-linear blow-up.                                                            |
| `LOOM_MAX_BRANCHES`        | `10000` (ours) | Hard cap on branches per `loom::model`. Aborts runaway models.                                                                           |
| `LOOM_MAX_DURATION`        | unset          | Wall-clock budget for a single model run (seconds). Use when chasing combinatorial explosion.                                            |
| `LOOM_MAX_PERMUTATIONS`    | unset          | Cap on permutations explored. Rarely needed.                                                                                             |
| `LOOM_CHECKPOINT_FILE`     | unset          | Write-through checkpoint; resume a long model from where it crashed.                                                                     |
| `LOOM_CHECKPOINT_INTERVAL` | unset          | Branches between checkpoint writes.                                                                                                      |
| `LOOM_LOCATION`            | unset          | Emit source locations for atomic ops in the failure trace. Turn on when a model fails and you need to map the interleaving back to code. |
| `LOOM_LOG`                 | unset          | Verbose scheduler trace (`trace`, `debug`, `info`). Loud; only use on a single failing test.                                             |

Full reference: [loom `Builder` docs](https://docs.rs/loom/latest/loom/model/struct.Builder.html).

## Writing a new loom test

1. Copy the shape of [`crates/mango-loom-demo/src/lib.rs`](../crates/mango-loom-demo/src/lib.rs).
   The pattern is: cfg-split imports, cfg-split `UnsafeCell` shim,
   loom-aware `.with()/.with_mut()` accessors on any guard type.
2. Put the test in `tests/loom_*.rs` with `#![cfg(loom)]` at the
   file head. The `test(~loom)` filter in `.config/nextest.toml`
   routes it into the 30-minute budget.
3. Wrap each model in `loom::model(|| { … })`. Everything inside
   the closure — `Arc::new`, `loom::thread::spawn`, `.join()` — runs
   once _per interleaving_, not once.
4. Use `loom::thread::spawn` (not `std::thread::spawn`) and
   `loom::sync::Arc` (or `std::sync::Arc` — std's is Sync under
   loom too, but the loom-native `Arc` integrates better).
5. Drive the model through `loom::thread::yield_now()` in spin
   loops. On the non-loom arm, use `std::hint::spin_loop()`.

### The critical rule: no raw-pointer escape hatch

Any code path that reaches `*ptr` on an `UnsafeCell`'s data must go
through `.with()` / `.with_mut()`. On the loom arm these are the
hooks loom uses to detect data races. A Deref impl that dereferences
the cell's raw pointer directly will work on std but be invisible
to loom — the exact "passes `cargo test`, broken under `--cfg loom`"
footgun this template is designed to prevent.

The demo's `Deref` / `DerefMut` impls are cfg-gated to the non-loom
arm for this reason. Under loom, tests are forced to use
`.with()` / `.with_mut()`.

### Sanity-break every model

Before merging, prove the model is actually checking something:
flip each `Ordering::Acquire` to `Relaxed`, flip each
`Ordering::Release` to `Relaxed`, run loom, and confirm the model
**fails** with a causality violation. Restore the orderings. A model
that passes under both strong and weak orderings is not checking
what you think it's checking.

The demo was sanity-broken in both directions during development:

- `compare_exchange_weak(Acquire, Relaxed)` → `(Relaxed, Relaxed)`
  → loom reports ``Causality violation: Concurrent write accesses to `UnsafeCell`.`` (backticks around `UnsafeCell` are loom's verbatim output).
- `store(Release)` → `store(Relaxed)` → same violation.

## Version-bump procedure

`loom` is pinned exactly at the workspace level:

```toml
[workspace.dependencies]
loom = "=0.7.2"
```

The exact pin is load-bearing. Loom's env-var semantics, model
internals, and which interleavings are explored can drift between
minor versions. A `cargo update` that silently bumps loom can
change CI semantics without a diff anyone can review.

To bump:

1. Update the pin in the workspace `Cargo.toml`.
2. Run `cargo update -p loom`.
3. Re-run the canonical invocation locally and confirm the demo
   still passes.
4. **Sanity-break again.** New loom versions have occasionally
   weakened the detection — if the sanity breaks no longer fail the
   model, that's a red flag worth investigating before merging.
5. Scan loom's CHANGELOG for env-var renames or default changes.
   Update this doc and `.github/workflows/loom.yml` in the same
   commit as the bump.
6. PR review gate: the rust-expert agent must sign off on the
   bump diff.

## Debugging a failed model

When loom reports a causality violation, the output is a schedule
— a sequence of thread steps interleaved with atomic ops. The
default output is terse. To make it readable:

1. Set `LOOM_LOCATION=1` — annotates each step with the source
   location of the atomic op or thread spawn. Essential for mapping
   the schedule back to code.
2. Drop `LOOM_MAX_BRANCHES` to `100` — loom explores in a
   roughly-deterministic order, so the first failing schedule is
   usually near the start of the exploration. A tight cap gets you
   to it faster.
3. Drop `LOOM_MAX_PREEMPTIONS` to `1` — if the model fails at 1
   preemption, the bug is a _simpler_ violation than a 2-preemption
   one. Fixing the 1-preemption case often fixes the 2-preemption
   one.
4. Run a single model: `cargo nextest run -E 'test(my_model)'`.

If loom still reports a violation you don't understand, turn on
`LOOM_LOG=trace` on a single model. The output is extremely verbose
— capture it to a file.

## Non-negotiables (pre-merge checklist)

For any PR that adds or modifies a shared-state primitive:

- [ ] A loom model exists and lives in `tests/loom_*.rs` or a
      loom-named module.
- [ ] The model passes under the canonical invocation
      (`LOOM_MAX_PREEMPTIONS=2`).
- [ ] The model has been sanity-broken at least once per ordering
      it's supposed to verify.
- [ ] Any `Ordering::Relaxed` has a comment justifying why weaker
      than `Acquire`/`Release` is correct.
- [ ] Any `UnsafeCell` access goes through `.with()` / `.with_mut()`.
- [ ] The PR description links the loom job run (green) on the
      final commit.

A PR without these is not mergeable.

## See also

- [`docs/testing.md`](testing.md) — the `loom` test class (30-minute
  watchdog) lives in the shared nextest policy.
- [`docs/miri.md`](miri.md) — Miri is the orthogonal UB-detection
  gate; loom verifies ordering, Miri verifies soundness of `unsafe`
  blocks. Both are required when Phase 3+ primitives ship `unsafe`
  and atomics together.
- [`CONTRIBUTING.md` §8](../CONTRIBUTING.md) — optional local loom
  invocation.
- [`ROADMAP.md` item 0.5.2](../ROADMAP.md) — where this policy was declared.
- [loom — tokio-rs/loom](https://github.com/tokio-rs/loom)
- [loom Builder env-var reference](https://docs.rs/loom/latest/loom/model/struct.Builder.html)
- The blog post series that led to loom:
  [_An Introduction to lock-free programming_ — Jeff Preshing](https://preshing.com/20120612/an-introduction-to-lock-free-programming/).
