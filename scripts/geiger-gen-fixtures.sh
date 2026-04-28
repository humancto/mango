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

# --- storage-dep fixtures (ROADMAP:823, ADR 0002 §5 trigger #8) -----
#
# These fixtures exercise the storage-dep gate added in commit 1.
# A redb (Registry) and raft-engine (Git) entry pair forms the
# canonical storage-dep baseline; scenarios mutate it.
#
# Source-shape contract (cargo-geiger 0.13.0 Source enum, externally
# tagged):
#   Path     -> {"Path": "<url>"}
#   Git      -> {"Git": {"url": "<url>", "rev": "<sha>"}}
#   Registry -> {"Registry": {"name": "<name>", "url": "<url>"}}
#
# The Registry url value (canonical crates.io-index URL vs. the
# newer index.crates.io URL) is verified against a real CI run in
# commit 2 — these synthetic fixtures use the original
# rust-lang/crates.io-index value, which the checker matches by
# whole-object equality, not by URL substring.

# Helper: produce a packages-array with redb + raft-engine entries
# at the canonical baseline source pin and the supplied per-category
# counts. Args: redb_exprs raft_exprs.
storage_scan_packages() {
    local r_exprs="$1" rg_exprs="$2"
    cat <<EOF
[
  {
    "package": {
      "id": {
        "name": "redb",
        "version": "4.1.0",
        "source": {
          "Registry": {
            "name": "crates-io",
            "url": "https://github.com/rust-lang/crates.io-index"
          }
        }
      },
      "dependencies": []
    },
    "unsafety": {
      "used": {
        "functions":   { "safe": 0, "unsafe_": 0 },
        "exprs":       { "safe": 0, "unsafe_": $r_exprs },
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
  },
  {
    "package": {
      "id": {
        "name": "raft-engine",
        "version": "0.4.2",
        "source": {
          "Git": {
            "url": "https://github.com/humancto/raft-engine",
            "rev": "e1d738d9ad1c1fc4f5b21c8c73bf605b5696f535"
          }
        }
      },
      "dependencies": []
    },
    "unsafety": {
      "used": {
        "functions":   { "safe": 0, "unsafe_": 0 },
        "exprs":       { "safe": 0, "unsafe_": $rg_exprs },
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
]
EOF
}

# storage-equal — scan with redb=30, raft-engine=40 exprs at pinned
# sources; baseline pins the same numbers. Used as the canonical
# "no growth" scenario.
write storage-equal "$(
    storage_pkgs="$(storage_scan_packages 30 40)"
    jq --argjson sp "$storage_pkgs" '
        .packages[0].unsafety.used.exprs.unsafe_      = 4 |
        .packages[0].unsafety.used.item_impls.unsafe_ = 2 |
        .packages += $sp
    ' <<<"$base"
)"

# storage-grown-redb — redb exprs 30 -> 41 (+11, exceeds +10 budget).
# Scenario 17 expects exit 6 with ADR-refresh remediation.
write storage-grown-redb "$(
    storage_pkgs="$(storage_scan_packages 41 40)"
    jq --argjson sp "$storage_pkgs" '
        .packages[0].unsafety.used.exprs.unsafe_      = 4 |
        .packages[0].unsafety.used.item_impls.unsafe_ = 2 |
        .packages += $sp
    ' <<<"$base"
)"

# storage-trade-redb — sum unchanged (exprs −11, item_impls +11) on
# redb. Scenario 18 verifies per-category +10 enforcement (the
# trade-bypass that aggregate-sum would have missed).
write storage-trade-redb "$(
    storage_pkgs="$(storage_scan_packages 19 40)"
    jq --argjson sp "$storage_pkgs" '
        .packages[0].unsafety.used.exprs.unsafe_      = 4 |
        .packages[0].unsafety.used.item_impls.unsafe_ = 2 |
        .packages += $sp |
        ( .packages[]
          | select(.package.id.name == "redb")
          | .unsafety.used.item_impls.unsafe_ ) = 11 |
        .
    ' <<<"$base"
)"

# storage-version-drift — redb scan reports a different Registry
# entry (version still pinned by the source object key set; we
# change the version to also drift the source-match). Scenario 19
# expects exit 7 (re-pin needed).
write storage-version-drift "$(
    storage_pkgs="$(storage_scan_packages 30 40)"
    jq --argjson sp "$storage_pkgs" '
        .packages[0].unsafety.used.exprs.unsafe_      = 4 |
        .packages[0].unsafety.used.item_impls.unsafe_ = 2 |
        .packages += $sp |
        ( .packages[]
          | select(.package.id.name == "redb")
          | .package.id.version ) = "4.2.0" |
        .
    ' <<<"$base"
)"

# storage-stranger — scan contains a redb entry whose source has
# been rerouted (e.g. via [patch]). Scenario for the stranger
# detector (exit 7).
write storage-stranger "$(
    storage_pkgs="$(storage_scan_packages 30 40)"
    jq --argjson sp "$storage_pkgs" '
        .packages[0].unsafety.used.exprs.unsafe_      = 4 |
        .packages[0].unsafety.used.item_impls.unsafe_ = 2 |
        .packages += $sp |
        ( .packages[]
          | select(.package.id.name == "redb")
          | .package.id.source ) = {"Path": "file:///vendor/redb"} |
        .
    ' <<<"$base"
)"

# storage-missing-redb — scan omits redb entirely. Used by scenarios
# 20 (still in Cargo.toml -> feature-unification) and 21 (also
# absent from Cargo.toml -> dep removed). Both branches share this
# fixture; the scenarios diverge on the cargo-metadata side.
write storage-missing-redb "$(
    storage_pkgs="$(storage_scan_packages 30 40)"
    jq --argjson sp "$storage_pkgs" '
        .packages[0].unsafety.used.exprs.unsafe_      = 4 |
        .packages[0].unsafety.used.item_impls.unsafe_ = 2 |
        .packages += $sp |
        .packages |= map(select(.package.id.name != "redb"))
    ' <<<"$base"
)"

# --- storage-dep baseline fixtures ----------------------------------
# Canonical "required: true" baseline with both deps at the pinned
# source/version + per-category totals.
write storage-baseline-required-true '{
  "generated_by": "scripts/geiger-update-baseline.sh",
  "generated_at": "2026-04-28T00:00:00Z",
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
  },
  "storage_deps_required": true,
  "storage_deps": {
    "redb": {
      "source": {
        "Registry": {
          "name": "crates-io",
          "url": "https://github.com/rust-lang/crates.io-index"
        }
      },
      "version": "4.1.0",
      "totals": {
        "functions": 0,
        "exprs": 30,
        "item_impls": 0,
        "item_traits": 0,
        "methods": 0
      },
      "forbids_unsafe": false
    },
    "raft-engine": {
      "source": {
        "Git": {
          "url": "https://github.com/humancto/raft-engine",
          "rev": "e1d738d9ad1c1fc4f5b21c8c73bf605b5696f535"
        }
      },
      "version": "0.4.2",
      "totals": {
        "functions": 0,
        "exprs": 40,
        "item_impls": 0,
        "item_traits": 0,
        "methods": 0
      },
      "forbids_unsafe": false
    }
  }
}'

# storage-baseline-required-false — bootstrap dormant baseline
# (commit-1 shape). Used by scenario 16b: pair with a scan that
# would normally trip the stranger detector to confirm the
# dormancy guard skips the entire block.
write storage-baseline-required-false '{
  "generated_by": "scripts/geiger-update-baseline.sh",
  "generated_at": "2026-04-28T00:00:00Z",
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
  },
  "storage_deps_required": false
}'

# storage-baseline-missing-redb — required: true but redb entry
# absent. Scenario 23 (S2 schema check, exit 3).
write storage-baseline-missing-redb '{
  "generated_by": "scripts/geiger-update-baseline.sh",
  "generated_at": "2026-04-28T00:00:00Z",
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
  },
  "storage_deps_required": true,
  "storage_deps": {
    "raft-engine": {
      "source": {
        "Git": {
          "url": "https://github.com/humancto/raft-engine",
          "rev": "e1d738d9ad1c1fc4f5b21c8c73bf605b5696f535"
        }
      },
      "version": "0.4.2",
      "totals": {
        "functions": 0,
        "exprs": 40,
        "item_impls": 0,
        "item_traits": 0,
        "methods": 0
      },
      "forbids_unsafe": false
    }
  }
}'

# storage-baseline-tolerance — required: true with a forbidden
# per-dep `tolerance` field. Scenario for B3 (tolerance rejection,
# exit 3).
write storage-baseline-tolerance '{
  "generated_by": "scripts/geiger-update-baseline.sh",
  "generated_at": "2026-04-28T00:00:00Z",
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
  },
  "storage_deps_required": true,
  "storage_deps": {
    "redb": {
      "source": {
        "Registry": {
          "name": "crates-io",
          "url": "https://github.com/rust-lang/crates.io-index"
        }
      },
      "version": "4.1.0",
      "totals": {
        "functions": 0,
        "exprs": 30,
        "item_impls": 0,
        "item_traits": 0,
        "methods": 0
      },
      "forbids_unsafe": false,
      "tolerance": 20
    },
    "raft-engine": {
      "source": {
        "Git": {
          "url": "https://github.com/humancto/raft-engine",
          "rev": "e1d738d9ad1c1fc4f5b21c8c73bf605b5696f535"
        }
      },
      "version": "0.4.2",
      "totals": {
        "functions": 0,
        "exprs": 40,
        "item_impls": 0,
        "item_traits": 0,
        "methods": 0
      },
      "forbids_unsafe": false
    }
  }
}'

# storage-baseline-required-true-flake — same shape as
# storage-baseline-required-true but redb.exprs anchored at 29 (one
# below scan-time 30, simulating a small cargo-geiger
# nondeterminism flake within the +10 budget). Used by scenario 24
# to prove the in-tolerance re-anchor recovery path: default-mode
# updater rewrites totals to 30, no ADR refresh required.
write storage-baseline-required-true-flake '{
  "generated_by": "scripts/geiger-update-baseline.sh",
  "generated_at": "2026-04-28T00:00:00Z",
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
  },
  "storage_deps_required": true,
  "storage_deps": {
    "redb": {
      "source": {
        "Registry": {
          "name": "crates-io",
          "url": "https://github.com/rust-lang/crates.io-index"
        }
      },
      "version": "4.1.0",
      "totals": {
        "functions": 0,
        "exprs": 29,
        "item_impls": 0,
        "item_traits": 0,
        "methods": 0
      },
      "forbids_unsafe": false
    },
    "raft-engine": {
      "source": {
        "Git": {
          "url": "https://github.com/humancto/raft-engine",
          "rev": "e1d738d9ad1c1fc4f5b21c8c73bf605b5696f535"
        }
      },
      "version": "0.4.2",
      "totals": {
        "functions": 0,
        "exprs": 40,
        "item_impls": 0,
        "item_traits": 0,
        "methods": 0
      },
      "forbids_unsafe": false
    }
  }
}'

echo "wrote fixtures to $out_dir:"
ls -1 "$out_dir" | sed 's/^/  /'
