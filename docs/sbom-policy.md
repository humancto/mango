# SBOM policy (CycloneDX)

Mango publishes a Software Bill-of-Materials (SBOM) for every
workspace member so downstream scanners can answer "what is in
this binary?" without re-running cargo. The SBOM format is
[CycloneDX](https://cyclonedx.org/), the OWASP / ISO-IEC
5962:2021-aligned standard; the generator is
[`cargo-cyclonedx`](https://github.com/CycloneDX/cyclonedx-rust-cargo).

This doc is the policy. The CI enforcement is
[`.github/workflows/sbom.yml`](../.github/workflows/sbom.yml);
the shared validator is
[`scripts/sbom-check.sh`](../scripts/sbom-check.sh); the frozen
fixtures are under [`tests/fixtures/sbom/`](../tests/fixtures/sbom/).

## Why this exists (supply-chain transparency)

Three Phase 0 gates audit transitive deps at different layers:

- [`cargo-deny`](supply-chain-policy.md) — licenses, bans,
  duplicates, yanked crates.
- [`cargo-audit`](supply-chain-policy.md) — RustSec advisories.
- [`cargo-vet`](supply-chain-policy.md) — human source audits.

Those three gate a PR. The SBOM does NOT gate the dep graph —
that is cargo-vet / cargo-deny / cargo-audit's job. The SBOM
publishes a machine-readable description of what shipped so
downstream consumers (trivy, grype, Dependency-Track, customers'
own scanners) can perform their own policy checks without
needing our source tree. Publishing that artifact is the
security posture; the malformed-SBOM failure is the only
build-time assertion.

See [`unsafe-policy.md`](unsafe-policy.md) for the
growth-tracking counterpart that audits `unsafe` sites in our
own crates, and [`supply-chain-policy.md`](supply-chain-policy.md)
for the human-audit layer.

## What the gate produces

For each Mango workspace member, one CycloneDX 1.5 JSON file
written adjacent to its `Cargo.toml`:

| Crate             | Output                                            | Shippable?         |
| ----------------- | ------------------------------------------------- | ------------------ |
| `mango`           | `crates/mango/mango.cdx.json`                     | Yes (server crate) |
| `mango-proto`     | `crates/mango-proto/mango-proto.cdx.json`         | Yes (proto crate)  |
| `mango-loom-demo` | `crates/mango-loom-demo/mango-loom-demo.cdx.json` | Internal           |
| `xtask-vet-ttl`   | `crates/xtask-vet-ttl/xtask-vet-ttl.cdx.json`     | Internal           |

All four are generated now. Phase 12 decides which attach to a
GitHub Release; this gate does not pick that subset prematurely.
The CI workflow splits the upload into two artifacts:

- `sbom-release` — `mango.cdx.json` and `mango-proto.cdx.json`
  (intended to flow to release attachment in Phase 12).
- `sbom-internal` — the loom-demo and xtask SBOMs (useful for
  debug, not for publication).

## Format and spec version

- **Format**: CycloneDX JSON.
- **Spec version**: `1.5`. The `cargo-cyclonedx 0.5.9` default is
  `1.3`; our explicit `--spec-version 1.5` flag is
  load-bearing. Supported values on this tool version are `1.3`,
  `1.4`, `1.5`. `1.6` is NOT supported — a tool bump is required
  before we can emit it.
- **Build deps**: excluded via `--no-build-deps`. The SBOM
  describes what is linked into the shipped binary, not what ran
  during its build. `build.rs` transitives (prost, tonic-build,
  etc.) are not in the artifact, so including them would skew
  downstream vulnerability counts.
- **Reproducibility**: the workflow sets `SOURCE_DATE_EPOCH=1`
  before invoking the tool so `metadata.timestamp` is
  deterministic (`1970-01-01T00:00:01.000000000Z`) and the
  `serialNumber` field is `null` rather than a fresh UUID per
  run. This lets the gate run the generator twice and diff the
  output byte-for-byte (after stripping those two fields) to
  catch non-determinism from future tool changes.

## Validation contract

Every emitted SBOM is validated by
[`scripts/sbom-check.sh`](../scripts/sbom-check.sh) before the
artifact is uploaded. The validator asserts, per file:

1. File parses as JSON.
2. `bomFormat == "CycloneDX"`.
3. `specVersion == "1.5"`.
4. `metadata.component.name` matches the expected root crate
   name passed on the command line (prevents generator output
   being miswired to the wrong expected crate).
5. `metadata.tools[]` contains an entry with
   `name == "cargo-cyclonedx"` and `version` equal to the pinned
   `CARGO_CYCLONEDX_VERSION` — provenance assertion.
6. Volatile fields are shaped: `serialNumber` is either a
   `urn:uuid:...` string OR `null` (deterministic run);
   `metadata.timestamp` is a string.

The workflow additionally asserts, across all four files:

7. Exactly one SBOM per workspace member, using
   `cargo metadata --no-deps --format-version=1 | jq -r
'.packages[] | select(.source == null) | .name'` as the
   source of truth (same pattern as `geiger.yml`). Workspace
   members live under `crates/` and `benches/` so the workflow
   scans both roots.
8. Every `components[].purl` of shape `pkg:cargo/<name>@<version>`
   corresponds to a `(name, version)` row in `Cargo.lock`
   (forward check — every SBOM entry exists in the lockfile,
   proving no fabrication). The oracle is the full `Cargo.lock`
   without a `source` filter: Cargo.lock contains both registry
   packages and workspace members, and workspace-internal deps
   (e.g. mango-mvcc → mango-storage) legitimately appear as
   `pkg:cargo/<sibling>@<ver>` purls in cargo-cyclonedx output.
9. Every runtime (normal-edge) dep reachable from any
   workspace member — derived from `cargo tree --workspace
--edges=normal --prefix=none --no-dedupe` — appears in the
   SBOM union (reverse check). The oracle is `cargo tree`, NOT
   raw `Cargo.lock`, because `--no-build-deps` legitimately
   excludes build-time + dev-only deps (roughly 50 of
   `Cargo.lock`'s 79 entries in the current tree). Comparing
   against the lockfile directly would produce a false failure
   on every run. Extras in the SBOM over `cargo tree`'s output
   are permitted — `cargo-cyclonedx` can surface feature-gated
   deps that `cargo tree --no-dedupe` elides; the forward check
   has already verified each extra is in `Cargo.lock`. Attack
   this catches: a generator bug (or tampering) that silently
   drops a runtime dep from `components[]` while the forward
   check still passes because every remaining purl is valid.
10. Non-empty floors — `mango-proto.cdx.json` has ≥ 5
    `components[]`, `xtask-vet-ttl.cdx.json` has ≥ 10. `mango`
    and `mango-loom-demo` are exempt (currently 0 direct deps
    each). Current reality under `--no-build-deps` is 10 and 19
    respectively; floors are well below that so they won't flap
    on lockfile bumps, but still catch an empty-output bug.
11. Reproducibility diff — the generator runs twice;
    `.serialNumber` and `.metadata.timestamp` are stripped from
    both outputs; the diff must be empty.

## Tool pin

`cargo-cyclonedx 0.5.9`. Exact pin (not caret). Pinned in two
places that must move together:

- `.github/workflows/sbom.yml` → `env.CARGO_CYCLONEDX_VERSION`.
- `tests/fixtures/sbom/valid.json` → regenerated when the pin
  moves via `scripts/sbom-gen-fixtures.sh`.

The workflow includes a version-sanity step that asserts the
installed binary matches `CARGO_CYCLONEDX_VERSION` — different
tool versions can produce subtly different SBOM shapes, and a
silent drift would bypass the shape contract.

### Bump procedure

1. Edit `CARGO_CYCLONEDX_VERSION` in
   `.github/workflows/sbom.yml` to the new version.
2. Install that version locally: `cargo install --locked
cargo-cyclonedx --version <new>`.
3. Run `bash scripts/sbom-gen-fixtures.sh` to regenerate
   `tests/fixtures/sbom/valid.json`.
4. Run `bash scripts/sbom-scripts-test.sh` — must pass.
5. Commit the workflow bump + fixture regeneration in a single
   PR. CI will re-exercise the shape contract on the real
   workspace.

If the new version changes the shape contract itself (renames a
top-level key, etc.), `scripts/sbom-check.sh` may need updating;
the self-test will fail loudly rather than drifting silently.

## Fixture regeneration

`tests/fixtures/sbom/valid.json` is a frozen snapshot from the
pinned tool. It exists so `scripts/sbom-scripts-test.sh` can
exercise the validator without cargo in the loop — the test is
pure bash + jq.

Regenerate: `bash scripts/sbom-gen-fixtures.sh`.

The four invalid fixtures (`invalid-*.json`) are hand-crafted
mutations of the valid shape, each designed to fail exactly one
assertion. The self-test asserts each one fails via the
expected code path.

## Phase 12 forward reference

This gate produces the artifact; [Phase
12](../ROADMAP.md#phase-12--release-and-operational-polish)
attaches it to GitHub Releases alongside SLSA provenance
attestations. The CI artifact retention of 90 days is
intentionally transient — archival lives in the release.

### Known Phase-12 concern: `bom-ref` absolute path leakage

`cargo-cyclonedx 0.5.9` writes `metadata.component.bom-ref`
(and the matching field on each `components[]` entry) as
`path+file:///absolute/path/to/crate#version`. On a CI runner
this leaks `/home/runner/work/mango/mango/...`. The value is a
BOM-internal reference, not a consumption identifier (scanners
use `purl`), so it is not a correctness problem today — but
before attaching to a public release in Phase 12, we should
either:

- Set `--override-filename` + patch bom-refs to a relative form
  after generation (jq).
- Or accept it as "CI runner paths" — common in upstream CycloneDX
  SBOMs from cargo-cyclonedx but worth a conscious decision.

Tracked for Phase 12; this gate does not attempt to rewrite.

## Related docs

- [`supply-chain-policy.md`](supply-chain-policy.md) — cargo-vet
  / cargo-audit / cargo-deny human-audit layer.
- [`unsafe-policy.md`](unsafe-policy.md) — unsafe-growth gate,
  same three-layer defense pattern applied to `unsafe`.
- [`semver-policy.md`](semver-policy.md) — API surface gate that
  complements the SBOM for downstream stability signals.
