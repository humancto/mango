#!/usr/bin/env bash
# scripts/geiger-gen-fixtures.sh
#
# Regenerates synthetic cargo-geiger-scan fixtures under
# `tests/fixtures/geiger/` from a canonical base + jq mutations.
# Paired with `scripts/geiger-scripts-test.sh` scenario
# "fixture-checksum", which re-runs this generator into a temp
# directory and fails if the committed fixtures drift.
#
# The canonical base is one workspace-member package shaped like
# real cargo-geiger output (Path-sourced `package.id.source`, full
# `unsafety.used` tree). Each fixture tweaks it for the scenario
# its name describes.
#
# Requires: bash, jq.
set -euo pipefail

command -v jq >/dev/null 2>&1 || { echo "error: jq required" >&2; exit 2; }

repo_root="$(git rev-parse --show-toplevel)"
out_dir="${1:-$repo_root/tests/fixtures/geiger}"
mkdir -p "$out_dir"

# Canonical base scan — a single Path-sourced mango-loom-demo package
# with zero unsafe (overridden per-fixture below). External deps (if
# any were here) would live alongside with a Registry source; we
# omit them because geiger-check.sh filters them out anyway.
base='{
  "packages": [
    {
      "package": {
        "id": {
          "name": "mango-loom-demo",
          "version": "0.1.0",
          "source": { "Path": "file:///repo/crates/mango-loom-demo" }
        },
        "dependencies": []
      },
      "unsafety": {
        "used": {
          "functions":   { "safe": 0, "unsafe_": 0 },
          "exprs":       { "safe": 0, "unsafe_": 0 },
          "item_impls":  { "safe": 0, "unsafe_": 0 },
          "item_traits": { "safe": 0, "unsafe_": 0 },
          "methods":     { "safe": 0, "unsafe_": 0 }
        },
        "unused": {
          "functions":   { "safe": 0, "unsafe_": 0 },
          "exprs":       { "safe": 0, "unsafe_": 0 },
          "item_impls":  { "safe": 0, "unsafe_": 0 },
          "item_traits": { "safe": 0, "unsafe_": 0 },
          "methods":     { "safe": 0, "unsafe_": 0 }
        },
        "forbids_unsafe": false
      }
    }
  ],
  "packages_without_metrics": [],
  "used_but_not_scanned_files": []
}'

write() {
    local name="$1" content="$2"
    printf '%s' "$content" | jq . > "$out_dir/$name.json"
}

# --- fixtures -------------------------------------------------------
# Each helper produces a scan JSON for the named scenario. Baselines
# (to compare against) live alongside at the same scenario name with
# `-baseline` suffix when they differ from the canonical "totals
# match scan" shape.

# equal — scan matches baseline exactly (both 4 exprs, 2 impls).
write equal "$(jq '.packages[0].unsafety.used.exprs.unsafe_      = 4
                 | .packages[0].unsafety.used.item_impls.unsafe_ = 2' <<<"$base")"

# grown-exprs — exprs went from 4 -> 5.
write grown-exprs "$(jq '.packages[0].unsafety.used.exprs.unsafe_      = 5
                        | .packages[0].unsafety.used.item_impls.unsafe_ = 2' <<<"$base")"

# grown-exprs-6 — exprs went to 6. Used by the "stale baseline"
# scenario: current=6 but the PR bumped baseline only to 5, so the
# gate must still fail.
write grown-exprs-6 "$(jq '.packages[0].unsafety.used.exprs.unsafe_      = 6
                          | .packages[0].unsafety.used.item_impls.unsafe_ = 2' <<<"$base")"

# shrunk-exprs — exprs went from 4 -> 3 (monotonic allows, no bump needed).
write shrunk-exprs "$(jq '.packages[0].unsafety.used.exprs.unsafe_      = 3
                         | .packages[0].unsafety.used.item_impls.unsafe_ = 2' <<<"$base")"

# non-workspace-growth — add an external Registry-sourced package
# that grew its unsafe surface. Workspace totals stay equal to the
# baseline. geiger-check.sh must ignore the external package.
write non-workspace-growth "$(jq '
    .packages[0].unsafety.used.exprs.unsafe_      = 4 |
    .packages[0].unsafety.used.item_impls.unsafe_ = 2 |
    .packages += [{
      "package": {
        "id": {
          "name": "libc",
          "version": "0.2.150",
          "source": { "Registry": "https://github.com/rust-lang/crates.io-index" }
        },
        "dependencies": []
      },
      "unsafety": {
        "used": {
          "functions":   { "safe": 0, "unsafe_": 500 },
          "exprs":       { "safe": 0, "unsafe_": 5000 },
          "item_impls":  { "safe": 0, "unsafe_": 10 },
          "item_traits": { "safe": 0, "unsafe_": 0 },
          "methods":     { "safe": 0, "unsafe_": 0 }
        },
        "unused": {
          "functions":   { "safe": 0, "unsafe_": 0 },
          "exprs":       { "safe": 0, "unsafe_": 0 },
          "item_impls":  { "safe": 0, "unsafe_": 0 },
          "item_traits": { "safe": 0, "unsafe_": 0 },
          "methods":     { "safe": 0, "unsafe_": 0 }
        },
        "forbids_unsafe": false
      }
    }]
' <<<"$base")"

# malformed — empty top-level object, should hit exit 3.
write malformed '{}'

# used-but-not-scanned — counts match baseline but the warn field
# is non-empty. Must still exit 0.
write used-but-not-scanned "$(jq '
    .packages[0].unsafety.used.exprs.unsafe_      = 4 |
    .packages[0].unsafety.used.item_impls.unsafe_ = 2 |
    .used_but_not_scanned_files = ["mango-loom-demo/src/auto_generated.rs"]
' <<<"$base")"

# --- baseline fixtures (paired with scans above) --------------------
# These are baseline JSONs used by the test scenarios. The "matching"
# baseline is used by `equal` / `shrunk-exprs` / `non-workspace-growth`
# / `used-but-not-scanned`. The "pre-growth-matching" baseline is
# used to exercise growth scenarios from the scan side.

# baseline-4-2 — represents an `unsafe-baseline.json` with
# exprs=4, item_impls=2, everything else 0.
write baseline-4-2 '{
  "generated_by": "scripts/geiger-update-baseline.sh",
  "generated_at": "2026-04-23T00:00:00Z",
  "cargo_geiger_version": "0.13.0",
  "crates": {
    "mango-loom-demo": {
      "functions": 0,
      "exprs": 4,
      "item_impls": 2,
      "item_traits": 0,
      "methods": 0
    }
  },
  "totals": {
    "functions": 0,
    "exprs": 4,
    "item_impls": 2,
    "item_traits": 0,
    "methods": 0
  }
}'

# baseline-5-2 — matches `grown-exprs` exactly; used by the
# "grown-with-label-baseline-matches" scenario.
write baseline-5-2 '{
  "generated_by": "scripts/geiger-update-baseline.sh",
  "generated_at": "2026-04-23T00:00:00Z",
  "cargo_geiger_version": "0.13.0",
  "crates": {
    "mango-loom-demo": {
      "functions": 0,
      "exprs": 5,
      "item_impls": 2,
      "item_traits": 0,
      "methods": 0
    }
  },
  "totals": {
    "functions": 0,
    "exprs": 5,
    "item_impls": 2,
    "item_traits": 0,
    "methods": 0
  }
}'

# baseline-wrong-version — triggers exit 4.
write baseline-wrong-version '{
  "generated_by": "scripts/geiger-update-baseline.sh",
  "generated_at": "2026-04-23T00:00:00Z",
  "cargo_geiger_version": "0.12.0",
  "crates": {
    "mango-loom-demo": {
      "functions": 0,
      "exprs": 4,
      "item_impls": 2,
      "item_traits": 0,
      "methods": 0
    }
  },
  "totals": {
    "functions": 0,
    "exprs": 4,
    "item_impls": 2,
    "item_traits": 0,
    "methods": 0
  }
}'

echo "wrote fixtures to $out_dir:"
ls -1 "$out_dir" | sed 's/^/  /'
