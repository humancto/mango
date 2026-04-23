# Miri policy

[Miri](https://github.com/rust-lang/miri) is the interpreter for
Rust's MIR. It runs your tests inside an interpreter that detects
undefined behaviour (UB) — aliasing violations, invalid
provenance, uninitialized reads, leaks, invalid transmutes — that
a native binary would silently miscompile.

This doc is the policy. The CI enforcement is
[`.github/workflows/miri.yml`](../.github/workflows/miri.yml); the
curated-subset table is `[workspace.metadata.mango.miri]` in the
workspace
[`Cargo.toml`](../Cargo.toml); the helper scripts are
[`scripts/miri-crates.sh`](../scripts/miri-crates.sh) and
[`scripts/miri-changed-crates.sh`](../scripts/miri-changed-crates.sh).

## Why Miri (and why it's orthogonal to loom)

Miri and loom test different properties:

- **loom** explores interleavings under a sequentially-consistent
  memory model to verify ordering invariants. It answers: "does my
  atomic protocol observe the happens-before relations I claimed?"
- **Miri** interprets a single execution at a time under Rust's
  memory model to detect UB — aliasing, provenance, uninit reads,
  leaks, invalid transmutes. It answers: "is my `unsafe` block
  actually sound?"

Running Miri on `#[cfg(loom)]` tests is pointless, not impossible.
loom's atomics and cell shims are instrumented models; Miri
interpreting loom's instrumentation adds nothing Miri couldn't
already see in the underlying std code, and loom's model is not
the thing Miri is trying to check. For that reason the curated
subset in this project runs the `not(loom)` arm of the code under
Miri, and loom tests stay on their own CI workflow.

The two are complementary. If Phase 3+ primitives ship `unsafe` +
atomics, they need **both**: loom to verify ordering, Miri to
verify soundness of the `unsafe` blocks.

## The curated subset

Miri is slow. A full workspace Miri run is multiple orders of
magnitude slower than `cargo test` and is not the right signal on
every PR. Instead, Mango maintains a **curated subset** — the
crates that actually contain `unsafe` — and runs Miri against that
subset only.

The subset lives in the workspace manifest:

```toml
# Cargo.toml
[workspace.metadata.mango.miri]
crates = ["mango-loom-demo"]
```

Any crate that ships `unsafe` code (and therefore carries
`#![allow(unsafe_code)]` to opt out of the workspace
`unsafe_code = "forbid"` lint) **must name itself here in the same
PR**. The opt-in is explicit. This is a zero-cost piggyback on the
deliberate act of introducing `unsafe`: if you've already justified
why the `#![allow(unsafe_code)]` is worth it, adding one line to
the metadata table is not an additional friction.

A follow-up dylint (tracked as a GitHub issue) will enforce the
pairing at lint time. Until it lands, reviewer discipline is the
enforcement. A PR that introduces `unsafe` without also adding the
crate to the table should be REVISE-gated.

## MIRIFLAGS

| Flag                       | PR baseline | Nightly canary | Rationale                                                                                                             |
| -------------------------- | :---------: | :------------: | --------------------------------------------------------------------------------------------------------------------- |
| `-Zmiri-strict-provenance` |      ✓      |       ✓        | Rejects `usize -> ptr` casts that don't come from a `with_addr` / `expose` call. Catches provenance laundering.       |
| `-Zmiri-tree-borrows`      |             |       ✓        | Stricter aliasing model (Tree Borrows) — successor to Stacked Borrows. Still documented as experimental; canary-only. |

PR-blocking flags: `-Zmiri-strict-provenance`.
Nightly canary (reported, non-blocking): the PR set plus
`-Zmiri-tree-borrows`. If Tree Borrows flags a pattern we believe
is sound, file an issue with the minimized case; don't silence it
by dropping the flag.

Extra flags useful for debug sessions but NOT CI-enabled:

- `-Zmiri-symbolic-alignment-check` — stricter alignment auditing
  at pointer deref. Off in CI because it currently flags some
  stdlib patterns; enable when isolating an alignment bug.
- `-Zmiri-check-number-validity` — flags reads of uninitialized
  bytes that happen to coincide with a valid number. Off in CI
  because many crates pass it through unrelated transmutes; enable
  when isolating an uninit read.
- `-Zmiri-many-seeds` — re-runs the test under many different
  seeds of the interpreter's scheduler, broadening interleaving
  coverage. Nightly-only when Phase 1+ ships real atomics with
  preemption-sensitive invariants (follow-up item).

## Running locally

```bash
# The workflow pins this date; bump it deliberately.
MIRI_NIGHTLY=nightly-2026-04-01   # NOT auto-exported into your shell

rustup install "$MIRI_NIGHTLY"
rustup component add miri rust-src --toolchain "$MIRI_NIGHTLY"
cargo "+$MIRI_NIGHTLY" miri setup

# PR-baseline run:
MIRIFLAGS="-Zmiri-strict-provenance" \
    cargo "+$MIRI_NIGHTLY" miri test -p mango-loom-demo --lib --tests

# Nightly-canary run (Tree Borrows):
MIRIFLAGS="-Zmiri-strict-provenance -Zmiri-tree-borrows" \
    cargo "+$MIRI_NIGHTLY" miri test -p mango-loom-demo --lib --tests
```

`MIRI_NIGHTLY` is a mango-local env-var convention; it is not
exported into your shell by the workflow, and Miri itself does not
read it. Hardcode the date or source it from the workflow file.

`--lib --tests` explicitly excludes doctests. Doctests under Miri
are fragile (IO, environment reliance). A `miri-doc` job is a
future item for when a crate ships a non-trivial doctest on
unsafe.

## Nightly pin procedure

The `MIRI_NIGHTLY` env var at the top of
`.github/workflows/miri.yml` is the single source of truth for the
toolchain. Bump quarterly, or sooner when a tracked Miri issue
unblocks something we care about. Procedure:

1. Pick a candidate date (most recent green nightly on the Miri
   [rustc-dev-guide badge](https://rust-lang.github.io/rustup-components-history/)).
2. `rustup install nightly-YYYY-MM-DD`,
   `rustup component add miri rust-src --toolchain <...>`,
   `cargo +<...> miri setup`.
3. Run the full curated subset under both PR + nightly MIRIFLAGS
   variants (commands above). Both must be clean.
4. Scan the Miri [CHANGELOG](https://github.com/rust-lang/miri/blob/master/CHANGELOG.md)
   for flag renames, removals, or default flips between the old
   and new date. If a default flipped, update the flag table above
   in the same PR.
5. Update `MIRI_NIGHTLY` in `.github/workflows/miri.yml`.
6. Update the example date in this doc.
7. Open a PR tagged `miri-pin-bump`. rust-expert review.

## Debugging a Miri failure

Miri's UB reports are aborts, not Rust panics. Treat the first
diagnostic as the symptom, the `stack-like trace` it prints as
the frame chain, and use these flags to zero in:

- `-Zmiri-backtrace=full` — include line info for every frame, not
  just the innermost.
- `-Zmiri-seed=<N>` — reproduce a specific scheduler choice; paired
  with `-Zmiri-many-seeds` CI runs.
- `-Zmiri-tag-gc=0` — disable retag GC so the traces you print
  match the tags Miri shows in the diagnostic.

Minimization strategy: strip out code until the diagnostic
disappears; the last thing removed is the causal site. If you end
up at a stdlib call, the likely culprit is an aliasing invariant
inside _your_ `unsafe` block that only manifests through the call
— look at the `&mut`/`&` pairings the call implies.

## What Miri does NOT catch

Miri is an interpreter, not a race detector for weak-memory
architectures, not a sanitizer for FFI, and not a full-system
emulator. Known limits:

- **Weak-memory races across threads.** Miri's concurrency model
  is sequentially consistent at the Rust-atomics level. For
  architecture-specific reorderings, that's loom's job.
- **Inline asm.** Blocks compile but aren't interpreted; Miri
  skips them.
- **FFI without shims.** `extern "C"` calls into dynamic libraries
  are not interpreted. Write a Miri shim (`-Zmiri-extern-so-file`)
  or mark the test `#[cfg_attr(miri, ignore)]` if the crate
  genuinely requires a live lib call.
- **Syscall-level races.** Miri emulates a narrow set of syscalls
  deterministically; timing-based races between syscalls are not
  reproducible.
- **Unsafe transmutes between layouts with niches.** Miri catches
  some, not all; layout differences inside enums with
  `NonNull`-like niches are a known edge.
- **Uninitialized padding through memcpy.** Historically shifted;
  enable `-Zmiri-check-number-validity` if you suspect a
  padding-read issue.
- **const-eval UB.** The interpreter is the same but the context
  differs; a const-eval diagnostic may fire where the runtime Miri
  diagnostic does not, and vice versa.
- **Custom targets.** Miri emulates a narrow set of target tuples;
  `aarch64-apple-darwin` is supported, `wasm32-unknown-unknown`
  is not (as of the pinned date).

## Sanity-break recipe

A Miri gate that's never seen a failure is indistinguishable from
a Miri gate that's broken. If you've never seen Miri fail on this
project, apply this temporarily to confirm the gate would catch
UB:

```rust
// In crates/mango-loom-demo/src/lib.rs, inside the tests mod.
// DO NOT COMMIT. git stash after confirming.
#[test]
fn sanity_break_miri_would_catch_this() {
    let lock = Spinlock::new(0_u32);
    let mut g = lock.lock();
    let v: &mut u32 = unsafe {
        // Launder the pointer through a usize to break provenance.
        let p = g.with_mut(|p| p as *mut u32 as usize);
        &mut *(p as *mut u32)
    };
    *v = 1;
}
```

Under `MIRIFLAGS="-Zmiri-strict-provenance"` this fails with a
provenance error. The recipe is NOT committed to the repo; the
gate remains manually verifiable but not self-testing. Miri's UB
reports are interpreter aborts, not Rust panics, so a
`should_panic` wrapper is fragile and brittle.

## Interaction with other CI workflows

The Miri workflow is independent of `ci.yml` / `loom.yml` /
`audit.yml`:

- **Runner**: ubuntu-24.04 (same as others). OS-level isolation
  via separate job.
- **Cache**: separate `prefix-key: v0-miri`. The Miri sysroot
  (~300MB) lives in `~/.cache/miri` and the toolchain path; both
  are explicitly in `cache-directories`. Does not share state
  with the default-toolchain jobs.
- **Budget**: 30 min for the PR job, 60 min for the full job.
  Today's smoke takes < 3 minutes; if any single test exceeds
  ~5 minutes we split into `miri-quick` / `miri-slow` jobs.

## Non-negotiables

- A crate introducing `unsafe` must also add itself to
  `[workspace.metadata.mango.miri]` in the **same PR**.
- New `unsafe` blocks must carry a `// SAFETY:` comment per the
  contributor guide.
- If Miri flags your code, **do not** add `#[cfg_attr(miri, ignore)]`
  as a first move. Fix the bug. The skip attribute is reserved for
  the narrow FFI/inline-asm cases listed above.
- Tree-borrows canary failures are signals, not merge blocks, but
  they **must be investigated** and filed with the minimized case
  before a bump PR lands.

## See also

- [`docs/loom.md`](loom.md) — concurrency model checking;
  complementary to Miri.
- [`docs/testing.md`](testing.md) — test policy and runner.
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) §7 — `unsafe` policy;
  §8 — local commands.
- [`ROADMAP.md`](../ROADMAP.md) item 0.5 — where this policy was
  declared.
- [Miri README](https://github.com/rust-lang/miri) — upstream
  docs, flag reference.
- [Rust UCG](https://github.com/rust-lang/unsafe-code-guidelines) —
  the Rust Unsafe Code Guidelines working group; where the
  aliasing models (Stacked Borrows, Tree Borrows) are specified.
