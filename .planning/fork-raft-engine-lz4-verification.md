# Fork tracking: `humancto/raft-engine` (lz4-sys feature gate)

This file records the state of mango's temporary fork of
[`tikv/raft-engine`](https://github.com/tikv/raft-engine) and the plan
to retire the fork once upstream adopts the change.

## Why the fork exists

`tikv/raft-engine` at master SHA `f0fcebe922c384fcb673576f1f5b638203550ee7`
had `lz4-sys = "=1.9.5"` as an unconditional `[dependencies]` entry.
That C FFI crate is incompatible with mango's pure-Rust north-star
(ROADMAP.md "What Rust gives us that Go etcd cannot" — no C in the
default build graph). Upstream had no Cargo feature to opt out.

See ADR 0002 §W5 (`.planning/adr/0002-storage-engine.md`) for the
full rationale.

## Current state

| Field                       | Value                                                                  |
| --------------------------- | ---------------------------------------------------------------------- |
| Fork repo                   | [`humancto/raft-engine`](https://github.com/humancto/raft-engine)      |
| Fork branch                 | `feat/feature-gate-lz4-sys`                                            |
| Fork SHA (pinned by mango)  | `e1d738d9ad1c1fc4f5b21c8c73bf605b5696f535`                             |
| Upstream base SHA           | `f0fcebe922c384fcb673576f1f5b638203550ee7`                             |
| Upstream PR                 | [`tikv/raft-engine#397`](https://github.com/tikv/raft-engine/pull/397) |
| Upstream PR state           | Open (filed 2026-04-24)                                                |
| Review status on fork patch | `rust-expert` APPROVE_WITH_NITS (two rounds)                           |

## What the fork changes

Two commits on top of upstream master:

1. `4e8a088 feat(features): make lz4-sys optional behind lz4-compression feature`
   - `Cargo.toml`: `lz4-sys` → `optional = true`; new feature
     `lz4-compression = ["dep:lz4-sys"]`; `default = [..., "lz4-compression"]`
     preserves BC.
   - `src/util.rs`: `pub mod lz4` split into two `#[cfg]`-gated variants
     with identical public surface.
2. `e1d738d fix(lz4-feature): address rust-expert REVISE findings`
   - `Config::sanitize` rejects `batch_compression_threshold > 0` when
     feature is off.
   - Stub `decompress_block` returns `Error::Other` (not `Corruption`).
   - `DEFAULT_LZ4_COMPRESSION_LEVEL` hoisted to a parent-scope `pub const`.
   - Rustdoc note on `batch_compression_threshold`.
   - `CHANGELOG.md` + `README.md` entries.
   - New `cargo build --no-default-features --verbose` step in CI.
   - New unit test `test_sanitize_rejects_nonzero_threshold_without_lz4_feature`.

Total footprint: 6 files changed, +93 / -9.

## How mango consumes the fork

In `Cargo.toml` at the workspace root:

```toml
[workspace.dependencies]
raft-engine = { version = "0.4.2", git = "https://github.com/humancto/raft-engine", rev = "e1d738d9ad1c1fc4f5b21c8c73bf605b5696f535", default-features = false, features = [
    "internals",
] }
```

- `default-features = false` strips ALL three upstream default
  features: `lz4-compression`, `scripting`, AND `internals`.
- `features = ["internals"]` re-enables ONLY `internals` — the Raft
  log reader/writer types that `mango-raft` (Phase 1 impl PRs) needs.
- `lz4-compression` is deliberately excluded: that's the entire reason
  the fork exists (it pulls `lz4-sys`, a C FFI dep). Mango configs must
  set `batch-compression-threshold = 0` (enforced by the fork's
  `Config::sanitize`); compression happens above raft-engine in
  `mango-raft` via `lz4_flex`.
- `scripting` is deliberately excluded: it pulls `rhai` →
  `smartstring`, which is MPL-2.0 — an allow-list widening mango does
  not want to take for a feature mango does not use. `scripting` is
  raft-engine's TiKV-admin-CLI path; mango has no consumer of that
  surface. (Diverges from this doc's earlier versions, which listed
  `["internals", "scripting"]`; dropped during PR #49 implementation
  after cargo-deny flagged MPL-2.0 transitively. Going forward, fork
  rebases MUST NOT quietly re-enable `scripting` — if upstream moves
  anything mango needs from `internals` into `scripting`, update this
  file and the license allow-list in the SAME PR.)
- `version = "0.4.2"` on the git dep is load-bearing for cargo-deny
  (`wildcards = "deny"` rejects a git dep without `version =`). It
  also matches the fork's `package.version`, so the single
  `[[exemptions.raft-engine]] version = "0.4.2"` cargo-vet entry
  continues to cover both the active fork and the post-retirement
  upstream (see §"Supply-chain audit posture" below).

Verified absence of lz4-sys:

```
$ cargo tree --edges no-dev | grep lz4
(empty)
```

## Retirement plan

Fork retires when upstream PR #397 merges.

Retirement steps (in order):

1. Watch `tikv/raft-engine#397`. When it merges, note the upstream
   merge SHA.
2. Run `cargo update -p raft-engine` in a mango branch, repoint the
   workspace dep to `tikv/raft-engine` at the merged SHA:
   ```toml
   raft-engine = { version = "<merged-version>", git = "https://github.com/tikv/raft-engine", rev = "<merged-sha>", default-features = false, features = [
       "internals",
   ] }
   ```
   (Keep `features = ["internals"]` only — do NOT re-add `scripting`;
   see §"How mango consumes the fork" for the license rationale.)
3. Run the full mango test suite (`cargo nextest run --workspace`).
4. If tests pass, open a mango PR with the repointing, get
   `rust-expert` APPROVE, merge.
5. Update this file: mark `Upstream PR state` as merged, blank out
   "Fork SHA", move everything below the dividing line to a
   "Historical" section.
6. Archive `humancto/raft-engine` repo (GitHub → Settings → Danger
   Zone → Archive) so it cannot drift.

## What if upstream rejects the approach or stalls

The retirement plan above assumes PR #397 merges. Two other
endings need named handling:

### (a) #397 closed without merge, upstream proposes a different shape

If TiKV maintainers reject the Cargo-feature approach in favor of
a different upstream shape (e.g. a runtime `Config::enable_lz4`
knob, or a separate `raft-engine-core` crate split):

1. Engage upstream on the accepted shape; open a fresh fork branch
   built from that direction.
2. Update this file: replace `Fork branch`, `Fork SHA`, and
   `Upstream PR` rows to point at the new tracking PR. Mark the
   old #397 state as "Closed, superseded by #NNN" in a "Historical"
   section at the bottom.
3. Retirement now keys on the replacement PR merging, following
   the same six steps as the primary retirement plan.

### (b) #397 stalls for 12+ months with no maintainer engagement

Matches ADR 0002 §W4's "pre-1.0 / stale on crates.io" escape
hatch. If the fork's only deviation from upstream is still the
feature gate and no mango code depends on any internals that
upstream hasn't addressed:

1. Escalate to §W4 escape hatch (b): vendor the pinned fork SHA
   under `crates/vendored-raft-engine/` with a CODEOWNERS stanza
   forcing maintainer review.
2. Mango's workspace dep flips from `git = ...` to `path = "crates/vendored-raft-engine"`.
3. Archive `humancto/raft-engine` — no further rebases.
4. Annotate this file: "Status: vendored. See `crates/vendored-raft-engine/`."

This is the "upstream is functionally dead for our use case" exit.
Do not do this before the 12-month mark without explicit
discussion — vendoring a 20 kLOC storage engine is not a decision
to take lightly.

## Rebase policy while the fork lives

If mango needs a newer upstream SHA before #397 merges:

1. `cd ~/Desktop/claude-projects/raft-engine`
2. `git fetch upstream && git rebase upstream/master feat/feature-gate-lz4-sys`
3. Resolve conflicts in `src/util.rs` / `src/config.rs` if upstream
   touched them. If the upstream churn is material, re-run
   `rust-expert` on the rebased diff before pushing.
4. `git push --force-with-lease origin feat/feature-gate-lz4-sys`
5. Update the fork SHA in this file and in mango's workspace
   `Cargo.toml`. Both must move together.

## Supply-chain audit posture

The fork is public, pinned by SHA (not tag, not branch), and adds
zero `unsafe` tokens on top of upstream.

**Exemption keying (load-bearing, verified against cargo-vet 0.10
behavior).** `cargo vet` keys exemptions on the fully-resolved
source, which for a git dep is
`<package.version>@git:<resolved-SHA>` — not on `package.version`
alone. This is true EVEN when `[policy.raft-engine]` sets
`audit-as-crates-io = true` (which governs how audits are
interpreted, not how exemptions are matched). As a result, the
mango exemption in `supply-chain/config.toml` MUST carry the
SHA-qualified form:

```toml
[[exemptions.raft-engine]]
version = "0.4.2@git:e1d738d9ad1c1fc4f5b21c8c73bf605b5696f535"
criteria = "safe-to-deploy"
```

This exemption covers the active fork at this specific SHA only.
It does NOT automatically cover:

- A rebased fork SHA — new exemption line at the new
  `0.4.2@git:<new-SHA>` form required (the old line can be removed
  in the same PR).
- Post-retirement upstream crates.io 0.4.2 — new exemption line at
  plain `version = "0.4.2"` required (the SHA-qualified line can
  be removed in the retirement PR).

In short: every fork rebase and the fork retirement are
exemption-churning events. Those are already flagged as manual
actions in the "Rebase policy" and "Retirement plan" sections;
this section just spells out the specific config.toml edit.

Other caveats:

1. **Patch-version bumps on either side are not auto-covered.**
   If either the fork or upstream bumps `package.version`
   (raft-engine historically bumps on master without publishing
   to crates.io — see ADR 0002 §B3), the exemption stops matching
   and needs a new line. Watch for this on rebase.
2. **`review-by` is a mango convention, not a vet behavior.**
   Mango annotates every exemption with a `review-by: YYYY-MM-DD`
   note. Those dates do not auto-regenerate on source change.
   When rebasing the fork OR retiring to upstream, manually
   refresh the `review-by` date on the `raft-engine` exemption
   to reflect that the code under review changed.

## Last updated

2026-04-24 — PR #49 wires the dep into the workspace and
`crates/mango-storage` skeleton. Feature set tightened to
`["internals"]` only (dropped `scripting` after cargo-deny flagged
MPL-2.0 via `rhai`/`smartstring`; see §"How mango consumes the fork"
for the full rationale). Fork created, upstream PR #397 open.
