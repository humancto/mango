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
#
# Requires: bash, jq, git (only on pull_request), cargo-geiger (only
# for the version check).
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
