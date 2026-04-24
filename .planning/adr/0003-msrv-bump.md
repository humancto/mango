# ADR 0003: Workspace MSRV bump 1.80 → 1.89

## Status

**Accepted** — 2026-04-24.

Supersedes: none.
Superseded by: none.

Deciders: Archith (maintainer), `rust-expert` (adversarial plan review, 2026-04-24, APPROVE verdict on plan v3).

**Gate:** every factual claim below is grounded in a file in-repo or a public `Cargo.toml` on crates.io / tikv/raft-engine master at a specific SHA. Version floors for redb and raft-engine are to be re-verified against the pinned SHA during PR-1 execution and pasted into that PR body.

## Context

### ADR 0002's engine picks force an MSRV question

[ADR 0002](0002-storage-engine.md) committed the Phase 1 storage stack to:

1. **KV backend:** `redb` 4.1.0+ (crates.io).
2. **Raft log engine:** `tikv/raft-engine`, git-pinned master.

Both declare `edition = "2024"` and carry `rust-version` floors **above** the workspace's current MSRV of 1.80:

- redb 4.x publishes with `rust-version` in the 1.85–1.89 range; at 4.1.0 the published Cargo.toml declares 1.85+ (precise value to be SHA-captured in PR-1).
- raft-engine master declares `rust-version = "1.85"` (precise value to be SHA-captured in PR-1).

The workspace floor at 1.80 cannot parse or compile either dep. This was not reconciled in ADR 0002 and is the first load-bearing blocker for Phase 1 implementation work.

### The CI layers that enforce the MSRV

Three machine-checked sources of truth, plus a docs surface. All must move together or drift is caught by `scripts/test-msrv-pin.sh`:

1. `Cargo.toml` → `[workspace.package] rust-version`.
2. `clippy.toml` → `msrv`.
3. `.github/workflows/ci.yml` → `msrv` job `dtolnay/rust-toolchain` input (and `prefix-key` cache key).
4. `.github/workflows/madsim.yml` → `env.MSRV` (a separate MSRV gate for the `--cfg madsim` build).

### Adjacent follow-through

- **RUSTSEC-2026-0009** (`time` 0.3.41 DoS via RFC 2822 parsing). The ignore in `deny.toml:63` is justified as "fix is unreachable without a policy bump" because `time` ≥ 0.3.42 requires rustc ≥ 1.81. At MSRV 1.89 the fix IS reachable; this ADR resolves the advisory as a consequence.
- **`--target x86_64-unknown-linux-gnu` MSRV-job workaround** (per Issue #23) exists because cargo 1.80 cannot parse `wit-bindgen 0.57.1`'s `edition = "2024"` manifest. cargo ≥ 1.85 parses `edition2024` natively. At MSRV 1.89 the workaround's underlying rationale is gone.
- **Non-exhaustive tripwire** at `scripts/non-exhaustive-check.sh:321` is "inert at MSRV ≤ 1.80" by design. At MSRV 1.81+ it enforces that publishable crates use the inline `#[allow(clippy::exhaustive_enums, reason = "...")]` form instead of the `// reason:` line-comment workaround. `grep -rn 'reason:' crates/` today returns zero hits — the migration is a no-op. The tripwire becomes a forward-drift enforcement rail.

## Decision

Bump workspace MSRV from **1.80 → 1.89**.

Single-PR scope (PR-0 in the four-PR Phase 1 sequence defined in `.planning/phase-1-storage-skeleton.plan.md`):

1. Update the three machine-checked sources of truth to `1.89`.
2. Update every doc surface that names the MSRV number.
3. Resolve RUSTSEC-2026-0009 by upgrading `time` to ≥ 0.3.47 and removing the ignore.
4. Drop the `--target x86_64-unknown-linux-gnu` workaround (MSRV ≥ 1.85 parses `edition2024`). Update `docs/msrv.md` and the CI comments.
5. Regenerate `Cargo.lock` with `cargo update`; justify any non-trivial cascades in the PR body.
6. Amend ADR 0002 §W with a "Resolved in ADR 0003" cross-reference at §W1 (redb 4.1.0 MSRV) and §W4 (raft-engine master-rev MSRV).

## Considered alternatives

### Alternative B: Stay at 1.80; pin pre-edition-2024 redb / raft-engine

Pin redb to a 3.x release and raft-engine to a pre-edition-2024 master SHA so both compile on 1.80.

Rejected:

- redb 3.x is a different on-disk format than 4.x (MVCC + table-layout changes landed in 4.0). Committing to 3.x means inheriting accumulated bug history we do not need to own on day 1 of Phase 1.
- Pre-edition-2024 raft-engine master requires SHA archaeology to find a commit with working behavior and an acceptable MSRV. It is less fresh, not more stable.
- ADR 0002's escape-hatch logic (§5) assumes "current supported versions"; pinning to legacy versions invalidates the escape plan.
- Kicks the can: the moment either dep's next pin requires a higher MSRV, we're back here.

### Alternative C: Bump to the minimum-viable floor (1.85)

1.85 is the first version with stable `edition2024` and the raft-engine master floor, but NOT redb 4.x's floor.

Rejected:

- Would admit raft-engine master but not redb 4.x.
- We'd still need to pin redb 3.x → inherits Alternative B's cost.
- Only one `rust-version` bump per period is worth the doc churn; pick the floor both deps satisfy.

### Alternative D: Hand-roll the storage engine

Dismissed in ADR 0002 §alternative-F. Team shape (solo + AI) cannot afford a bespoke engine.

## Consequences

Positive:

- Unblocks Phase 1 implementation (PR-1 / PR-2 / PR-3 of the skeleton sequence).
- Retires the `--target` workaround and closes Issue #23 at its structural root.
- Closes RUSTSEC-2026-0009 in the same PR that unblocks the fix.
- Activates the non-exhaustive tripwire as a live enforcement rail for future drift.
- Inline `#[allow(lint, reason = "...")]` becomes available — preferred over the `// reason:` line-comment workaround per rustc's own recommendation and `docs/api-stability.md`'s migration plan.

Negative:

- Raises the contributor-onboarding floor. Someone running stable 1.84 cannot compile mango. Mitigation: the stable channel is 1.91+ at the time of this ADR; the affected population is small.
- Invalidates stale reviewer muscle memory around the `// reason:` comment convention. Mitigation: `docs/api-stability.md` is updated in this PR; the tripwire enforces the new form.
- MSRV bumps are a public contract; frequent movement is a project-quality signal. Mitigation: N-6 policy is stated explicitly in "Forward-compat" below.

Neutral:

- `.cargo/config.toml`'s `[resolver] incompatible-rust-versions = "fallback"` setting stays. Comments are updated to reflect the new floor; the mechanism is still useful as a forward-compat guard against MSRV-gated deps being installed on older toolchains.

## Forward-compat

MSRV policy: **latest stable minus 6 months, rounded to a whole minor version, bumped deliberately, not incidentally**. Rust's stable cadence is ~6 weeks per minor → ~4 minors per 6 months. At the time of this ADR, stable is ~1.91; 1.89 is ~N-2. The policy gives headroom before the next forced bump.

Revisit cadence:

- Every phase boundary (if a phase's deps force higher, bump at phase start, not mid-phase).
- Every engine-dep bump (redb / raft-engine pin advancement) if it requires higher.
- If no forcing event, re-audit annually to avoid falling too far behind ecosystem floors.

## Cross-references

- [ADR 0002 — Storage engine](0002-storage-engine.md) §W1, §W4 (amended in this PR to cross-reference this ADR).
- [`docs/msrv.md`](../../docs/msrv.md) — operational doc for the new MSRV (rewritten in this PR).
- [`docs/dependency-updates.md`](../../docs/dependency-updates.md) §MSRV-incompatible bumps — the procedure this ADR follows.
- [`docs/api-stability.md`](../../docs/api-stability.md) §"How to add a per-enum exception" — title updated to drop the "at MSRV 1.80" qualifier.
- [`deny.toml`](../../deny.toml) — RUSTSEC-2026-0009 ignore removed in this PR.
- [`.cargo/config.toml`](../../.cargo/config.toml) — `incompatible-rust-versions = "fallback"` comment rewritten in this PR.
- [`scripts/non-exhaustive-check.sh:321`](../../scripts/non-exhaustive-check.sh) — MSRV tripwire activates at 1.81+.
- [`.planning/phase-1-storage-skeleton.plan.md`](../phase-1-storage-skeleton.plan.md) — the plan this ADR is a prerequisite for.
- Issue [#23](https://github.com/humancto/mango/issues/23) — the `wit-bindgen 0.57.1` / `edition2024` problem this MSRV bump resolves structurally.
