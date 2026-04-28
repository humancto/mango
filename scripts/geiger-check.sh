#!/usr/bin/env bash
# scripts/geiger-check.sh
#
# Compare a merged cargo-geiger scan against the committed
# `unsafe-baseline.json` and enforce the monotonic unsafe-growth
# policy described in docs/unsafe-policy.md.
#
# Usage:
#   bash scripts/geiger-check.sh <scanned-json> <baseline-json>
#
# The script reads three environment variables to adapt to the
# CI event context (set by `.github/workflows/geiger.yml`):
#
#   GITHUB_EVENT_NAME   "pull_request" | "push" | "merge_group" | "workflow_dispatch"
#   GITHUB_BASE_REF     PR base branch name (only meaningful on pull_request)
#   PR_LABELS           JSON array of label names (set only on pull_request)
#
# All three are optional for local use; defaults treat the run as
# a non-PR push (strict gate).
#
# Exit codes:
#   0  PASS
#   1  growth without `unsafe-growth-approved` label
#   2  growth with label, baseline missing-bump or counts mismatch
#   3  scan-result validation error (unparseable JSON / missing fields)
#   4  version skew: baseline cargo_geiger_version != installed
#   6  storage-dep growth: per-category +10 over baseline (ROADMAP:823,
#      ADR 0002 §5 advisory trigger #8). Remediation: refresh ADR +
#      MANGO_GEIGER_REPIN=1 bash scripts/geiger-update-baseline.sh.
#   7  storage-dep re-pin needed: source/version drift, dep absent, or
#      stranger source detected. Remediation: bump baseline.
#
# Requires: bash, jq, git (only on pull_request), cargo-geiger (only
# for the version check), cargo (only for storage-dep B4
# disambiguation; tolerated absent locally).
set -euo pipefail

# ---------------------------------------------------------------------
# arg parsing
# ---------------------------------------------------------------------
if [ "$#" -ne 2 ]; then
    printf 'usage: %s <scanned.json> <baseline.json>\n' "$0" >&2
    exit 64
fi

scanned="$1"
baseline="$2"

command -v jq >/dev/null 2>&1 || {
    printf 'error: jq not found on PATH\n' >&2
    exit 3
}

[ -f "$scanned" ] || { printf 'error: scanned json not found: %s\n' "$scanned" >&2; exit 3; }
[ -f "$baseline" ] || { printf 'error: baseline not found: %s\n' "$baseline" >&2; exit 3; }

event="${GITHUB_EVENT_NAME:-push}"
base_ref="${GITHUB_BASE_REF:-}"
pr_labels="${PR_LABELS:-[]}"

# ---------------------------------------------------------------------
# version skew check
# ---------------------------------------------------------------------
baseline_version="$(jq -r '.cargo_geiger_version // empty' "$baseline")"
if [ -z "$baseline_version" ]; then
    printf 'error: baseline missing cargo_geiger_version field\n' >&2
    exit 3
fi

# Skip the version check if cargo-geiger is not on PATH (local dev
# may run this script without installing geiger). CI always has it.
if command -v cargo-geiger >/dev/null 2>&1; then
    actual_version="$(cargo-geiger --version | awk '{print $2}')"
    if [ "$baseline_version" != "$actual_version" ]; then
        printf 'error: baseline cargo-geiger version (%s) != installed (%s)\n' \
            "$baseline_version" "$actual_version" >&2
        exit 4
    fi
fi

# ---------------------------------------------------------------------
# derive workspace members from scanned JSON
#
# Workspace members have .packages[].package.id.source shaped as
# {"Path": "file://..."}. External deps carry {"Registry": ...} or
# null. We keep only Path-sourced packages, which — thanks to the
# per-crate scan loop in geiger.yml — are exactly mango's workspace
# members.
# ---------------------------------------------------------------------
workspace_filter='
  .packages
  | map(select(.package.id.source
               | type == "object" and has("Path")))
'

# Validate the scanned JSON has the top-level shape we expect.
if ! jq -e "$workspace_filter" "$scanned" >/dev/null 2>&1; then
    printf 'error: scanned json does not have expected .packages[].package.id.source shape\n' >&2
    exit 3
fi

# ---------------------------------------------------------------------
# storage-dep gate (ROADMAP:823, ADR 0002 §5 advisory trigger #8)
#
# Runs BEFORE the workspace gate so a label-approved workspace bump
# cannot mask a storage-dep regression (M4 / scenario 22 in
# geiger-scripts-test.sh).
#
# Dormancy guard (B5): when storage_deps_required is false or
# missing, this entire block is skipped — including the stranger
# detector. The dormant state is the bootstrap window between
# commit 1 (this commit) and commit 2 (real numbers + flag flip)
# of the storage-dep PR, and IS the single bypass; documented in
# unsafe-policy.md.
#
# Exit codes specific to this block:
#   3 — schema error: required dep missing, tolerance field present
#   6 — per-category +10 growth (ADR 0002 §5 trigger #8)
#   7 — source/version drift, dep absent, stranger source
# ---------------------------------------------------------------------

# Hardcoded list — policy lives in the checker, not the baseline,
# so a maintainer cannot widen the gate by editing JSON. Adding a
# dep here is a conscious source-edit reviewed alongside ADR 0002.
STORAGE_REQUIRED_DEPS=(redb raft-engine)

storage_deps_required="$(jq -r '.storage_deps_required // false' "$baseline")"

if [ "$storage_deps_required" = "true" ]; then
    # B3: reject per-dep tolerance fields. Tolerance is hardcoded
    # at +10 per category by policy. A maintainer must not be able
    # to weaken the gate by editing this JSON.
    tol_offenders="$(
        jq -r '
          (.storage_deps // {})
          | to_entries
          | map(select(.value | has("tolerance")))
          | map(.key)
          | .[]
        ' "$baseline"
    )"
    if [ -n "$tol_offenders" ]; then
        printf 'error: tolerance is not configurable per-dep; remove field from:\n' >&2
        printf '  storage_deps.%s\n' $tol_offenders >&2
        printf 'Tolerance is hardcoded at +10 per category by policy.\n' >&2
        exit 3
    fi

    # S2: schema check — every required dep must be in storage_deps.
    # The bootstrap PR populates them; a future PR that deletes an
    # entry hits this branch and fails.
    for dep in "${STORAGE_REQUIRED_DEPS[@]}"; do
        present="$(jq -r --arg d "$dep" '.storage_deps | has($d)' "$baseline")"
        if [ "$present" != "true" ]; then
            printf 'error: schema: storage_deps.%s required when storage_deps_required: true\n' \
                "$dep" >&2
            printf 'Either add the entry or flip storage_deps_required to false (and explain in PR description).\n' >&2
            exit 3
        fi
    done

    # M1 (stranger detector): enumerate every package in the scan
    # whose name is in STORAGE_REQUIRED_DEPS but whose source does
    # not match the baseline pin. Catches [patch] reroutes, vendor
    # substitutions, and accidental path overrides. Runs BEFORE
    # per-dep matching (per-dep code can then assume any matching-
    # name packages share the pinned source).
    strangers_jq='
      ($req | split(",")) as $req_arr |
      .packages
      | map(select(.package.id.name as $n | $req_arr | index($n)))
      | map({
          name: .package.id.name,
          scan_source: .package.id.source,
          pinned_source: ($base.storage_deps[.package.id.name].source // null)
        })
      | map(select(.pinned_source != null and .scan_source != .pinned_source))
    '
    req_csv="$(IFS=,; printf '%s' "${STORAGE_REQUIRED_DEPS[*]}")"
    base_obj="$(cat "$baseline")"
    strangers="$(
        jq -c --arg req "$req_csv" --argjson base "$base_obj" "$strangers_jq" "$scanned"
    )"
    if [ "$(jq 'length' <<<"$strangers")" -gt 0 ]; then
        printf 'FAIL: unexpected storage-dep sources detected:\n' >&2
        jq -r '.[] | "  \(.name): scan=\(.scan_source | tostring)  pinned=\(.pinned_source | tostring)"' \
            <<<"$strangers" >&2
        printf '\nThis indicates a [patch] reroute, a vendor substitution, or a fork URL change.\n' >&2
        printf 'Remediation: re-pin the baseline (MANGO_GEIGER_REPIN=1 bash scripts/geiger-update-baseline.sh)\n' >&2
        printf 'AND review whether the source change is intentional (cargo-vet entry, [patch] table).\n' >&2
        exit 7
    fi

    # B4 disambiguation: cache cargo-metadata package names so we
    # can distinguish "dep dropped from Cargo.toml" (legitimate,
    # bump baseline + ADR) from "scan dropped it" (cargo-geiger /
    # feature-unification bug). Only invoked on the matches=0
    # branch below; cached because multiple deps could end up
    # there.
    storage_cargo_meta_cache=""
    storage_cargo_meta_init() {
        if [ -n "$storage_cargo_meta_cache" ]; then
            return 0
        fi
        if ! command -v cargo >/dev/null 2>&1; then
            # Defensive: existing checker tolerates cargo-geiger
            # missing locally; preserve that contract for cargo
            # too. Sentinel "MISSING" lets the caller distinguish.
            storage_cargo_meta_cache="MISSING"
            return 0
        fi
        # We want transitive deps too — redb / raft-engine are
        # NOT workspace members, so `--no-deps` would always
        # report them absent and miss the "feature-unification
        # bug" branch. `--offline` first for reproducibility,
        # falling back to network metadata if the offline cache
        # is empty.
        storage_cargo_meta_cache="$(
            cargo metadata --format-version=1 --offline 2>/dev/null \
                | jq -r '.packages[].name' \
                || cargo metadata --format-version=1 2>/dev/null \
                | jq -r '.packages[].name' \
                || true
        )"
        if [ -z "$storage_cargo_meta_cache" ]; then
            storage_cargo_meta_cache="MISSING"
        fi
    }

    # Per-dep loop: match by (name, source, version), assert dedup
    # consistency, check per-category +10.
    #
    # Version is part of the match key because the cargo-geiger
    # Source enum does NOT include version (Registry carries only
    # name + url). A 4.1.0 -> 4.2.0 bump from the same Registry
    # source yields an identical source object, so source-only
    # matching would silently accept the upgrade. Adding version
    # to the match makes version drift surface as exit 7.
    for dep in "${STORAGE_REQUIRED_DEPS[@]}"; do
        pinned_source="$(jq -c --arg d "$dep" '.storage_deps[$d].source' "$baseline")"
        pinned_version="$(jq -r --arg d "$dep" '.storage_deps[$d].version' "$baseline")"
        pinned_totals="$(jq -c --arg d "$dep" '.storage_deps[$d].totals' "$baseline")"

        # Find all scan packages with matching name AND source AND version.
        matches="$(
            jq -c --arg d "$dep" --arg ver "$pinned_version" --argjson src "$pinned_source" '
              .packages
              | map(select(.package.id.name == $d
                           and .package.id.source == $src
                           and .package.id.version == $ver))
            ' "$scanned"
        )"
        match_count="$(jq 'length' <<<"$matches")"

        if [ "$match_count" = "0" ]; then
            # Version-drift branch: same name + same source but
            # different version. The stranger detector already
            # caught same-name-different-source above, so a
            # remaining matching-source mismatch is purely a
            # version bump.
            version_drift="$(
                jq -c --arg d "$dep" --argjson src "$pinned_source" '
                  .packages
                  | map(select(.package.id.name == $d
                               and .package.id.source == $src))
                  | map(.package.id.version)
                  | unique
                ' "$scanned"
            )"
            if [ "$(jq 'length' <<<"$version_drift")" -gt 0 ]; then
                printf 'FAIL: %s version drift: scan reports %s, baseline pins %s.\n' \
                    "$dep" "$(jq -r 'join(", ")' <<<"$version_drift")" \
                    "$pinned_version" >&2
                printf 'Remediation: refresh ADR 0002 §5 trigger #8, then\n' >&2
                printf '  MANGO_GEIGER_REPIN=1 bash scripts/geiger-update-baseline.sh\n' >&2
                exit 7
            fi

            # B4: distinguish "removed from Cargo.toml" from "scan
            # dropped it". Strangers were already caught above, so
            # we know there's no same-name-different-source either.
            storage_cargo_meta_init
            if [ "$storage_cargo_meta_cache" = "MISSING" ]; then
                printf 'FAIL: %s absent from scan and `cargo` not on PATH; cannot disambiguate.\n' \
                    "$dep" >&2
                printf 'Install cargo or run in a workspace with cargo on PATH.\n' >&2
                exit 7
            fi
            in_cargo_meta=0
            for name in $storage_cargo_meta_cache; do
                if [ "$name" = "$dep" ]; then
                    in_cargo_meta=1
                    break
                fi
            done
            if [ "$in_cargo_meta" = "1" ]; then
                printf 'FAIL: %s declared in Cargo.toml but absent from cargo-geiger scan.\n' \
                    "$dep" >&2
                printf 'Likely cause: feature-unification bug or cargo-geiger flake.\n' >&2
                printf 'Remediation: re-run the geiger workflow; if persistent, file an issue.\n' >&2
                exit 7
            else
                printf 'FAIL: %s removed from Cargo.toml but still pinned in storage_deps.\n' \
                    "$dep" >&2
                printf 'Remediation: refresh ADR 0002 §5 trigger #8 (engine swap event), then\n' >&2
                printf '  delete storage_deps.%s from unsafe-baseline.json.\n' "$dep" >&2
                exit 7
            fi
        fi

        # S3: dedup-with-identical-counts assertion. cargo's
        # version unification means N matches must report the
        # same `unsafety.used` block; if not, something is wrong
        # with cargo-geiger or the scan was concatenated incorrectly.
        unique_used="$(
            jq -c '[.[].unsafety.used] | unique' <<<"$matches"
        )"
        if [ "$(jq 'length' <<<"$unique_used")" != "1" ]; then
            printf 'error: %s reported with inconsistent counts across %s occurrences in merged scan\n' \
                "$dep" "$match_count" >&2
            jq -r '.[] | "  \(.)"' <<<"$unique_used" >&2
            exit 3
        fi
        current_used="$(jq -c '.[0]' <<<"$unique_used")"

        # B2: per-category +10. Aggregate-sum was bypassable via
        # category-trade (exprs ↓11, methods ↑11). Check each.
        for cat in functions exprs item_impls item_traits methods; do
            cur="$(jq -r --arg c "$cat" '.[$c].unsafe_ // 0' <<<"$current_used")"
            base="$(jq -r --arg c "$cat" '.[$c] // 0' <<<"$pinned_totals")"
            limit=$((base + 10))
            if [ "$cur" -gt "$limit" ]; then
                printf 'FAIL: storage-dep %s.%s grew beyond +10 tolerance.\n' "$dep" "$cat" >&2
                printf '  current:  %s\n' "$cur" >&2
                printf '  baseline: %s\n' "$base" >&2
                printf '  limit:    %s (baseline + 10)\n' "$limit" >&2
                printf '\nADR 0002 §5 advisory trigger #8: refresh required.\n' >&2
                printf 'Remediation:\n' >&2
                printf '  1. Refresh ADR 0002 §5 trigger #8 with the new numbers and\n' >&2
                printf '     a sentence on what new unsafe surface appeared in %s.\n' "$dep" >&2
                printf '  2. Run: MANGO_GEIGER_REPIN=1 bash scripts/geiger-update-baseline.sh\n' >&2
                printf '  3. Commit ADR + baseline in the same PR.\n' >&2
                exit 6
            fi
        done
    done
fi

# ---------------------------------------------------------------------
# extract current totals
# ---------------------------------------------------------------------
current_totals_json="$(
    jq -c "
      [$workspace_filter | .[] | .unsafety.used] |
      reduce .[] as \$u ({
        functions:0, exprs:0, item_impls:0, item_traits:0, methods:0
      };
        .functions  += (\$u.functions.unsafe_  // 0) |
        .exprs      += (\$u.exprs.unsafe_      // 0) |
        .item_impls += (\$u.item_impls.unsafe_ // 0) |
        .item_traits+= (\$u.item_traits.unsafe_// 0) |
        .methods    += (\$u.methods.unsafe_    // 0)
      )
    " "$scanned"
)"

baseline_totals_json="$(jq -c '.totals' "$baseline")"
if [ "$baseline_totals_json" = "null" ]; then
    printf 'error: baseline missing .totals\n' >&2
    exit 3
fi

# ---------------------------------------------------------------------
# compare: categorise as equal / shrunk / grown per category
# ---------------------------------------------------------------------
comparison_json="$(
    jq -c -n \
        --argjson cur "$current_totals_json" \
        --argjson base "$baseline_totals_json" '
      ["functions","exprs","item_impls","item_traits","methods"] as $cats |
      reduce $cats[] as $c ({grown:[], shrunk:[], equal:[]};
        ($cur[$c] // 0) as $cv |
        ($base[$c] // 0) as $bv |
        if   $cv >  $bv then .grown  += [{cat:$c, cur:$cv, base:$bv}]
        elif $cv <  $bv then .shrunk += [{cat:$c, cur:$cv, base:$bv}]
        else                 .equal  += [{cat:$c, cur:$cv, base:$bv}]
        end
      )
    '
)"

grown_count="$(jq '.grown | length' <<<"$comparison_json")"

print_totals_table() {
    jq -r -n \
        --argjson cur "$current_totals_json" \
        --argjson base "$baseline_totals_json" '
      "category       current  baseline  delta",
      "------------- -------- --------- -------",
      (["functions","exprs","item_impls","item_traits","methods"][] as $c |
         ($cur[$c]//0) as $cv | ($base[$c]//0) as $bv |
         "\($c | . + "             " | .[0:13])  \($cv | tostring | . + "        " | .[0:7])  \($bv | tostring | . + "        " | .[0:8])  \(($cv - $bv) | tostring)")'
}

# ---------------------------------------------------------------------
# PASS: no growth
# ---------------------------------------------------------------------
if [ "$grown_count" = "0" ]; then
    echo "PASS: no unsafe growth versus baseline"
    print_totals_table
    exit 0
fi

# ---------------------------------------------------------------------
# FAIL path: at least one category grew. Event-dispatch:
#   push / merge_group / anything-not-PR : strict gate → exit 1
#   pull_request                         : consult label + baseline diff
# ---------------------------------------------------------------------
print_growth_summary() {
    printf 'FAIL: unsafe growth detected\n'
    print_totals_table
    printf '\nGrown categories:\n'
    jq -r '.grown[] | "  - \(.cat): \(.base) -> \(.cur) (+\(.cur - .base))"' <<<"$comparison_json"
}

if [ "$event" != "pull_request" ]; then
    print_growth_summary
    printf '\nNon-PR event (%s): strict gate, growth is not allowed.\n' "$event" >&2
    exit 1
fi

# --- pull_request branch ---------------------------------------------
# Normalise labels: env may arrive as "null" string (if a future
# workflow edit regresses) or a JSON array. Anything non-array yields
# an empty label set.
label_match="$(
    printf '%s' "$pr_labels" | jq -r '
      if type == "array" then
        .[] | select(. == "unsafe-growth-approved")
      else empty end
    ' 2>/dev/null || true
)"

if [ -z "$label_match" ]; then
    print_growth_summary
    printf '\nNo `unsafe-growth-approved` label on this PR.\n'
    printf 'Remediation: justify the growth, ask a maintainer to apply the label.\n' >&2
    exit 1
fi

# Label present — baseline must have been updated in this PR.
if ! command -v git >/dev/null 2>&1; then
    printf 'error: git not available, cannot verify baseline bump\n' >&2
    exit 3
fi

if [ -z "$base_ref" ]; then
    printf 'error: GITHUB_BASE_REF unset on pull_request event\n' >&2
    exit 3
fi

# Note: requires `fetch-depth: 0` in actions/checkout so the full
# history is available for `git diff` across the merge-base.
baseline_changed="$(
    git diff --name-only "origin/${base_ref}...HEAD" -- "$baseline" 2>/dev/null || true
)"

if [ -z "$baseline_changed" ]; then
    print_growth_summary
    printf '\nLabel is present but unsafe-baseline.json was not updated in this PR.\n'
    printf 'Remediation: run `bash scripts/geiger-update-baseline.sh` and commit.\n' >&2
    exit 2
fi

# Baseline was updated. Do the counts now match current?
if [ "$current_totals_json" != "$baseline_totals_json" ]; then
    print_growth_summary
    printf '\nBaseline was updated but totals still differ from current scan.\n'
    printf 'Remediation: rebase on origin/main, re-run `bash scripts/geiger-update-baseline.sh`, commit.\n' >&2
    exit 2
fi

echo "PASS: unsafe growth approved by label and baseline matches current scan"
print_totals_table
exit 0
