# MSRV policy

Mango's Minimum Supported Rust Version (MSRV) is **1.80**.

The MSRV is declared in three places that must stay in sync. Drift
is caught by `scripts/test-msrv-pin.sh`, which runs in the
[`msrv` CI job](../.github/workflows/ci.yml):

1. `Cargo.toml` → `[workspace.package] rust-version = "1.80"`
2. `clippy.toml` → `msrv = "1.80"` (pairs with
   `clippy::incompatible_msrv = "deny"` in the workspace lint
   table, flagging any stdlib API newer than MSRV at PR time)
3. `.github/workflows/ci.yml` → `msrv` job's
   `dtolnay/rust-toolchain` action input `toolchain: "1.80"`

Mango's compile floor on the CI target (`x86_64-unknown-linux-gnu`)
is **1.80** and stays there until a deliberate bump (see "Bumping
the MSRV" below).

## Scope of the guarantee

**The MSRV guarantee is platform-scoped to
`x86_64-unknown-linux-gnu`.** When the CI matrix expands (e.g.,
adding `aarch64-unknown-linux-gnu` or `aarch64-apple-darwin`), the
MSRV job's `--target` list expands with it. Other targets —
including developer laptops on arm64 — are not guaranteed to pass
`cargo check` on the MSRV toolchain. In practice today the
difference is negligible because no mango crate is
platform-conditional, but the guarantee is stated plainly so it
isn't assumed to be wider than it is.

## The `--target x86_64-unknown-linux-gnu` flag in the CI job

The `msrv` job runs
`cargo fetch --locked --target x86_64-unknown-linux-gnu` and
`cargo check --workspace --all-targets --locked --target
x86_64-unknown-linux-gnu`, not the bare commands.

Why: `Cargo.lock` contains wasi-only transitives (currently
`wasip2 1.0.3 → wit-bindgen 0.57.1`, pulled through
`tonic-build → tempfile → getrandom 0.3`). `wit-bindgen 0.57.1`
declares `edition = "2024"`, which cargo 1.80 cannot parse — the
`edition2024` feature did not stabilize until cargo 1.85. Without
`--target`, `cargo fetch --locked` would try to parse every
manifest referenced by the lockfile regardless of whether that
crate gets compiled on any target mango CI runs, and fail
immediately on the `wit-bindgen 0.57.1` manifest parse. With
`--target`, cargo never enters the wasi-only subgraph and the job
passes. Tracking: [Issue #23](https://github.com/humancto/mango/issues/23).

The compile floor on Linux is still 1.80 — the `wit-bindgen`
crate is never compiled on any target mango supports. The
`--target` flag is a manifest-parsing workaround, not a looser
MSRV guarantee.

## Validating MSRV locally

```bash
rustup toolchain install 1.80
rustup run 1.80 cargo check --workspace --all-targets --locked \
  --target x86_64-unknown-linux-gnu
```

Both steps must exit 0. This is the exact command the CI `msrv`
job runs. Contributors are not required to install the 1.80
toolchain for routine stable-toolchain work — the `msrv` job
catches regressions pre-merge — but running the command locally
before a dep-bump PR is cheap insurance.

### Side effect of dep updates

`cargo update` may pull additional edition2024 transitives through
non-wasi paths at any time. If that happens, the MSRV job goes
red on the PR that bumps the lockfile. The on-call author of the
update decides between:

- **Pin / downgrade** the offending dep (if an older version is
  acceptable and doesn't cascade widely).
- **Bump MSRV deliberately** to the first stable Rust release
  that parses the new edition.

## Bumping the MSRV

MSRV bumps are **deliberate**, not incidental to a dep update.
Per roadmap item 0.9: "start at 1.80, bump deliberately." The
process:

1. Open an issue naming the dep that forced the bump and the
   Rust release that parses / compiles it.
2. Open a PR that updates all three sources of truth above
   (`Cargo.toml` rust-version, `clippy.toml` msrv, ci.yml
   `toolchain`) in one commit, plus this doc.
3. Update `Cargo.lock` with `cargo update` and commit the
   resulting churn in the same PR. Justify any large cascades
   in the PR body.
4. Verify locally with the "Validating MSRV locally" command
   above, substituting the new toolchain version.
5. **When bumping to 1.81 or later**, migrate every `// reason:`
   line-comment preceding `#[allow(clippy::exhaustive_enums)]` to
   the inline `#[allow(clippy::exhaustive_enums, reason = "...")]`
   form. The line-comment convention is a MSRV-1.80 workaround
   (the attribute's `reason = ...` field stabilized in 1.81);
   dropping it keeps the rationale attached to the attribute
   rather than on a preceding line where refactors can separate
   them. See [`docs/api-stability.md`](api-stability.md)
   §"How to add a per-enum exception at MSRV 1.80" and update
   `scripts/non-exhaustive-check.sh`'s awk state machine in the
   same PR.
6. rust-expert adversarial review.
7. Merge.

### Ecosystem floors to be aware of

Current dep graph on the Linux target:

- `wit-bindgen 0.49` (last edition2021 version before 0.57) requires Rust **1.82**.
- `wasip2 1.0.3` declares `rust-version = "1.87"` (only enforced on
  wasi targets — not a Linux constraint today).
- `getrandom 0.3` requires Rust **1.63**.
- `tonic-build 0.12` requires Rust **1.71**.

**A deliberate bump to 1.82 would NOT let us drop `--target`** —
the underlying problem (wasi-only edition2024 transitives) is not
solved by bumping to 1.82 alone. Dropping `--target` requires
either (a) bumping MSRV to 1.85 (the first version that parses
the `edition2024` feature), or (b) upgrading past the dep chain
that pulls `wit-bindgen 0.57+`.

## See also

- [`CONTRIBUTING.md`](../CONTRIBUTING.md) §7 "Other policies" row
  for the short version
- `scripts/test-msrv-pin.sh` — drift-detection between the three
  sources of truth
- [`ROADMAP.md:757`](../ROADMAP.md) — item 0.9 (where the MSRV
  gate landed)
