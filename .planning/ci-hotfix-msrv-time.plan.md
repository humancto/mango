# Hotfix: restore green CI on main

## Problem

`main` CI is red with two distinct blockers since the 0.7-vet and
0.5-semver merges. Every subsequent PR inherits the failures, so no
roadmap work can land until this clears.

### Blocker 1 — MSRV regression (E0658)

`crates/xtask-vet-ttl/src/lib.rs:225`:

```rust
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests intentionally use these patterns for concise assertions"
)]
```

The `reason = "..."` clause on `#[allow]` is **unstable on 1.80**
(stabilized in 1.81). Workspace MSRV is pinned to 1.80 via
`rust-version = "1.80"` and the `msrv (cargo check @ 1.80)` CI job,
so the job fails with `E0658: lint reasons are experimental`.

This was introduced in PR #32 (item 0.7-vet) — slipped past because
the ADR-required `cargo +1.80 check` wasn't run locally; nightly +
stable builds pass.

### Blocker 2 — RUSTSEC-2026-0009 (time 0.3.41)

`time 0.3.41` has a DoS via stack exhaustion when parsing untrusted
RFC 2822 input. Patched in **0.3.47**. `cargo-deny` flags this as
`error[vulnerability]` and fails the `deny` job.

**MSRV conflict — cannot bump:**

| version | MSRV   |
| ------- | ------ |
| 0.3.41  | 1.67.1 |
| 0.3.42  | 1.81.0 |
| 0.3.45  | 1.83.0 |
| 0.3.46  | 1.88.0 |
| 0.3.47  | 1.88.0 |

Every version above 0.3.41 requires rustc ≥ 1.81; the patched 0.3.47
requires 1.88. Workspace MSRV is **1.80**, enforced by the `msrv
(cargo check @ 1.80)` job. Bumping `time` is blocked on a workspace
MSRV policy change — out of scope for a hotfix.

**Usage audit — attack surface does not apply:**

`xtask-vet-ttl` (the only consumer) uses `time` exclusively for:

- `time::macros::format_description!` — compile-time format string
  construction. No runtime input.
- `Date::parse(..., format_description!("[year]-[month]-[day]"))` —
  parses ISO-8601 dates from `supply-chain/config.toml`
  (repo-controlled).
- `OffsetDateTime::now_utc()` — no parsing.

No RFC 2822 parsing. No untrusted input — the tool runs locally and
in CI on repo-controlled `supply-chain/config.toml`. The advisory's
attack precondition ("user-provided input provided to any type that
parses with the RFC 2822 format") is not reachable.

**Plan:** add a documented `[advisories] ignore` entry to `deny.toml`.
`cargo-deny` 0.19.4's schema accepts only `id` and `reason` (no
structured `expiration` field — verified empirically by the tool
rejecting the key with `error[unexpected-keys]`). The re-audit
trigger (next workspace MSRV bump, target 2026-10-23) and the
full rationale live in the `reason` string, and a policy comment
above the `ignore` table mandates that every entry carry a
re-audit trigger. When MSRV eventually moves past 1.88, the ignore
becomes removable and the real fix can land.

## Scope

One PR, two atomic commits, targeted at restoring green CI. No
roadmap item is created for this; both are bug fixes against
previously-shipped items.

## Files

1. `crates/xtask-vet-ttl/src/lib.rs` — drop the `reason = "..."`
   clause; preserve the intent via an explanatory comment above
   the attribute.
2. `deny.toml` — populate the `[advisories] ignore` table with a
   table-form entry for `RUSTSEC-2026-0009` carrying a reason and
   expiration date.

## Test strategy

- **Local `cargo +1.80 check --workspace --all-targets --locked`**:
  must pass. This is the missing step from PR #32 that let the
  regression land; adding it to CONTRIBUTING is a follow-up (not
  in this hotfix — out of scope).
- **Local `cargo test -p xtask-vet-ttl`** on stable: confirms the
  `#[allow]`-attribute edit didn't break the test module's
  build/behavior.
- **Local `cargo deny check advisories`**: must show zero
  vulnerability errors after the ignore is added. The `deny` CI
  job runs the same invocation.
- **CI end-to-end**: the hotfix PR must pass `msrv`, `deny`,
  `ci`, `audit`, `semver-checks`, `vet`, `ct-comparison`, `miri`,
  `geiger`.

## Commit topology

Two commits — each fixes exactly one failing CI job, so a revert
on either leaves the other fix intact:

1. `fix(msrv): drop 1.81+ lint reason clause in xtask-vet-ttl tests`
   - Edits `crates/xtask-vet-ttl/src/lib.rs`.
   - Adds a comment referencing MSRV 1.80 and the 1.81 stabilization.
2. `fix(deny): ignore RUSTSEC-2026-0009 with documented re-audit trigger`
   - Edits `deny.toml` `[advisories] ignore` table.
   - Commit message documents the MSRV conflict, the usage-audit
     rationale (no RFC 2822 parsing, CI-only input), and the
     re-audit trigger embedded in `reason` (no structured
     `expiration` key in cargo-deny 0.19.x).

## Risks

- **Other `reason = "..."` sites exist in the workspace.**
  Mitigation: `grep -rn 'reason =' --include '*.rs'` before
  committing to confirm the `lib.rs:225` site is the only one.
  Already run: only the new explanatory comment matches.
- **`deny.toml` ignore entry might be incorrectly rejected by
  `cargo-deny` 0.19.x schema.** Mitigation: run
  `cargo deny check advisories` locally before push; the CI
  `deny` job runs the same command.
- **The expiration date is forgotten.** Mitigation: date format
  and reason link to this plan so a future reviewer has enough
  context. A Renovate/Dependabot follow-up in a later roadmap
  item will surface the expiration via PR.
- **`cargo-vet` gate re-runs on the `deny.toml` change and flags
  unrelated deltas.** Mitigation: the change is only `deny.toml`,
  which is outside the vet-audited surface (vet audits crate
  sources, not project configs).

## Rollback plan

Either commit can be reverted individually without touching the
other. If the ignore entry produces unexpected behavior under
`cargo-deny`, revert commit 2 and temporarily accept red `deny`
CI while a replacement is authored — but note that leaves main
red again, so the revert path is only viable if a better fix
lands within hours.

## Definition of done

- PR green on all CI jobs.
- rust-expert review verdict `APPROVE`.
- Merge + push.
- Main CI green on the post-merge `chore:` commit (no roadmap flip
  needed — this isn't a roadmap item).
