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
raft-engine = { git = "https://github.com/humancto/raft-engine", rev = "e1d738d9ad1c1fc4f5b21c8c73bf605b5696f535", default-features = false, features = ["internals", "scripting"] }
```

- `default-features = false` strips `lz4-compression` from the default list.
- `features = ["internals", "scripting"]` keeps the two other default
  features `raft-engine` ships with `default = ["internals", "scripting", "lz4-compression"]`.
- `lz4-compression` is deliberately excluded. Mango configs must set
  `batch-compression-threshold = 0` (enforced by the fork's
  `Config::sanitize`); compression happens above raft-engine in
  `mango-raft` via `lz4_flex`.

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
   raft-engine = { git = "https://github.com/tikv/raft-engine", rev = "<merged-sha>", default-features = false, features = ["internals", "scripting"] }
   ```
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
zero `unsafe` tokens on top of upstream. `cargo vet` keys
exemptions on `(crate_name, crate_version)` — both the fork and
upstream ship `package.version = "0.4.2"`, so the same
`[[exemptions.raft-engine]] version = "0.4.2"` entry in
`supply-chain/config.toml` covers both while the fork is active
and after retirement.

Caveats that change this:

1. **Patch-version bumps are not auto-covered.** If either the
   fork or upstream bumps `package.version` (raft-engine
   historically bumps on master without publishing to crates.io
   — see ADR 0002 §B3), the exemption stops matching and needs
   a new `version = "0.4.3"` line. Watch for this on rebase.
2. **SHA swaps are silent to vet.** `cargo vet` verifies the
   resolved git hash against the locked source, but the hash is
   not part of exemption identity. Changing the fork SHA without
   bumping `package.version` requires no vet edits.
3. **`review-by` is a mango convention, not a vet behavior.**
   Mango annotates every exemption with a `review-by: YYYY-MM-DD`
   note. Those dates do not auto-regenerate on source change.
   When rebasing the fork OR retiring to upstream, manually
   refresh the `review-by` date on the `raft-engine` exemption
   to reflect that the code under review changed.

## Last updated

2026-04-24 (fork created, PR #397 opened, mango still on skeleton
phase — dep not yet wired into a workspace `Cargo.toml`; that lands
with PR-1).
