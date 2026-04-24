# `madsim` — deterministic-simulation runtime

Mango uses [`madsim`](https://github.com/madsim-rs/madsim) as its
deterministic-simulator runtime for async tests. Every Phase 3+
async primitive (`mango-raft`, `mango-mvcc`, `mango-server`,
`mango-client`) MUST be `madsim`-compatible by the time its phase
ships. This document pins the mechanism, the version, the CI
invocation, and the contract.

## Why madsim (and not a custom harness)

- **Deterministic task scheduling, timers, channels, network, and
  RNG** — a seeded madsim run schedules tasks, delivers messages,
  fires timers, and generates random numbers in the exact same
  order on every machine. A failing test replays.
- **Runtime-layer simulation, not test-layer** — the swap happens
  at `tokio::time::sleep` / `tokio::spawn` / `tonic::Channel`, not
  at a mocked trait boundary. Application code is unchanged.
- **Upstream-maintained ecosystem of renamed crates**
  (`madsim-tokio`, `madsim-tonic`, etc.) used by RisingWave and
  others. No bespoke runtime to maintain.
- **Why Phase 0.5 instead of Phase 5/13**: retrofitting the
  package rename across an established codebase is more expensive
  than adopting it from the first tokio-using crate. Adopting in
  Phase 0.5 turns the Phase 5 "deterministic simulation testing
  harness" item from "build it" into "write tests against it."

## The rename

Mango activates madsim via Cargo's **package-rename** mechanism,
not `[patch.crates-io]`:

```toml
# workspace Cargo.toml
[workspace.dependencies]
tokio = { version = "=0.2.30", package = "madsim-tokio" }
madsim = { version = "=0.2.30", features = ["macros"] }
```

```toml
# crates/mango-<something>/Cargo.toml
[dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "sync", "time"] }

[dev-dependencies]
madsim = { workspace = true, features = ["macros"] }
```

Source code then writes `use tokio::time::sleep;` / `use
tokio::sync::mpsc;` **unchanged** — the rename does the swap at
link time.

### Why the workspace-level rename is accepted by Cargo

`package = "..."` inside `[workspace.dependencies]` is accepted
by Cargo and the rename is inherited through `dep.workspace =
true` on each member crate. This is the pattern RisingWave uses
in its workspace `Cargo.toml` today. Cargo issue
[#12546](https://github.com/rust-lang/cargo/issues/12546)
discusses the inverse case (member-side `package` with
`workspace = true`, which emits an unused-manifest-key warning);
mango does not hit that case.

If upstream Cargo ever tightens this behavior, the assertion in
`scripts/madsim-scripts-test.sh` (which probes `cargo metadata`
for the demo crate's `tokio` dep resolving to the `madsim-tokio`
package) will go red the same day.

## The cfg and the flag — `--cfg madsim` vs `#[cfg(madsim)]`

- `RUSTFLAGS="--cfg madsim"` — the rustc flag that activates the
  simulator inside `madsim-tokio`. Set at the CI matrix level,
  never in `Cargo.toml`. Not a Cargo feature (so it doesn't leak
  through the dep graph to downstream builds).
- `#[cfg(madsim)]` — the Rust-source attribute that gates
  **sim-only scaffolding** (fault injectors, `madsim::rand` calls,
  simulated-network setup).
- `#[cfg(not(madsim))]` — gates code that only makes sense under
  real tokio (TLS handshake tests, OS-level I/O, signal handlers).
- Library code should have **zero cfg gates** — the rename handles
  the swap transparently. Every `#[cfg(madsim)]` in a `src/` file
  is a code smell to review.

## What does not work under sim

- **TLS**: there is no published `madsim-tokio-rustls` shim.
  Real `tokio-rustls` works under `cfg(madsim)` but the handshake
  timing is non-deterministic; most sim tests skip TLS or
  terminate it in front of the simulator (e.g., a real load
  balancer in integration tests, a no-TLS test profile in sim).
- **RNG**: `rand::thread_rng()` is not intercepted. Use
  `madsim::rand` from inside `#[cfg(madsim)]` code, or carry a
  `rand::rngs::StdRng` seeded from `madsim::runtime::Handle` in
  sim tests.
- **File I/O**: `madsim::fs` is partial. For storage-engine sim
  tests, mock the storage trait rather than relying on simulated
  filesystem semantics.
- **OS signals**: no-op under sim.

## CI invocation

```bash
RUSTFLAGS="--cfg madsim" \
MADSIM_TEST_SEED=1 \
MADSIM_TEST_NUM=100 \
  cargo +stable nextest run \
      --profile ci \
      --target-dir target/madsim \
      -p mango-madsim-demo
```

- **`--cfg madsim`** — activates the simulator.
- **`MADSIM_TEST_SEED=1`** — fixed seed for CI reproducibility.
  Local debugging of a flake uses the same var.
- **`MADSIM_TEST_NUM=100`** — each test runs 100 times with
  different seeds internally. Upstream-recommended CI value.
- **`--target-dir target/madsim`** — dedicated target dir; the
  `cfg(madsim)` compilation produces a different artifact graph
  from the default build, so sharing `target/` thrashes the
  incremental cache.
- **`--profile ci`** — from `.config/nextest.toml` (per-class
  timeouts, JUnit output).
- **`-p <crate>`** — the curated subset is read from
  `[workspace.metadata.mango.madsim].crates` via
  `scripts/madsim-crates.sh`.

## Running locally

```bash
# real tokio (default build — no flag)
cargo nextest run -p mango-madsim-demo

# deterministic simulator
RUSTFLAGS="--cfg madsim" MADSIM_TEST_SEED=1 MADSIM_TEST_NUM=100 \
    cargo nextest run --target-dir target/madsim -p mango-madsim-demo

# MSRV check under --cfg madsim (matches CI)
RUSTFLAGS="--cfg madsim" \
    cargo +1.89 check --tests -p mango-madsim-demo \
    --target-dir target/madsim
```

## Writing a sim test

Every async crate gets two test files in `tests/`:

- `tests/madsim_<scenario>.rs` — file-scoped `#![cfg(madsim)]`,
  uses `#[madsim::test]`.
- `tests/tokio_<scenario>.rs` — file-scoped `#![cfg(not(madsim))]`,
  uses `#[tokio::test]`.

Library code (`src/`) is unchanged across profiles. Never mix
profiles in one test file.

The canonical template lives at
[`crates/mango-madsim-demo/`](../crates/mango-madsim-demo/) —
copy it into the new crate's `tests/` and rename.

## When to add a crate to the curated subset

The same PR that starts using `tokio.workspace = true` on a new
crate MUST:

1. Add the crate name to `[workspace.metadata.mango.madsim].crates`
   in the workspace `Cargo.toml`.
2. Ship at least one `#[madsim::test]` test in `tests/`.
3. Ship the paired real-tokio test so the default-build CI job
   exercises the same code path.

Reviewers enforce this. `scripts/madsim-scripts-test.sh` enforces
the metadata-table consistency; the `madsim.yml` CI job enforces
that the sim tests pass.

## Bumping the pin

madsim's env-var semantics (`MADSIM_TEST_SEED`, `MADSIM_TEST_NUM`,
`MADSIM_ALLOW_SYSTEM_THREAD`) and simulated-runtime behavior can
drift between minor versions. The pin is exact (`=0.2.30`) for
the same reason `loom = "=0.7.2"`.

Bump procedure:

1. Verify the new version exists for **both** `madsim` and
   `madsim-tokio` at crates.io. If Phase 6 has added
   `madsim-tonic`, verify it too. Versions must be mutually
   compatible (check `madsim-tokio`'s `madsim` dep req).
2. Bump all three `=x.y.z` strings in workspace `Cargo.toml` in
   a single PR.
3. Run the full CI matrix on the bump PR:
   - `cargo nextest run -p mango-madsim-demo` (real tokio)
   - `RUSTFLAGS="--cfg madsim" cargo nextest run --target-dir
target/madsim -p mango-madsim-demo` (simulator)
   - The MSRV check under `--cfg madsim`.
4. If any breakage, pin back down and file an upstream issue
   before retrying.

Patch bumps are usually safe and can be proposed by Dependabot.
Minor bumps require a human smoke-test via the demo crate's
sim + tokio tests. Major bumps (e.g., `0.3.x`) are a design
discussion — open an ADR.

## Scope — the Phase-6 gate

Mirrors [ROADMAP.md:792]:

> `mango-raft`, `mango-mvcc`, `mango-server`, `mango-client`
> MUST be `madsim`-compatible by the time their respective
> phases ship. The CI matrix runs every async test under both
> the default profile (real `tokio`) and the `sim` profile
> (`RUSTFLAGS="--cfg madsim"`); regressions in either profile
> fail the PR.

The rust-expert is instructed to refuse any Phase 6 PR that
lands before every async primitive has paired sim + tokio test
coverage.
