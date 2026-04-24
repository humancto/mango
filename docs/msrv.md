# MSRV policy

Mango's Minimum Supported Rust Version (MSRV) is **1.89**.

The MSRV is declared in four places that must stay in sync. Drift
is caught by `scripts/test-msrv-pin.sh`, which runs in the
[`msrv` CI job](../.github/workflows/ci.yml):

1. `Cargo.toml` → `[workspace.package] rust-version = "1.89"`
2. `clippy.toml` → `msrv = "1.89"` (pairs with
   `clippy::incompatible_msrv = "deny"` in the workspace lint
   table, flagging any stdlib API newer than MSRV at PR time)
3. `.github/workflows/ci.yml` → `msrv` job's
   `dtolnay/rust-toolchain` action input `toolchain: "1.89"`
4. `.github/workflows/madsim.yml` → `env.MSRV: "1.89"` for the
   separate `--cfg madsim` MSRV gate (`cargo +1.89 check` under
   `RUSTFLAGS="--cfg madsim"`)

Mango's compile floor on the CI target (`x86_64-unknown-linux-gnu`)
is **1.89** and stays there until a deliberate bump (see "Bumping
the MSRV" below).

The current MSRV was set by [ADR 0003](../.planning/adr/0003-msrv-bump.md).

## Scope of the guarantee

**The MSRV guarantee is platform-scoped to
`x86_64-unknown-linux-gnu`.** When the CI matrix expands (e.g.,
adding `aarch64-unknown-linux-gnu` or `aarch64-apple-darwin`), the
MSRV job's target list expands with it. Other targets — including
developer laptops on arm64 — are not guaranteed to pass
`cargo check` on the MSRV toolchain. In practice today the
difference is negligible because no mango crate is
platform-conditional, but the guarantee is stated plainly so it
isn't assumed to be wider than it is.

## The historical `--target x86_64-unknown-linux-gnu` workaround

Earlier versions of this doc described a `--target
x86_64-unknown-linux-gnu` flag on the MSRV job's `cargo fetch` /
`cargo check` invocations, added under Issue #23 as a workaround
for cargo 1.80's inability to parse `wit-bindgen 0.57.1`'s
`edition = "2024"` manifest (the `edition2024` feature stabilized
in cargo 1.85). At MSRV 1.89 that workaround is **retired** — the
bare `cargo check` invocation succeeds. See
[ADR 0003](../.planning/adr/0003-msrv-bump.md) §Consequences.

## Validating MSRV locally

```bash
rustup toolchain install 1.89
rustup run 1.89 cargo check --workspace --all-targets --locked
```

Both steps must exit 0. This is the exact command the CI `msrv`
job runs. Contributors are not required to install the 1.89
toolchain for routine stable-toolchain work — the `msrv` job
catches regressions pre-merge — but running the command locally
before a dep-bump PR is cheap insurance.

### Side effect of dep updates

`cargo update` may pull transitives with a declared `rust-version`
newer than the workspace floor at any time. The
`incompatible-rust-versions = "fallback"` setting in
`.cargo/config.toml` prefers the newest compatible version when
resolving, but cannot invent a compatible version that doesn't
exist. If the workspace floor is below some dep's minimum, the
MSRV job goes red on the PR that bumps the lockfile. The on-call
author decides between:

- **Pin / downgrade** the offending dep (if an older version is
  acceptable and doesn't cascade widely).
- **Bump MSRV deliberately** to the first stable Rust release
  that satisfies the dep, following the process below.

## Bumping the MSRV

MSRV bumps are **deliberate**, not incidental to a dep update.
The policy is **latest stable minus 6 months**, rounded to a whole
minor, revisited at every phase boundary or engine-dep bump (see
[ADR 0003](../.planning/adr/0003-msrv-bump.md) §Forward-compat).

The process:

1. Write an ADR under `.planning/adr/NNNN-msrv-bump.md` naming
   the forcing dep, the target floor, the considered
   alternatives (stay; bump-to-minimum; hand-roll), and the
   consequences. [ADR 0003](../.planning/adr/0003-msrv-bump.md)
   is the template.
2. Open a PR that updates all four machine-checked sources of
   truth above (`Cargo.toml` rust-version, `clippy.toml` msrv,
   ci.yml `toolchain` input + cache prefix, madsim.yml
   `env.MSRV`) in one commit, plus this doc.
3. Update `Cargo.lock` with `cargo update` and commit the
   resulting churn in the same PR. Justify any large cascades
   in the PR body.
4. Verify locally with the "Validating MSRV locally" command
   above, substituting the new toolchain version.
5. Sweep every doc that names a literal MSRV number
   (`grep -rn '1\.XX' docs/ CONTRIBUTING.md README.md` for the
   outgoing version) and rewrite to the new floor.
6. rust-expert adversarial review (both on plan and on PR diff).
7. Merge once rust-expert gives `APPROVE`.

### Migration to the inline `#[allow(lint, reason = "...")]` form

The `reason = "..."` field on `#[allow]` attributes stabilized in
rustc 1.81. Prior to that, mango used a `// reason:` line-comment
immediately preceding `#[allow(clippy::exhaustive_enums)]` as a
workaround. At MSRV 1.89 the inline form is available across the
entire supported floor and is preferred per
[`docs/api-stability.md`](api-stability.md). The tripwire at
`scripts/non-exhaustive-check.sh:321` enforces the migration: any
surviving `// reason:` line-comment in a publishable crate's
`src/**/*.rs` fails the check.

## See also

- [ADR 0003 — MSRV bump 1.80 → 1.89](../.planning/adr/0003-msrv-bump.md)
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) §7 "Other policies" row
  for the short version
- `scripts/test-msrv-pin.sh` — drift-detection between the four
  machine-checked sources of truth
- [`docs/dependency-updates.md`](dependency-updates.md) §MSRV-
  incompatible bumps — the operational procedure
- [`ROADMAP.md`](../ROADMAP.md) — item 0.9 (where the MSRV
  gate landed)
