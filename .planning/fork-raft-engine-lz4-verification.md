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

The fork is public, pinned by SHA (not tag, not branch), and has zero
`unsafe` additions on top of upstream. `cargo vet` handles the fork
the same way it handles any git-pinned crate — the exemption is on
`raft-engine` regardless of origin. When retiring the fork, the
exemption continues to apply against the new upstream SHA without
edits (same crate name, same version `0.4.2`).

## Last updated

2026-04-24 (fork created, PR #397 opened, mango still on skeleton
phase — dep not yet wired into a workspace `Cargo.toml`; that lands
with PR-1).
