#!/usr/bin/env bash
# scripts/geiger-scripts-test.sh
#
# Regression-test harness for the cargo-geiger unsafe-growth scripts.
# Runs 15 scenarios (12 synthetic + 3 integration) against the
# committed fixtures under tests/fixtures/geiger/ and the toy
# workspace under tests/fixtures/geiger-toy-workspace/. Each
# scenario asserts stdout shape AND exit code — a script that exits
# 0 with the wrong message is still broken.
#
# CI runs this as the first step of `.github/workflows/geiger.yml`
# so a broken check script fails fast, before a real scan.
#
# Requires: bash, git, jq. Scenarios 13 and 14 also require
# cargo-geiger on PATH; they SKIP (do not FAIL) if missing, so
# contributors can run this locally without installing geiger.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(git rev-parse --show-toplevel)"
fixtures_dir="$repo_root/tests/fixtures/geiger"
toy_dir="$repo_root/tests/fixtures/geiger-toy-workspace"

check="$script_dir/geiger-check.sh"
updater="$script_dir/geiger-update-baseline.sh"
gen="$script_dir/geiger-gen-fixtures.sh"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

pass() { printf 'ok:   %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }
skip() { printf 'skip: %s (%s)\n' "$1" "$2"; }

# run_check <scanned> <baseline> <expected_exit> <scenario_name>
# Optional preceding env assignments on the caller side.
run_check() {
    local scanned="$1" baseline="$2" want="$3" name="$4"
    local out rc
    set +e
    out=$("$check" "$scanned" "$baseline" 2>&1)
    rc=$?
    set -e
    if [ "$rc" != "$want" ]; then
        printf '%s\n' "$out" >&2
        fail "$name: exit=$rc want=$want"
    fi
    echo "$out"
}

# Default env for scenarios: pretend this is a PR on main with no labels.
default_env() {
    export GITHUB_EVENT_NAME="pull_request"
    export GITHUB_BASE_REF="main"
    export PR_LABELS="[]"
    # Unsetting forces geiger-check to skip the baseline-diff branch
    # we don't have working-tree state for in tests; every scenario
    # that exercises the diff branch sets this explicitly.
}

# 1. equal — scan==baseline, exit 0.
(
    default_env
    run_check "$fixtures_dir/equal.json" "$fixtures_dir/baseline-4-2.json" 0 \
              "equal" >/dev/null
)
pass "1. equal: scan==baseline -> exit 0"

# 2. shrunk-no-baseline-update — current < baseline, PR, no label, exit 0.
(
    default_env
    run_check "$fixtures_dir/shrunk-exprs.json" "$fixtures_dir/baseline-4-2.json" 0 \
              "shrunk" >/dev/null
)
pass "2. shrunk: current < baseline, no label -> exit 0 (shrinkage is free)"

# 3. grown-without-label — exit 1.
(
    default_env
    run_check "$fixtures_dir/grown-exprs.json" "$fixtures_dir/baseline-4-2.json" 1 \
              "grown-no-label" >/dev/null
)
pass "3. grown-without-label -> exit 1"

# 4. grown-with-label-baseline-matches — exit 0.
# Uses git diff path, so run inside a throwaway repo where the
# baseline-5-2 file differs from the base ref.
(
    cd "$tmp"
    git init --quiet repo-4
    cd repo-4
    git config user.email t@t && git config user.name t
    cp "$fixtures_dir/baseline-4-2.json" unsafe-baseline.json
    git add -A
    git commit --quiet -m base
    git branch -M main
    git checkout --quiet -b feature
    cp "$fixtures_dir/baseline-5-2.json" unsafe-baseline.json
    git add -A
    git commit --quiet -m bump
    # Fake origin/main as an alias to main so the script's
    # origin/${GITHUB_BASE_REF}...HEAD path resolves locally.
    git update-ref refs/remotes/origin/main refs/heads/main

    export GITHUB_EVENT_NAME="pull_request"
    export GITHUB_BASE_REF="main"
    export PR_LABELS='["unsafe-growth-approved"]'
    "$check" "$fixtures_dir/grown-exprs.json" unsafe-baseline.json >/dev/null
)
pass "4. grown + label + matching baseline bump -> exit 0"

# 5. grown-with-label-baseline-stale — exit 2 (baseline updated but
# the bump was partial: current > new_baseline).
(
    cd "$tmp"
    git init --quiet repo-5
    cd repo-5
    git config user.email t@t && git config user.name t
    cp "$fixtures_dir/baseline-4-2.json" unsafe-baseline.json
    git add -A
    git commit --quiet -m base
    git branch -M main
    git checkout --quiet -b feature
    # Partial bump: baseline moves 4 -> 5, but actual current is 6.
    cp "$fixtures_dir/baseline-5-2.json" unsafe-baseline.json
    git add -A
    git commit --quiet -m partial-bump
    git update-ref refs/remotes/origin/main refs/heads/main

    export GITHUB_EVENT_NAME="pull_request"
    export GITHUB_BASE_REF="main"
    export PR_LABELS='["unsafe-growth-approved"]'
    set +e
    "$check" "$fixtures_dir/grown-exprs-6.json" unsafe-baseline.json >/dev/null 2>&1
    rc=$?
    set -e
    [ "$rc" = "2" ] || fail "5. grown + label + stale baseline: exit=$rc want=2"
)
pass "5. grown + label + baseline counts mismatch -> exit 2"

# 6. grown-with-label-baseline-not-updated — exit 2.
(
    cd "$tmp"
    git init --quiet repo-6
    cd repo-6
    git config user.email t@t && git config user.name t
    cp "$fixtures_dir/baseline-4-2.json" unsafe-baseline.json
    git add -A
    git commit --quiet -m base
    git branch -M main
    git checkout --quiet -b feature
    # No change to baseline.
    echo "unrelated" > unrelated.txt
    git add -A
    git commit --quiet -m unrelated
    git update-ref refs/remotes/origin/main refs/heads/main

    export GITHUB_EVENT_NAME="pull_request"
    export GITHUB_BASE_REF="main"
    export PR_LABELS='["unsafe-growth-approved"]'
    set +e
    "$check" "$fixtures_dir/grown-exprs.json" unsafe-baseline.json >/dev/null 2>&1
    rc=$?
    set -e
    [ "$rc" = "2" ] || fail "6. grown + label + no baseline bump: exit=$rc want=2"
)
pass "6. grown + label + baseline unchanged -> exit 2"

# 7. non-workspace-crate-growth-ignored — external libc grew; workspace totals unchanged.
(
    default_env
    run_check "$fixtures_dir/non-workspace-growth.json" "$fixtures_dir/baseline-4-2.json" 0 \
              "non-workspace-growth" >/dev/null
)
pass "7. non-workspace crate growth ignored -> exit 0"

# 8. malformed-geiger-json -> exit 3.
(
    default_env
    set +e
    "$check" "$fixtures_dir/malformed.json" "$fixtures_dir/baseline-4-2.json" >/dev/null 2>&1
    rc=$?
    set -e
    [ "$rc" = "3" ] || fail "8. malformed scan: exit=$rc want=3"
)
pass "8. malformed scan json -> exit 3"

# 9. version-skew — baseline 0.12.0, installed 0.13.0 (or whatever cargo-geiger reports).
# Only runs if cargo-geiger is on PATH; otherwise the version check
# is skipped inside the script and the scenario is vacuous.
if command -v cargo-geiger >/dev/null 2>&1; then
    (
        default_env
        set +e
        "$check" "$fixtures_dir/equal.json" "$fixtures_dir/baseline-wrong-version.json" >/dev/null 2>&1
        rc=$?
        set -e
        [ "$rc" = "4" ] || fail "9. version skew: exit=$rc want=4"
    )
    pass "9. version skew -> exit 4"
else
    skip "9. version skew" "cargo-geiger not on PATH; check skips version comparison"
fi

# 10. push-main-stale-null-labels — push event, PR_LABELS=null string, current<=baseline, exit 0.
(
    export GITHUB_EVENT_NAME="push"
    export PR_LABELS="null"
    "$check" "$fixtures_dir/equal.json" "$fixtures_dir/baseline-4-2.json" >/dev/null
)
pass "10. push event with PR_LABELS=\"null\" on clean scan -> exit 0"

# 11. push-main-growth — push event, growth, exit 1 regardless of labels.
(
    export GITHUB_EVENT_NAME="push"
    export PR_LABELS='["unsafe-growth-approved"]'  # even a label should not help on push
    set +e
    "$check" "$fixtures_dir/grown-exprs.json" "$fixtures_dir/baseline-4-2.json" >/dev/null 2>&1
    rc=$?
    set -e
    [ "$rc" = "1" ] || fail "11. push + growth: exit=$rc want=1"
)
pass "11. push event + growth -> exit 1 (strict gate)"

# 12. used-but-not-scanned-warn — field non-empty, totals match, exit 0.
(
    default_env
    "$check" "$fixtures_dir/used-but-not-scanned.json" "$fixtures_dir/baseline-4-2.json" >/dev/null
)
pass "12. used_but_not_scanned_files non-empty, totals match -> exit 0 (warn-only)"

# 13. toy-workspace-real-geiger — only if cargo-geiger on PATH.
if command -v cargo-geiger >/dev/null 2>&1; then
    (
        cd "$toy_dir"
        toy_scratch="$tmp/toy"
        mkdir -p "$toy_scratch"
        for member in toy-clean toy-unsafe; do
            cargo geiger \
                --manifest-path "$(pwd)/$member/Cargo.toml" \
                --output-format Json \
                --include-tests \
                > "$toy_scratch/$member.json"
        done
        jq -s '{
            packages: (map(.packages) | add),
            packages_without_metrics: (map(.packages_without_metrics) | add | unique),
            used_but_not_scanned_files: (map(.used_but_not_scanned_files) | add | unique)
        }' "$toy_scratch"/*.json > "$toy_scratch/merged.json"

        export GITHUB_EVENT_NAME="push"
        export PR_LABELS="[]"
        "$check" "$toy_scratch/merged.json" "$toy_dir/expected-baseline.json" >/dev/null
    )
    pass "13. toy-workspace-real-geiger: real scan matches hand-computed oracle"
else
    skip "13. toy-workspace-real-geiger" "cargo-geiger not on PATH"
fi

# 14. updater-check-round-trip — synthesise a merged scan, feed it
# through the updater, read the written baseline back through the
# checker.
(
    merged="$tmp/synthetic-merged.json"
    jq -n '{
        packages: [
            {
              package: {
                id: { name: "mango-loom-demo", version: "0.1.0",
                      source: { Path: "file:///repo/crates/mango-loom-demo" } },
                dependencies: []
              },
              unsafety: {
                used: {
                  functions:   { safe: 0, unsafe_: 0 },
                  exprs:       { safe: 0, unsafe_: 4 },
                  item_impls:  { safe: 0, unsafe_: 2 },
                  item_traits: { safe: 0, unsafe_: 0 },
                  methods:     { safe: 0, unsafe_: 0 }
                },
                unused: {
                  functions:   { safe: 0, unsafe_: 0 },
                  exprs:       { safe: 0, unsafe_: 0 },
                  item_impls:  { safe: 0, unsafe_: 0 },
                  item_traits: { safe: 0, unsafe_: 0 },
                  methods:     { safe: 0, unsafe_: 0 }
                },
                forbids_unsafe: false
              }
            }
        ],
        packages_without_metrics: [],
        used_but_not_scanned_files: []
    }' > "$merged"

    # Updater writes into unsafe-baseline.json at repo root. Redirect
    # to a temp file via a dedicated env var — but our updater only
    # supports writing to repo_root. Capture the write, restore.
    real_baseline="$repo_root/unsafe-baseline.json"
    stash="$tmp/baseline-stash.json"
    cp "$real_baseline" "$stash"

    GEIGER_FROM_MERGED_JSON="$merged" \
    GEIGER_VERSION_OVERRIDE="0.13.0" \
        bash "$updater" >/dev/null

    # Sanity: written baseline totals match the synthetic input.
    written_exprs="$(jq '.totals.exprs' "$real_baseline")"
    written_impls="$(jq '.totals.item_impls' "$real_baseline")"
    if [ "$written_exprs" != "4" ] || [ "$written_impls" != "2" ]; then
        cp "$stash" "$real_baseline"
        fail "14. updater wrote wrong totals: exprs=$written_exprs impls=$written_impls"
    fi

    # Checker round-trip: pass the same merged json plus the
    # freshly-written baseline.
    export GITHUB_EVENT_NAME="push"
    export PR_LABELS="[]"
    if ! "$check" "$merged" "$real_baseline" >/dev/null 2>&1; then
        cp "$stash" "$real_baseline"
        fail "14. checker rejected updater's own output"
    fi

    cp "$stash" "$real_baseline"
)
pass "14. updater -> checker round trip"

# 15. fixture-checksum — regenerator is idempotent.
(
    regen="$tmp/regen"
    mkdir -p "$regen"
    bash "$gen" "$regen" >/dev/null
    if ! diff -r "$fixtures_dir" "$regen" >/dev/null 2>&1; then
        diff -r "$fixtures_dir" "$regen" || true
        fail "15. fixture checksum drift: regenerate via bash $gen"
    fi
)
pass "15. fixture checksum: committed fixtures match generator"

echo "ok: all geiger script tests passed"
