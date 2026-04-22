# Plan: `cargo-deny` config + CI job

Roadmap item: Phase 0 — "Add `deny.toml` and a `cargo-deny` CI job
(license + advisory + duplicate-version checks; ban `git`-deps without
explicit allowlist; ban `openssl-sys` per the Security axis — use
`rustls`)." (`ROADMAP.md:754`)

## Goal

Four overlapping guarantees enforced at PR time:

1. **Licenses**: only an allowlisted set of SPDX identifiers may
   appear in the transitive dep tree. Catches the "quietly pulled in
   GPL" failure mode.
2. **Advisories**: block any dep with a RustSec advisory. Paired with
   the future `cargo-audit` nightly job (`ROADMAP.md:755`) that
   **auto-files an issue** — this PR-time gate stops new advisories
   from landing; the nightly catches advisories published against
   already-merged code.
3. **`git`-deps ban**: prod dep tree must come from crates.io, unless
   an explicit allowlist entry says otherwise. `git`-deps are mutable
   refs that don't version-lock reliably and skip supply-chain vetting.
4. **Specific crate bans**: `openssl-sys` is banned — the Security
   axis commits us to `rustls` for TLS. Also ban duplicate-version
   pile-ups (two incompatible versions of the same crate in the
   graph) because they double attack surface and blow up binary size.

## North-star axis

**Security + Supply-chain.** Go etcd pulls in openssl transitively;
mango doesn't. The ban is the enforcement, not a preference. A
RustSec advisory or a GPL transitive dep MUST fail the PR, not
"someone will notice during release."

## Approach

Two file touches. One new config. One CI workflow addition.

### D1. `deny.toml` (NEW at workspace root)

Four sections mapping to the four guarantees. Based on the
`cargo-deny` 0.16 schema (the current stable major as of 2026-04).

```toml
# Mango supply-chain policy enforced by `cargo-deny`. Runs in CI on
# every push and PR. Changes here need reviewer sign-off.
#
# Docs: https://embarkstudios.github.io/cargo-deny/
# Version pin: cargo-deny 0.16+ schema.

[graph]
# Single-target today; expand when cross-compilation lands.
targets = [{ triple = "x86_64-unknown-linux-gnu" }]
all-features = false

[output]
feature-depth = 1

[licenses]
# SPDX identifiers we accept. Anything outside this set fails the PR.
# Rationale per entry:
#   Apache-2.0, MIT, BSD-3-Clause, BSD-2-Clause, ISC — standard
#     permissive licenses, compatible with our Apache-2.0 release.
#   Unicode-3.0, Unicode-DFS-2016 — `unicode-ident` and friends.
#   Zlib — `adler`, `miniz_oxide`.
#   CC0-1.0 — public-domain-equivalent.
#   OpenSSL — explicitly permitted only for a future mandated dep.
#     Today, none. Re-audit if any dep requires this.
allow = [
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "MIT",
    "BSD-3-Clause",
    "BSD-2-Clause",
    "ISC",
    "Unicode-3.0",
    "Unicode-DFS-2016",
    "Zlib",
    "CC0-1.0",
]
confidence-threshold = 0.93
# No `exceptions` today — any license not in `allow` fails the PR.

[advisories]
version = 2
# `deny` fails on any advisory. `unmaintained = "all"` also fails on
# unmaintained crates, which is the right posture — a maintainer
# going dark is a supply-chain risk even without a filed CVE.
yanked = "deny"
unmaintained = "all"
ignore = []

[bans]
multiple-versions = "deny"
# When a dupe is unavoidable (during a major-version migration),
# allowlist it here with an issue link and a removal target. Empty
# today.
skip = []
skip-tree = []
wildcards = "deny"
# Crate-level bans: openssl-sys is the Security-axis banned crate.
# `rustls` + `rustls-pki-types` is the one true TLS path.
deny = [
    { name = "openssl-sys", reason = "Mango's Security axis mandates rustls; no OpenSSL in the tree." },
    { name = "openssl", reason = "Same as openssl-sys — use rustls." },
    { name = "native-tls", reason = "Pulls OpenSSL on Linux by default; use rustls." },
]

[sources]
# Prod dep tree must come from crates.io. Explicit allowlists below
# for any git-dep or alternate registry. Empty today.
unknown-registry = "deny"
unknown-git = "deny"
allow-registry = ["https://github.com/rust-lang/crates.io-index"]
allow-git = []
```

### D2. `.github/workflows/ci.yml` — add `deny` job

New job parallel to the existing `fmt` / `clippy` / `test`. Uses the
`EmbarkStudios/cargo-deny-action` which is the maintained action that
tracks the binary's release cadence. SHA-pinned per the existing
convention in `ci.yml`.

```yaml
deny:
  name: deny
  runs-on: ubuntu-24.04
  timeout-minutes: 10
  steps:
    - uses: actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5 # v4
    - uses: EmbarkStudios/cargo-deny-action@<SHA> # v2.x
      with:
        command: check all
        arguments: --all-features
```

The action downloads a pinned `cargo-deny` binary, so we don't need a
Rust toolchain for this job.

**Which SHA**: look up the latest `v2` release SHA on
`EmbarkStudios/cargo-deny-action` at implementation time. This is a
non-trivial security decision — an unpinned third-party action is
exactly the supply-chain hole `deny.toml` is supposed to close.

## Files to touch

- `deny.toml` — NEW at workspace root (~60 lines with comments).
- `.github/workflows/ci.yml` — add `deny` job (~10 lines added).

No code changes.

## Edge cases

- **`cargo-deny` finds a pre-existing violation on the very first
  run** — unlikely today (the only workspace member is a placeholder
  crate with one dep, `serde_json`... actually zero non-stdlib deps).
  Verified by `cargo metadata --format-version 1` grep. If a future
  dep trips the allowlist, the PR that adds it handles the fix in the
  same commit (either widen the allowlist with justification, or
  swap the dep).
- **License-detection false positives** — `cargo-deny` uses SPDX
  identifier matching against `Cargo.toml` `license` fields with
  fallback to `LICENSE` file scanning. The `confidence-threshold =
0.93` is the recommended default; lower = more matches (unsafe),
  higher = more false-rejects. Per-crate `clarify` entries handle
  known edge cases; none needed today.
- **Target scope** — `[graph] targets` gates which platforms' dep
  trees are checked. Single Linux target today; we'll add macOS and
  Windows entries when those platforms are tier-2-supported (post
  Phase 6 or so). `cargo-deny` only checks deps reachable from a
  listed target, so missing a target means missing some deps.
- **`unmaintained = "all"`** — the most aggressive setting. A crate
  published once and never touched (common for tiny utilities) will
  fail the check. Realistic failure: `atty`, `once_cell` (superseded
  by stdlib). None in the tree today. When encountered, the PR fixes
  by either swapping the dep or adding a justified `ignore = [...]`
  entry with a dated removal target.
- **`multiple-versions = "deny"`** — the strictest. A common dep
  (serde, tokio, rand) pulls in two incompatible version spans
  occasionally. Realistic: we'll eventually need a `skip = []`
  allowlist. Today the graph is tiny, so start strict and relax on
  first violation with justification.
- **`wildcards = "deny"`** — bans `*`-version requirements in
  workspace `Cargo.toml`. None today. Easy to keep.
- **GitHub Action SHA pinning** — `cargo-deny-action` is owned by
  Embark Studios who maintain `cargo-deny` itself. Pin to a specific
  v2 release SHA; update deliberately via Dependabot when that lands
  (future item).
- **`native-tls` ban** — added proactively even though it's not
  mentioned in the roadmap text. It's the same class — `native-tls`
  on Linux pulls OpenSSL by default. If we ban `openssl-sys` without
  also banning `native-tls`, a dep could pull `native-tls` which
  pulls `openssl-sys` and the ban would fire with a confusing
  transitive error instead of a direct one. Banning both at the
  top surfaces the root cause.

## Test strategy

Same playbook as prior config PRs. CI gate IS the test.

1. **Existing checks stay green** — `fmt` / `clippy` / `test` all
   still pass.
2. **`cargo deny check all` passes locally** — run before pushing.
   `cargo install cargo-deny --locked` once, then `cargo deny check
all --all-features` from workspace root must exit 0.
3. **`deny` CI job succeeds on the PR** — this is the persistent
   regression gate.
4. **Violation-injection audit (one-off)** — same shape as prior PRs.
   Two micro-audits bundled:
   - Add `openssl-sys = "*"` as a scratch dep to `crates/mango/Cargo.toml`;
     `cargo deny check bans` must fail with the `openssl-sys` reason
     string surfaced in the error. Revert.
   - Add a fake git-dep `foo = { git = "https://example.com/foo.git" }`;
     `cargo deny check sources` must fail. Revert. (Omit if the fake
     dep can't resolve — `cargo-deny` reads `Cargo.lock`, so the dep
     must actually resolve to trigger. If it can't resolve, the
     ban-only audit is sufficient.)

The CI gate is the persistent mechanism. A richer integration test
is filed as a future hardening item — out of scope tonight.

## Rollback

Single squash commit. Revert → `deny.toml` disappears, CI job is
removed, no enforcement. Zero runtime impact.

## Out of scope (explicit, do not do in this PR)

- **`cargo-audit` nightly job** — separate roadmap item
  (`ROADMAP.md:755`).
- **`cargo-msrv`** — separate roadmap item (`ROADMAP.md:756`).
- **Dependabot config** — adjacent but orthogonal; file as a future
  item.
- **Per-crate `clarify` entries** — none needed today; add surgically
  when a false-positive fires.
- **Tier-2 target platforms in `[graph] targets`** — add when those
  platforms are formally supported.
- **ROADMAP checkbox flip** — separate commit to main per workflow.

## Risks

- **Third-party action SHA drift** — pinned SHA must be kept current.
  Dependabot is the future fix; until then, quarterly manual audit
  is acceptable for a CI-only action.
- **License allowlist too tight** — a future dep with an obscure but
  acceptable license (e.g., `BSL-1.0`) will fail. Fix: widen the
  allowlist in the PR that adds the dep, with justification.
- **`unmaintained = "all"` is noisy** — likely. Relax to `"workspace"`
  (only direct deps) if it proves too noisy in practice; filed as a
  future tuning item. Start strict.
- **Local dev UX** — contributors need `cargo install cargo-deny`
  locally to reproduce CI failures before pushing. Documented in
  `CONTRIBUTING.md` when that item lands (`ROADMAP.md:761`); until
  then, the CI failure is the signal.
