# Testing policy

Mango's CI test runner is [`cargo-nextest`](https://nexte.st/). The
per-test-class timeout budgets, the watchdog enforcement, and the
process-per-test model all flow from that choice.

This doc is the policy. The config is
[`.config/nextest.toml`](../.config/nextest.toml); the regression
test is [`scripts/test-watchdog.sh`](../scripts/test-watchdog.sh).

## Why `cargo-nextest`

- **Per-test hard timeouts.** `cargo test` has no built-in watchdog;
  a wedged test runs until the GitHub Actions job-level 15-minute
  timeout expires, burning the whole budget on one test. nextest's
  `slow-timeout = { period = "…", terminate-after = 1 }` sends
  SIGTERM after the period elapses, records the test as failed, and
  lets the rest of the suite continue.
- **Process-per-test isolation.** nextest spawns each test in its
  own process, so a `static` mutex, a global singleton, or a
  `std::env::set_var` from one test cannot leak into another.
  `cargo test` runs tests as threads in one process — subtle
  cross-test coupling tends to hide there.
- **Filter expressions for routing.** Timeouts are per-class, and
  classes are defined by filter expressions (`kind(test)`,
  `test(~loom)`, etc.), which is both automatic and reviewable.

## Test classes and budgets

| Class | Selector | Budget | Used for |
|-------|----------|--------|----------|
| `unit` | default (unclassified) | 30s | `#[cfg(test)] mod tests` in `src/`, pure function tests, codec round-trips |
| `integration` | `kind(test)` | 5 min | `tests/*.rs` at each crate root; multi-module exercises |
| `loom` | `test(~loom)` | 30 min | Concurrency model checking (Phase 5+) |
| `chaos` | `test(~chaos)` | 24 h | Fault injection / crash-only recovery (Phase 14.5+) |

The filter expressions live in
[`.config/nextest.toml`](../.config/nextest.toml) under
`[[profile.default.overrides]]`. Policy lives in `[profile.default]`
so `cargo nextest run` locally enforces the same budgets as CI.
`[profile.ci]` inherits from `default`; only CI-specific tuning
goes there.

### Class-naming convention

- Put loom tests in files matching `tests/loom_*.rs`, or in a
  module/crate whose name contains `loom`.
- Put chaos tests in files matching `tests/chaos_*.rs`, or in a
  module/crate whose name contains `chaos`.

The filter is contains-match (`~`), not prefix — a file
`tests/my_loom_scenario.rs` still matches `test(~loom)`. The
convention is strict about presence of the keyword, lenient about
position.

If a test belongs to no class, it falls to the 30s `unit` budget.
Default-to-strict is intentional; if your test legitimately needs
more, move it to the matching class or open a PR that widens the
budget with justification.

## Flakiness policy

`retries = 0` in `[profile.default]`. A test that fails the first
time fails the build — we do not silently re-run and relabel as
`FLAKY`. This aligns with the `CONTRIBUTING.md §4` test bar:
"trust CI is not a test plan." A test that is legitimately
infra-sensitive (e.g., a network-bound integration test) should
carry a narrow per-test override with a `retries = N` line and a
comment naming the specific transience being tolerated, rather
than flipping the default.

## Output format (`failure-output = "immediate-final"`)

Failures print inline as they occur AND in a summary at the end of
the run. The default `"final"` hides first failures until the
whole suite completes, which is bad for long-running loom and
chaos budgets — reviewers want to see the first failure fast.

## Watchdog enforcement — how it works

`slow-timeout` alone is just a warning. `terminate-after = N`
multiplies the period by `N` and sends SIGTERM at the resulting
moment. Example from our config:

```toml
slow-timeout = { period = "30s", terminate-after = 1 }
```

→ nextest warns at 30s, SIGTERMs at 60s (period * (1+1)), and
records the test as `TIMEOUT` (exit code non-zero).

That last sentence is the load-bearing invariant. If anyone
removes `terminate-after`, the watchdog downgrades to warn-only.
[`scripts/test-watchdog.sh`](../scripts/test-watchdog.sh) is the
committed regression test: it runs a `#[ignore]`d test that sleeps
90s under the 30s unit budget and asserts nextest actually kills
it. The script fails loudly if the `TIMEOUT` marker disappears
from output or if the test runs to natural completion.

## Process-per-test footguns

Before you add a test that depends on cross-test state, know:

- **Global loggers** (`env_logger::init`, `tracing_subscriber::set_global_default`) — each test process calls `init` fresh. Fine in isolation; footgun only if you rely on state from a previous test. Prefer `try_init` to avoid panics from double-init inside a single process (belt-and-suspenders).
- **`static` / `OnceLock` singletons** — each test process has its own copy. A test that assumes "the registry has 3 entries because the previous test added them" will fail under nextest. Fix: set up state per-test.
- **`std::env::set_var`** — scoped to the process. Fine under nextest; nasty under `cargo test` (tests could race each other on env). Another reason to prefer nextest locally.
- **tokio runtimes** — each test builds its own. No issue, but the cost-per-test is non-trivial; batch assertions into fewer tests where sensible.

## Running tests locally

Install nextest once:

```bash
cargo install cargo-nextest --locked
# or, on macOS:
brew install cargo-nextest
```

Run the full suite (what CI runs):

```bash
cargo nextest run --workspace --all-targets --locked --profile ci
cargo test --doc --workspace --locked
```

Run a single test:

```bash
cargo nextest run -E 'test(my_test_name)'
```

Run under the default profile (friendlier output, same budgets):

```bash
cargo nextest run
```

Inspect the resolved config:

```bash
cargo nextest show-config test-groups --profile ci
cargo nextest list --profile ci        # shows which tests land where
```

## Doctests

nextest does **not** run doctests. CI runs `cargo test --doc
--workspace --locked` as a separate step, placed after nextest so
the dep cache is warm. Contributors must run both to reproduce
CI; `CONTRIBUTING.md §8` lists both commands.

## Adding a new test class

1. Pick a keyword that is unique and memorable (e.g., `fuzz`).
2. Choose the naming convention (`tests/fuzz_*.rs` is the blessed
   shape — consistent with loom and chaos).
3. Add a block to `.config/nextest.toml`:
   ```toml
   [[profile.default.overrides]]
   filter = 'test(~fuzz)'
   slow-timeout = { period = "1h", terminate-after = 1 }
   ```
4. Document the class in the table above.
5. Update `scripts/test-watchdog.sh` only if the new class needs
   its own regression — the existing smoke covers the unit class
   and the enforcement mechanism (`terminate-after`), which is the
   load-bearing property.

## Escape hatch for intentionally long tests

If a specific test must exceed the budget of its class — e.g., a
chaos test that intentionally runs 48h — add a per-test override:

```toml
[[profile.default.overrides]]
filter = 'test(=my_crate::the_really_long_test)'
slow-timeout = { period = "72h", terminate-after = 1 }
```

Comment above the block with the justification. A PR that adds a
new per-test override is a review signal; reviewers should push
back unless the justification is concrete.

## Forward references

- **madsim** (ROADMAP.md:788): the `sim` CI profile
  (`RUSTFLAGS="--cfg madsim"`) runs under nextest — same runner,
  additional env var. We do not fork the test runner for
  simulation; nextest is the one-and-only test orchestrator.
- **Miri** (ROADMAP.md:795): the Miri CI job runs `cargo miri test`,
  not nextest. Miri is an interpreter; it cannot do
  process-per-test because there is no process separation at the
  interpreter level. The nightly Miri workflow is its own job
  with its own (long) timeout, not subject to this doc.
- **10×-baseline regression detection** (ROADMAP.md:794): deferred
  as a follow-up. nextest's flake-detector is a retry mechanism,
  not a baseline comparator. A wrapper script over
  `--message-format libtest-json` is needed; not productive until
  the test suite has ~50+ tests.

## See also

- [`CONTRIBUTING.md §4`](../CONTRIBUTING.md) — the test bar.
- [`CONTRIBUTING.md §8`](../CONTRIBUTING.md) — commands CI runs.
- [`ROADMAP.md` item 0.5.1](../ROADMAP.md) — where this policy was declared.
- [nextest docs — filtersets](https://nexte.st/docs/filtersets/reference/)
- [nextest docs — slow tests and timeouts](https://nexte.st/docs/configuration/slow-tests/)
