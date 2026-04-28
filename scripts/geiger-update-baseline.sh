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
# Modes (storage-dep handling — ROADMAP:823, ADR 0002 §5 trigger #8):
#
#   default                       Refresh storage_deps.*.totals and
#                                 .forbids_unsafe by matching each
#                                 baseline entry against the scan by
#                                 (name, source). NEVER rewrites
#                                 .source or .version. If the scan
#                                 lacks a matching (name, source) for
#                                 any pinned dep, exits 5 — preserves
#                                 the round-trip oracle (checker
#                                 accepts updater output ⟺ no source
#                                 drift).
#
#   MANGO_GEIGER_REPIN=1          Above, PLUS rewrites .source and
#                                 .version from the scan. Use after
#                                 an ADR 0002 §5 refresh, when the
#                                 maintainer has consciously decided
#                                 to accept new source/version pins.
#
#   GEIGER_FROM_MERGED_JSON=…     Test escape hatch: skip cargo-geiger
#                                 invocation, read merged JSON from
#                                 the env var (paired with
#                                 GEIGER_VERSION_OVERRIDE).
#
# Exit codes:
#   0  PASS, baseline written (or --dry-run shown)
#   2  prerequisite missing (jq, cargo-geiger)
#   3  workspace metadata error
#   4  baseline cargo_geiger_version pin != installed
#   5  storage-dep source/version drift in default mode (re-pin needed)
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

# Derive workspace-member crate names. Cargo 1.77+ changed
# `.workspace_members[]` from the old space-delimited form
# `"<name> <version> (path+file://...)"` to a bare PackageId like
# `path+file://.../crates/NAME#VERSION` (no spaces). Iterating
# `.packages[] | select(.source == null) | .name` returns just the
# crate names without relying on the PackageId shape — stable
# across cargo versions. We avoid `readarray` / `mapfile` because
# those are bash 4+ only, and macOS ships bash 3.2.
members=()
while IFS= read -r line; do
    members+=("$line")
done < <(
    cargo metadata --no-deps --format-version=1 \
        | jq -r '.packages[] | select(.source == null) | .name'
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

# ---------------------------------------------------------------------
# storage-dep section — preserve storage_deps_required, refresh
# storage_deps.*.totals (and source/version under MANGO_GEIGER_REPIN).
# ROADMAP:823, ADR 0002 §5 advisory trigger #8.
# ---------------------------------------------------------------------
storage_deps_required="false"
existing_storage_deps='{}'
if [ -f "$baseline_path" ]; then
    storage_deps_required="$(
        jq -r '.storage_deps_required // false | tostring' "$baseline_path"
    )"
    existing_storage_deps="$(jq -c '.storage_deps // {}' "$baseline_path")"
fi

repin_mode=0
if [ "${MANGO_GEIGER_REPIN:-}" = "1" ]; then
    repin_mode=1
fi

new_storage_deps='{}'
if [ "$(jq 'length' <<<"$existing_storage_deps")" -gt 0 ]; then
    # Iterate baseline-pinned deps in deterministic key order.
    for dep in $(jq -r 'keys[]' <<<"$existing_storage_deps"); do
        pinned_source="$(
            jq -c --arg d "$dep" '.[$d].source' <<<"$existing_storage_deps"
        )"
        pinned_version="$(
            jq -r --arg d "$dep" '.[$d].version' <<<"$existing_storage_deps"
        )"

        if [ "$repin_mode" = "1" ]; then
            # Match by name only; collapse identical (version, source)
            # tuples (cargo unifies on (name, version) so duplicates
            # are expected) and pick the first remaining.
            match="$(
                jq -c --arg d "$dep" '
                  .packages
                  | map(select(.package.id.name == $d))
                  | unique_by([.package.id.version, .package.id.source])
                  | .[0] // null
                ' "$scratch/merged.json"
            )"
        else
            # Default mode: must match (name, source) exactly.
            match="$(
                jq -c --arg d "$dep" --argjson src "$pinned_source" '
                  .packages
                  | map(select(.package.id.name == $d
                               and .package.id.source == $src))
                  | .[0] // null
                ' "$scratch/merged.json"
            )"
        fi

        if [ "$match" = "null" ]; then
            if [ "$repin_mode" = "1" ]; then
                printf 'error: storage-dep %s absent from scan (REPIN mode)\n' \
                    "$dep" >&2
                printf 'hint: dep may have been removed from Cargo.toml; delete the entry from unsafe-baseline.json explicitly.\n' >&2
                exit 5
            fi
            printf 'error: storage-dep %s missing from scan at pinned source\n' \
                "$dep" >&2
            printf '  pinned source: %s\n' "$pinned_source" >&2
            printf 'Remediation:\n' >&2
            printf '  1. Refresh ADR 0002 §5 trigger #8 documenting the source/version change.\n' >&2
            printf '  2. Re-run with MANGO_GEIGER_REPIN=1 to accept the new pin.\n' >&2
            exit 5
        fi

        new_totals_entry="$(
            jq -c '
              .unsafety.used
              | {
                  functions:   (.functions.unsafe_   // 0),
                  exprs:       (.exprs.unsafe_       // 0),
                  item_impls:  (.item_impls.unsafe_  // 0),
                  item_traits: (.item_traits.unsafe_ // 0),
                  methods:     (.methods.unsafe_     // 0)
                }
            ' <<<"$match"
        )"
        new_forbids="$(
            jq -c '.unsafety.forbids_unsafe // false' <<<"$match"
        )"

        if [ "$repin_mode" = "1" ]; then
            # REPIN: take source + version from the scan match.
            scan_source="$(jq -c '.package.id.source' <<<"$match")"
            scan_version="$(jq -r '.package.id.version' <<<"$match")"
            entry="$(
                jq -n \
                    --argjson src "$scan_source" \
                    --arg ver "$scan_version" \
                    --argjson tot "$new_totals_entry" \
                    --argjson fb "$new_forbids" '
                  {source: $src, version: $ver, totals: $tot, forbids_unsafe: $fb}
                '
            )"
        else
            # Default: preserve baseline source + version verbatim.
            entry="$(
                jq -n \
                    --argjson src "$pinned_source" \
                    --arg ver "$pinned_version" \
                    --argjson tot "$new_totals_entry" \
                    --argjson fb "$new_forbids" '
                  {source: $src, version: $ver, totals: $tot, forbids_unsafe: $fb}
                '
            )"
        fi

        new_storage_deps="$(
            jq -c --arg d "$dep" --argjson entry "$entry" \
                '. + {($d): $entry}' <<<"$new_storage_deps"
        )"
    done
fi

new_baseline="$(
    jq -n \
        --arg gen_by "scripts/geiger-update-baseline.sh" \
        --arg gen_at "$timestamp" \
        --arg ver "$installed_version" \
        --argjson crates "$per_crate" \
        --argjson totals "$totals" \
        --argjson sdr "$storage_deps_required" \
        --argjson sd "$new_storage_deps" '
      ({
        generated_by: $gen_by,
        generated_at: $gen_at,
        cargo_geiger_version: $ver,
        crates: $crates,
        totals: $totals,
        storage_deps_required: $sdr
      })
      + (if ($sd | length) > 0 then {storage_deps: $sd} else {} end)
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
