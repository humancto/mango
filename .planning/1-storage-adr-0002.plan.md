# Phase 1 storage engine — ADR 0002

## Goal

Lock in mango's storage-engine architecture before any Phase 1 code starts. Produce a trustworthy, verified, adversarially-reviewed Architectural Decision Record at `.planning/adr/0002-storage-engine.md`, apply the roadmap edits that follow from it, and keep all claims cited to primary sources in `.planning/adr/0002-storage-engine.verification.md`.

This PR ships the ADR, the `Backend` + `RaftLogStore` trait contract, and the ROADMAP amendments. It does **not** ship any crate code. Phase 1 implementation begins in a separate PR after this merges.

## Decision (from rust-expert post-verification review)

**Candidate A — REAFFIRMED with mitigations.**

- KV backend: `redb` 4.1.0+ (pure-Rust, COW B-tree, mmap-free, post-1.0).
- Raft log: `tikv/raft-engine` git-pinned to a specific master SHA (crates.io 0.4.2 is 24 months stale; master is active through 2026-03-10).
- Both hidden behind a `Backend` + `RaftLogStore` trait pair in `crates/mango-storage`.
- raft-engine's `lz4-sys` C FFI disabled; mango does compression above the engine with `lz4_flex` (consistent with `ROADMAP.md:485`).

Every wart (redb no-marquee-users, 37 unsafe tokens; raft-engine pre-1.0, crates.io staleness, 49 unsafe tokens) has an explicit mitigation and an escape-hatch trigger. The `Backend` trait is designed so swapping to heed/LMDB (runner-up) is mechanical if Phase 1 differential testing finds a correctness divergence.

## Files to touch

1. `.planning/adr/0002-storage-engine.md` — new. Full ADR per rust-expert §7 outline.
2. `ROADMAP.md:462` — update `redb` inventory row: remove the unverified "Tested under Miri by upstream" claim; cite the 37-unsafe-token number from verification; reference ADR 0002 for the full rationale.
3. `ROADMAP.md:485` — amend `lz4_flex` row: note raft-engine's built-in compression is disabled in mango; compression (if any) is done above raft-engine.
4. `ROADMAP.md:813` — rewrite the engine-choice item: point to ADR 0002 as the decision record.
5. `ROADMAP.md:813` — add new items right after:
   - Define `Backend` and `RaftLogStore` trait pair per ADR 0002 §6.
   - Differential-test harness vs bbolt (blocker for Phase 1 close).
   - 7-day sustained chaos gate (blocker for Phase 1 close).
   - Engine-swap dry-run test (proves trait boundary is swappable).
6. `ROADMAP.md:823` — amend block-level compression item: clarify `lz4_flex` is the pure-Rust default and raft-engine's built-in compression is disabled.
7. `ROADMAP.md:883` — Phase 5 WAL item: note the WAL is implemented via the `RaftLogStore` trait backed by git-pinned `tikv/raft-engine`.
8. `ROADMAP.md:880` — cross-reference note: ADR 0005 assumes ADR 0002's `RaftLogStore` trait as the log-storage boundary.
9. `ROADMAP.md:0.5` — new item: track upstream raft-engine 1.0 discussion.

## Approach

One PR, atomic commits:

1. **Commit 1** — ADR document (`.planning/adr/0002-storage-engine.md`).
2. **Commit 2** — roadmap edits (inventory row corrections, Phase 1 expansion, Phase 5 cross-ref, upstream-tracking task).

Not yet:

- LICENSE (Apache 2.0) ships in a separate PR after this merges — I want it as its own PR for audit clarity.
- `crates/mango-storage` skeleton — that's the first Phase 1 implementation PR, not this one.

## Verification discipline

Every numerical or categorical claim in the ADR must appear in either:

- `.planning/adr/0002-storage-engine.verification.md` (primary-source verified as of 2026-04-24), OR
- `ROADMAP.md` (the authoritative north-star).

No invented recovery-time numbers, no guessed production-user lists, no "redb benches at X ops/sec" fabrications. If the verification doc doesn't back a claim, the ADR either drops the claim or flags it as TBD for Phase 1 measurement.

## Testing

This PR is docs-only. No code runs. The acceptance check is:

- `cargo test --workspace` remains green (no code changes, so trivially).
- `cargo clippy --workspace --all-targets -- -D warnings` remains green.
- All CI gates pass.
- rust-expert approves the ADR diff.

## Rollback plan

If rust-expert rejects Candidate A on the PR-diff review, revert commits and revise. If they reject the ADR's structure but affirm Candidate A, restructure the ADR and re-push. No impact on code; this is a pure docs change.

## Risks

- **Risk 1 — Lock-in under uncertainty.** The verification left 5 open questions (redb marquee users, raft-engine 1.0 plans, cargo-geiger formal number, bbolt max-value-size, etcd fsync batching thresholds). The ADR treats these as tracked open questions, not blockers. Mitigation: the escape-hatch criteria (§5 of the ADR) make the decision reversible by engine-swap. No open question invalidates the current evidence base.
- **Risk 2 — raft-engine git-pin maintenance burden.** Pinning to a master SHA means we lose cargo-audit's version-based advisory matching. Mitigation: cargo-vet entry at the pinned SHA (Phase 0 gate already shipped) + Renovate tracking of the master branch (Phase 0.5 gate already shipped) + explicit ADR commitment to re-check at every mango minor release.
- **Risk 3 — roadmap churn from the new items.** Adding the differential-test harness + 7-day chaos gate + engine-swap dry-run extends Phase 1's scope. This is intentional — the "redb has no marquee production user" wart requires us to own more of the validation burden. Phase 1 is already pre-Phase-2 with no external commitments, so the extra scope is absorbable.
