# Dependency updates

Mango uses [Dependabot] to keep GitHub Action SHAs and Cargo workspace
crate versions current. Every proposed bump ships as a PR and goes
through the normal expert-review + CI-gate loop — **no auto-merge
anywhere**. The bot is a signal, not an authority.

Config: [`.github/dependabot.yml`](../.github/dependabot.yml)
Self-test: [`scripts/dependabot-scripts-test.sh`](../scripts/dependabot-scripts-test.sh)
Schema: [`.github/schemas/dependabot-2.0.json`](../.github/schemas/dependabot-2.0.json)

[Dependabot]: https://docs.github.com/en/code-security/dependabot

## Why Dependabot, not Renovate

Dependabot is GitHub-native, understands our SHA-pin policy for
`uses:` lines, and reads `[workspace.dependencies]` (including the
`=` exact pins on `loom`, `madsim`, and `madsim-tokio`). Renovate is
more powerful but requires installing the Mend GitHub App — a
supply-chain surface we would need to audit to re-gain capabilities
we do not currently use.

## What Dependabot watches

- **GitHub Actions** — every `uses:` line in `.github/workflows/*.yml`.
  Dependabot bumps the 40-hex SHA and, for actions that publish
  semver tags, the trailing `# <ref>` comment in lockstep.
- **Cargo** — `[workspace.dependencies]` in `Cargo.toml` + `Cargo.lock`.
  Exact pins (`=0.7.2`) are bumped as exact pins (the `=` is kept).

## Schedule

Both ecosystems run **Monday 14:00 UTC** (10:00 ET / 07:00 PT).
Grouped PRs — minor/patch and major split per ecosystem, plus a
named `madsim-family` group for `madsim` + `madsim-tokio` (see
"Grouping and atomicity" below). Queue bounded by
`open-pull-requests-limit: 5` per ecosystem.

## Reviewer checklist for a Dependabot PR

For every Dependabot PR, verify on the green-CI path:

1. **MSRV gate green** — `ci.yml` + `madsim.yml` both run under
   workspace MSRV (1.89; see [ADR 0003](../.planning/adr/0003-msrv-bump.md)).
   A proposed bump that requires rustc > 1.89 red-flags these jobs.
   See "MSRV-incompatible bumps" below.
2. **cargo-deny green** — license / bans / sources / advisories.
3. **cargo-vet green** — supply-chain audit. Transitive graph shifts
   are the common failure mode; see "Transitive graph shifts" below.
4. **cargo-audit green** — RUSTSEC advisories. If the bump closes
   an advisory cited by `deny.toml`'s ignore list, prune the stale
   entry in the same PR.
5. **cargo-public-api / cargo-semver-checks green** — once Phase 6
   ships these are gating; today they are advisory.
6. **SHA-pin regression test (actions PRs)** — the self-test
   `scripts/dependabot-scripts-test.sh` asserts every workflow
   `uses:` line still carries a 40-hex SHA. `ci.yml` runs it on
   any PR that touches dependabot-adjacent files.
7. **Trailing comment** — see "Trailing comments" below.
8. **madsim-family bumps** — verify both `madsim` and `madsim-tokio`
   (manifest key `tokio`) are moved together in the same PR. The
   `madsim-family` group enforces this structurally; if you see a
   split PR, something has regressed.
9. **Stale ignore reasons in `deny.toml`** — if this PR bumps a
   crate cited by a `deny.toml` ignore entry (today: `time`,
   `bincode`), prune or rewrite the entry's reason text to reflect
   the new version.

## Grouping and atomicity

Named groups are evaluated in declaration order. `madsim-family` is
load-bearing:

```yaml
madsim-family:
  patterns:
    - "madsim"
    - "madsim-*"
    - "tokio" # madsim-tokio under the workspace package-rename
  update-types: [patch, minor, major]
```

`tokio` here is the _manifest key_, not the upstream crate name —
the workspace renames the `madsim-tokio` crate to `tokio` at
[`Cargo.toml:69`](../Cargo.toml):

```toml
tokio = { version = "=0.2.30", package = "madsim-tokio" }
```

Dependabot groups by manifest key, so listing `tokio` captures
`madsim-tokio`. If a future refactor removes this rename, the
`tokio` entry in `madsim-family` becomes dead and should either be
removed or repointed.

Why atomic: the Cargo.toml header comment for these lines says
_"move both lines together."_ `madsim` and `madsim-tokio` are
released in lockstep by upstream; splitting the bump across PRs
leaves one of the two PRs with a broken `cargo check` and red CI.
The group makes this impossible by construction — both land in the
same PR or neither does.

## Trailing comments (SHA-pin policy)

Every `uses:` line in `.github/workflows/*.yml` MUST be a 40-hex SHA.
The trailing `# <ref>` comment is a human-readable hint; its exact
form depends on whether the upstream action publishes semver tags
or tracks branches:

| Action                            | Example trailing comment |
| --------------------------------- | ------------------------ |
| `actions/checkout`                | `# v4`                   |
| `actions/cache`                   | `# v4`                   |
| `actions/upload-artifact`         | `# v4`                   |
| `Swatinem/rust-cache`             | `# v2`                   |
| `taiki-e/install-action`          | `# v2`                   |
| `EmbarkStudios/cargo-deny-action` | `# v2.0.17`              |
| `rustsec/audit-check`             | `# v2.0.0`               |
| `dtolnay/rust-toolchain`          | `# stable`               |

For **semver-tagged** actions, Dependabot bumps the SHA and updates
the comment to match the new tag. For **branch-tracking** actions
(`dtolnay/rust-toolchain`), Dependabot updates the SHA but leaves
the comment alone — that is correct; the ref is still "stable."

The self-test's regex is tolerant: `uses: <owner>/<repo>@<40-hex>`
with an **optional** trailing `# <anything>`. A missing comment is
acceptable (not preferred). A non-SHA ref (`@v4` short form) is a
regression that fails the check.

## MSRV-incompatible bumps

Policy: **do not pre-seed the `ignore:` list with MSRV guards.**
Let CI catch them.

When Dependabot proposes a bump that requires rustc >= MSRV+1:

1. CI red on ci.yml / madsim.yml MSRV job.
2. Reviewer decides: hold or bump MSRV?
   - **Hold** → close the PR and add an `ignore:` entry with a
     removal trigger:
     ```yaml
     ignore:
       - dependency-name: "some-crate"
         versions: [">=X.Y"]
         # Remove when workspace MSRV reaches 1.ZZ. Cross-ref:
         # deny.toml and this file's header.
     ```
     Every ignore entry MUST carry a removal trigger in a YAML
     comment — same discipline as deny.toml ignores and vet
     exemptions. Stale ignores accrete; audits miss them.
   - **Bump MSRV** → open a separate PR following the process in
     [`docs/msrv.md`](msrv.md) §"Bumping the MSRV" (write an ADR,
     update all four machine-checked sources of truth together,
     sweep docs, rust-expert review, merge). Ship the MSRV bump
     first, then return to the Dependabot PR and retry.

## Transitive graph shifts

When Dependabot bumps a top-level crate, the resolved transitive
graph can shift. cargo-vet expects an exemption for every unvetted
dep; new transitive versions cause the `vet.yml` job to fail.

Recipe on the Dependabot branch:

```bash
# On the Dependabot branch, after `cargo fetch --locked`:
cargo vet regenerate exemptions

# Annotate every new exemption with a review-by date. The
# xtask-vet-ttl gate requires a `notes = "review-by: YYYY-MM-DD ..."`
# line on every exemption entry — missing notes fail CI.
# (A helper script exists at scripts/vet-annotate-exemptions.sh if
# the drift is large; for single-crate bumps a manual edit is
# faster.)

git add supply-chain/config.toml
git commit -m "chore(vet): regenerate exemptions for <crate> bump"
git push
```

## Stale `deny.toml` ignore reasons

`deny.toml` carries RUSTSEC ignore entries whose reason text cites
specific crate versions (e.g. `time 0.3.41`, `bincode 1.3.3`). When
Dependabot bumps those crates, the reason text becomes stale but
the advisory `id` is still the canonical reference — cargo-deny
does not complain.

**Reviewer responsibility**: on any PR that bumps a crate cited in
a `deny.toml` ignore reason, prune or rewrite the entry in the
same PR. A mechanical lint (`scripts/deny-ignore-lint.sh`) is
tracked as a follow-up but not yet in place.

## Rebase strategy

Dependabot uses `rebase-strategy: auto`. When a new Dependabot PR
lands and a prior open one auto-rebases, **human commits on that
branch may be rewritten**. Keep human follow-ups (vet
regenerations, deny.toml edits) as amendments to Dependabot's
commit, or as last-mile commits immediately before merge. Do not
leave long-lived hand-written commits on a Dependabot branch.

## Pre-merge checklist

Before approving:

- [ ] CI green on all required workflows
- [ ] For actions PRs: trailing `# <ref>` comment sane
- [ ] For cargo PRs: any `deny.toml` / `supply-chain/config.toml`
      fallout handled in the same commit
- [ ] For madsim-family PRs: both crates in one commit
- [ ] No auto-merge enabled

## Follow-ups (tracked separately)

- `scripts/deny-ignore-lint.sh` — mechanical cross-check of ignore
  entry versions against Cargo.lock. Today it's reviewer eyeball.
- Consider a second cargo entry with
  `versioning-strategy: lockfile-only` for transitive security
  patches — decide after observing Dependabot's first month of
  traffic.
- Split cargo grouping into `cargo-dev` vs `cargo-prod` when Phase 1
  production crates land (the whole workspace is scaffolding today).
