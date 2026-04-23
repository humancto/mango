# Supply-chain audit policy

Mango treats every transitive dependency as code that will run with
full process privileges. An unaudited dep is a supply-chain risk,
not a neutral checkbox. The machinery that enforces this is three
layered gates:

- **`cargo-deny`** (advisory / license / banned-crate / duplicate
  policy) — runs in `.github/workflows/ci.yml` against
  [`deny.toml`](../deny.toml). Details at the bottom of this doc.
- **`cargo-audit`** (RustSec advisory freshness) — runs in
  [`.github/workflows/audit.yml`](../.github/workflows/audit.yml) on
  push, PR, and a nightly cron.
- **`cargo-vet`** (human-reviewed source audit graph) — the subject
  of this document. Runs in
  [`.github/workflows/vet.yml`](../.github/workflows/vet.yml).

The three are not redundant. `cargo-deny` and `cargo-audit` react
to metadata (licenses, CVEs); `cargo-vet` attests to the _source_
of each crate version having been looked at by a human we trust.

This doc is the policy. The CI enforcement is
[`.github/workflows/vet.yml`](../.github/workflows/vet.yml); the
contributor helper is
[`scripts/vet-update.sh`](../scripts/vet-update.sh); the TTL binary
is [`crates/xtask-vet-ttl`](../crates/xtask-vet-ttl); the audit
graph itself is under [`supply-chain/`](../supply-chain/).

## Why this exists

Every `cargo build` compiles and runs code from dozens of upstream
maintainers. `cargo-audit` catches _known_ vulnerabilities in that
code, but known vulnerabilities are a small subset of "things a
malicious maintainer can put in a dep." The 2024 `xz-utils`
incident is the reference point: a multi-year social-engineering
attack that landed a backdoor in a universally-linked library. No
CVE database would have caught that on day zero.

`cargo-vet` closes the gap by tracking, for every version of every
crate in the dependency graph, a specific person who has looked at
the source and attests that it matches a declared criterion (for
us: `safe-to-deploy`). The audit graph is committed, reviewed, and
fully auditable.

Our `safe-to-deploy` bar: no obfuscation, no runtime code
generation from network input, no calls into `unsafe` FFI that
aren't documented and scoped, no behaviour at odds with the
crate's public API contract. This is the Mozilla criterion — we
deliberately reuse it instead of inventing a local one so that
Mozilla's own audits transitively apply to crates we both depend
on.

## What the gate enforces

On every PR that touches `Cargo.toml`, `Cargo.lock`,
`supply-chain/**`, `crates/xtask-vet-ttl/**`,
`.github/workflows/vet.yml`, or `scripts/vet-*.sh`, the workflow
runs two gates.

### Gate 1: `cargo vet check --locked --frozen`

Walks the Cargo resolver output and, for every `(crate, version)`
pair in the graph, demands one of:

1. An entry in [`supply-chain/audits.toml`](../supply-chain/audits.toml)
   — a first-party audit written by a mango contributor.
2. An entry in an imported audit set (see Import sets below) that
   covers this version.
3. An entry in [`supply-chain/config.toml`](../supply-chain/config.toml)'s
   `[[exemptions.<crate>]]` section — an escape hatch with a TTL
   (see Gate 2).

`--locked`: honour `Cargo.lock` exactly.
`--frozen`: no network fetches. The imports feed is hydrated from
[`supply-chain/imports.lock`](../supply-chain/imports.lock), which
is committed. CI after the checkout step runs fully offline.

Any new transitive dep that is neither audited nor exempted fails
here with a readable diff.

### Gate 2: `xtask-vet-ttl` (exemption TTLs)

cargo-vet natively supports `end:` dates on `[[trusted]]` and
`[[wildcard-audits]]` entries, and `cargo vet check` rejects any
entry past its `end`. cargo-vet does **not** support `end:` on
`[[exemptions]]` — which is the long tail of our supply-chain
graph.

[`crates/xtask-vet-ttl`](../crates/xtask-vet-ttl) is a small Rust
binary that parses every `[[exemptions.<crate>]]` entry, extracts a
`review-by: YYYY-MM-DD` token from its `notes` field, and fails CI
if any date is in the past. Without this, an exemption added
"temporarily" becomes permanent — review friction is how audits
stay fresh.

Exit codes:

- `0` every exemption either has a future review-by or none (the
  latter is advisory unless `--strict`).
- `1` at least one exemption's `review-by` is past, or (with
  `--strict`) at least one exemption is missing the token.
- `2` a date token is present but malformed (e.g., `2026-13-01`),
  or the config file is not syntactically valid TOML.

The distinction between `1` (expired) and `2` (malformed) is
deliberate: a typo in a new exemption should produce a different
signal than a stale exemption that needs renewing.

Run locally:

```bash
cargo run -q -p xtask-vet-ttl               # same gate CI runs
cargo run -q -p xtask-vet-ttl -- --list     # diagnostic listing
cargo run -q -p xtask-vet-ttl -- --strict   # treat missing tokens as failures
```

## Import sets

`supply-chain/config.toml` imports three external audit feeds:

| Set                 | URL                                                                                                                                                                            | Coverage                                                      |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------- |
| `mozilla`           | [raw.githubusercontent.com/mozilla/supply-chain/main/audits.toml](https://raw.githubusercontent.com/mozilla/supply-chain/main/audits.toml)                                     | Large general-purpose Rust ecosystem, especially std-adjacent |
| `google`            | [raw.githubusercontent.com/google/supply-chain/main/audits.toml](https://raw.githubusercontent.com/google/supply-chain/main/audits.toml)                                       | Broad, overlaps heavily with Mozilla; strong on gRPC / proto  |
| `bytecode-alliance` | [raw.githubusercontent.com/bytecodealliance/wasmtime/main/supply-chain/audits.toml](https://raw.githubusercontent.com/bytecodealliance/wasmtime/main/supply-chain/audits.toml) | wasmtime-adjacent (`wit-bindgen`, prost ecosystem)            |

URLs are canonical from the cargo-vet registry published by Mozilla:
<https://raw.githubusercontent.com/mozilla/cargo-vet/main/registry.toml>.
Verified HTTP 200 on 2026-04-23.

The URLs in `config.toml` are the _source_; what actually gets
checked is `supply-chain/imports.lock`, which is committed and
updated by `cargo vet regenerate imports`. This makes CI
deterministic: a feed rotating a key or moving a file does not
break a merged PR until someone explicitly refreshes the lock.

Adding a new import set is a no-op at the CI level (the lock only
grows if the new set covers a crate in our graph). We keep the
list small by policy — every import set is an additional trust
relationship, and supply-chain risk compounds.

## First-party policy entries

Every workspace crate has an entry in `config.toml`:

```toml
[policy.mango]
audit-as-crates-io = false
criteria = "safe-to-deploy"
```

- `audit-as-crates-io = false` because our crates are not published.
  Without this, cargo-vet tries to resolve them against crates.io
  and fails.
- `criteria = "safe-to-deploy"` pins the audit bar that transitive
  deps must clear when pulled in by this crate.

New workspace members MUST add a `[policy.<crate-name>]` block in
the same PR that adds the crate, or CI fails.

## Contributor workflow

### When a dep bump lands

```bash
# from the repo root, after editing Cargo.toml / cargo update
bash scripts/vet-update.sh
```

That script:

1. Verifies your local `cargo-vet` version matches the CI pin.
2. Runs `cargo vet regenerate imports` to refresh the lock.
3. Runs `cargo vet check --locked --frozen` (same as CI).
4. Runs `xtask-vet-ttl` (same as CI).
5. Prints a diff of `supply-chain/` for you to commit.

If the dep is covered by an existing imported audit, steps 2-4
succeed and `config.toml`'s exemption list may shrink.

If the dep is NOT covered, `cargo vet check` fails and lists the
crate + version. You have three options:

1. **Add an exemption** with a TTL. Edit `supply-chain/config.toml`:

   ```toml
   [[exemptions.new-crate]]
   version = "1.2.3"
   criteria = "safe-to-deploy"
   notes = "review-by: 2026-10-23 — upstream maintainer, audit pending"
   ```

   The `review-by` date is six months from the day you add the
   exemption. xtask-vet-ttl will fail CI on that date unless the
   entry is renewed (push the date forward after another eyeball
   pass) or replaced with a full audit.

2. **Write a first-party audit.** After reading the crate source
   and convincing yourself it meets `safe-to-deploy`:

   ```bash
   cargo vet certify new-crate 1.2.3
   ```

   This records you as the auditor in `supply-chain/audits.toml`.
   Most small utility crates can be audited in one sitting.

3. **Wait for the upstream import to cover it.** If the crate is
   widely used, Mozilla / Google may have already audited it;
   `cargo vet suggest` lists the specific import that would
   eliminate the exemption.

### When an exemption's `review-by` lapses

A lapsed exemption turns CI red with a specific message:

```
xtask-vet-ttl: N exemption(s) past review-by (today = YYYY-MM-DD):
  foo @ 1.2.3 -> review-by 2026-10-23
```

Do one of:

- Re-eyeball the crate and push the date six months forward.
- Replace the exemption with a full audit (`cargo vet certify`).
- Check whether the crate is now covered by an import
  (`bash scripts/vet-update.sh` then retry).

## CI pin bump procedure

`.github/workflows/vet.yml` pins the cargo-vet binary via
`CARGO_VET_VERSION`. Different vet versions can accept/reject the
same audit graph differently, so the pin is exact.

To bump:

1. Update `CARGO_VET_VERSION` in `vet.yml` to the new version.
2. Locally:
   ```bash
   cargo install cargo-vet --version <new> --locked --force
   cargo vet check --locked --frozen
   cargo run -q -p xtask-vet-ttl
   ```
3. If `config.toml`'s `[cargo-vet] version` field needs bumping
   (major.minor only — vet rejects x.y.z in this field), do it in
   the same PR.
4. Open a PR; rust-expert reviews for the same reasons it reviews
   any security-sensitive config change.

## What lives where

| File                          | Role                        | Hand-edited?                         |
| ----------------------------- | --------------------------- | ------------------------------------ |
| `supply-chain/config.toml`    | imports, policy, exemptions | yes (exemption notes, policy blocks) |
| `supply-chain/audits.toml`    | first-party audits          | via `cargo vet certify`              |
| `supply-chain/imports.lock`   | resolved imports            | via `cargo vet regenerate imports`   |
| `crates/xtask-vet-ttl/`       | exemption TTL binary        | yes                                  |
| `.github/workflows/vet.yml`   | CI gate                     | yes                                  |
| `scripts/vet-update.sh`       | contributor helper          | yes                                  |
| `scripts/vet-scripts-test.sh` | harness self-test           | yes                                  |

## Relationship to `cargo-deny` and `cargo-audit`

- `cargo-deny` ([`deny.toml`](../deny.toml)) gates licenses,
  duplicate versions, and banned crates (`openssl-sys`,
  `openssl-src`). Runs in `ci.yml`. Its `advisories` section also
  gates against the RustSec DB, with strict posture (yanked,
  unmaintained, unsound all fail).
- `cargo-audit` (`audit.yml`) gates RustSec advisories on push /
  PR / nightly cron. Nightly is the differentiator — it surfaces
  advisories published _after_ a PR merged.
- `cargo-vet` (this doc) gates source-level audit attestations.
  Slowest-moving, hardest to bypass, catches things the other two
  cannot (like a backdoor in a package whose metadata looks clean).

All three are required for `safe-to-deploy`.
