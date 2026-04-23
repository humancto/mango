#!/usr/bin/env bash
# scripts/geiger-update-baseline.sh
#
# Contributor helper. Runs cargo-geiger per workspace member, merges
# the per-crate JSONs, extracts totals + per-crate counts, and
# rewrites `unsafe-baseline.json` at the repo root. See
# docs/unsafe-policy.md for the monotonic-growth policy this
# baseline enforces.
#
# Usage:
#   bash scripts/geiger-update-baseline.sh            # write baseline
#   bash scripts/geiger-update-baseline.sh --dry-run  # print diff only
#
# Requires: bash, cargo, cargo-geiger, jq, date.
set -euo pipefail

dry_run=0
if [ "${1:-}" = "--dry-run" ]; then
    dry_run=1
fi

command -v jq >/dev/null 2>&1 || { echo "error: jq required" >&2; exit 2; }

# Test-only escape hatch: if GEIGER_FROM_MERGED_JSON is set to a
# pre-computed merged scan JSON, skip invoking cargo-geiger and
# process that file instead. Lets `scripts/geiger-scripts-test.sh`
# exercise the updater's jq pipeline without needing cargo-geiger
# installed, which in turn lets the updater<->checker round-trip
# test (scenario 14) run locally. Version string must be provided
# via GEIGER_VERSION_OVERRIDE in the same mode.
if [ -z "${GEIGER_FROM_MERGED_JSON:-}" ]; then
    command -v cargo-geiger >/dev/null 2>&1 || {
        echo "error: cargo-geiger not found on PATH" >&2
        echo "hint:  cargo install --locked cargo-geiger --version 0.13.0" >&2
        exit 2
    }
fi

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

baseline_path="$repo_root/unsafe-baseline.json"
if [ -n "${GEIGER_FROM_MERGED_JSON:-}" ]; then
    installed_version="${GEIGER_VERSION_OVERRIDE:?GEIGER_VERSION_OVERRIDE required when GEIGER_FROM_MERGED_JSON set}"
else
    installed_version="$(cargo-geiger --version | awk '{print $2}')"
fi

# Version-skew guard: if a baseline exists, its pinned version must
# match the installed cargo-geiger. Bumping one without the other
# would silently regress CI semantics; fail loudly instead.
if [ -f "$baseline_path" ]; then
    existing_version="$(jq -r '.cargo_geiger_version // empty' "$baseline_path")"
    if [ -n "$existing_version" ] && [ "$existing_version" != "$installed_version" ]; then
        printf 'error: baseline pins cargo-geiger %s but %s is installed\n' \
            "$existing_version" "$installed_version" >&2
        printf 'bump both at once per docs/unsafe-policy.md "Version-bump procedure".\n' >&2
        exit 4
    fi
fi

# Derive workspace members. `workspace_members` entries look like
#   "<name> <version> (path+file://...)"
# — take the first whitespace-delimited token. We avoid `readarray`
# / `mapfile` here because those are bash 4+ only, and macOS ships
# bash 3.2; newline-split into IFS-delimited words instead.
members=()
while IFS= read -r line; do
    members+=("$line")
done < <(
    cargo metadata --no-deps --format-version=1 \
        | jq -r '.workspace_members
                 | map(split(" ") | .[0])
                 | .[]'
)

if [ "${#members[@]}" -eq 0 ]; then
    echo "error: no workspace members discovered via cargo metadata" >&2
    exit 3
fi

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

if [ -n "${GEIGER_FROM_MERGED_JSON:-}" ]; then
    # Test escape hatch: treat the supplied file as the already-merged
    # scan result and skip the per-crate cargo-geiger loop entirely.
    cp "$GEIGER_FROM_MERGED_JSON" "$scratch/merged.json"
else

printf 'scanning %d workspace members with cargo-geiger %s...\n' \
    "${#members[@]}" "$installed_version" >&2

# Per-crate scan. `cargo geiger --workspace` does NOT exist; we
# iterate and merge. --manifest-path needs an absolute path.
for member in "${members[@]}"; do
    manifest_rel="$(
        cargo metadata --no-deps --format-version=1 \
            | jq -r --arg m "$member" '
                .packages[]
                | select(.name == $m)
                | .manifest_path
              '
    )"
    if [ -z "$manifest_rel" ] || [ ! -f "$manifest_rel" ]; then
        printf 'error: cannot locate manifest for member %s\n' "$member" >&2
        exit 3
    fi
    printf '  %s (%s)\n' "$member" "$manifest_rel" >&2
    cargo geiger \
        --manifest-path "$manifest_rel" \
        --output-format Json \
        --include-tests \
        > "$scratch/$member.json" 2>"$scratch/$member.err" \
    || {
        printf 'error: cargo-geiger failed for %s\n' "$member" >&2
        cat "$scratch/$member.err" >&2 || true
        exit 3
    }
done

# Merge per-crate JSONs into a single scanned artifact that matches
# the shape expected by geiger-check.sh.
jq -s '{
    packages: (map(.packages) | add),
    packages_without_metrics: (map(.packages_without_metrics) | add | unique),
    used_but_not_scanned_files: (map(.used_but_not_scanned_files) | add | unique)
}' "$scratch"/*.json > "$scratch/merged.json"

fi  # end GEIGER_FROM_MERGED_JSON branch

# Keep only workspace members (Path source) — external deps are
# cargo-vet's concern, not this tool's.
workspace_filter='
  .packages
  | map(select(.package.id.source
               | type == "object" and has("Path")))
'

# Per-crate counts: map each workspace-member package to its unsafe
# category totals, keyed by package name.
per_crate="$(
    jq "$workspace_filter
        | map({key: .package.id.name, value: {
            functions:  (.unsafety.used.functions.unsafe_   // 0),
            exprs:      (.unsafety.used.exprs.unsafe_       // 0),
            item_impls: (.unsafety.used.item_impls.unsafe_  // 0),
            item_traits:(.unsafety.used.item_traits.unsafe_ // 0),
            methods:    (.unsafety.used.methods.unsafe_     // 0)
          }})
        | from_entries" "$scratch/merged.json"
)"

# Totals.
totals="$(
    jq -n --argjson c "$per_crate" '
      reduce ($c | to_entries)[] as $kv
        ({functions:0, exprs:0, item_impls:0, item_traits:0, methods:0};
         .functions   += $kv.value.functions   |
         .exprs       += $kv.value.exprs       |
         .item_impls  += $kv.value.item_impls  |
         .item_traits += $kv.value.item_traits |
         .methods     += $kv.value.methods)
    '
)"

timestamp="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"

new_baseline="$(
    jq -n \
        --arg gen_by "scripts/geiger-update-baseline.sh" \
        --arg gen_at "$timestamp" \
        --arg ver "$installed_version" \
        --argjson crates "$per_crate" \
        --argjson totals "$totals" '
      {
        generated_by: $gen_by,
        generated_at: $gen_at,
        cargo_geiger_version: $ver,
        crates: $crates,
        totals: $totals
      }
    '
)"

if [ "$dry_run" -eq 1 ]; then
    if [ -f "$baseline_path" ]; then
        echo "--- diff: existing baseline vs computed (dry-run) ---"
        diff -u \
            <(jq -S . "$baseline_path") \
            <(jq -S . <<<"$new_baseline") \
            || true
    else
        echo "--- no existing baseline; would write: ---"
        jq . <<<"$new_baseline"
    fi
    exit 0
fi

printf '%s\n' "$new_baseline" | jq . > "$baseline_path"
echo "wrote $baseline_path"
