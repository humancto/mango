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

# ---------------------------------------------------------------------
# Storage-dep scenarios (ROADMAP:823, ADR 0002 §5 advisory trigger #8)
# ---------------------------------------------------------------------

# 16. storage-dep within tolerance — required: true baseline,
# scan reports same numbers, exit 0.
(
    default_env
    run_check "$fixtures_dir/storage-equal.json" \
              "$fixtures_dir/storage-baseline-required-true.json" 0 \
              "16. storage-dep within tolerance" >/dev/null
)
pass "16. storage-dep within tolerance -> exit 0"

# 16b. dormancy guard — required: false baseline + a scan that
# would normally trip the storage-dep block (version drift on
# redb). Exit 0. Without this, a regression where the dormancy
# guard silently ran the storage-dep checks anyway would slip
# through commit 1's bootstrap window.
(
    default_env
    run_check "$fixtures_dir/storage-version-drift.json" \
              "$fixtures_dir/storage-baseline-required-false.json" 0 \
              "16b. dormancy guard" >/dev/null
)
pass "16b. dormancy guard: required:false skips storage-dep block -> exit 0"

# 17. storage-dep over tolerance per-category — redb.exprs +11.
# Exit 6 with ADR-refresh remediation. Stdout must mention ADR.
(
    default_env
    set +e
    out="$("$check" "$fixtures_dir/storage-grown-redb.json" \
                    "$fixtures_dir/storage-baseline-required-true.json" 2>&1)"
    rc=$?
    set -e
    [ "$rc" = "6" ] || { printf '%s\n' "$out" >&2; fail "17. exit=$rc want=6"; }
    if ! printf '%s' "$out" | grep -q "ADR 0002 §5"; then
        printf '%s\n' "$out" >&2
        fail "17. expected ADR 0002 §5 in remediation"
    fi
    if ! printf '%s' "$out" | grep -q "redb"; then
        printf '%s\n' "$out" >&2
        fail "17. expected dep name 'redb' in diagnostic"
    fi
)
pass "17. storage-dep over tolerance per-category -> exit 6 with ADR refresh"

# 18. category-trade attempt — redb.exprs −11 (unused) + redb.item_impls
# +11. Aggregate sum unchanged but per-category +10 budget exceeded.
# Exit 6.
(
    default_env
    set +e
    out="$("$check" "$fixtures_dir/storage-trade-redb.json" \
                    "$fixtures_dir/storage-baseline-required-true.json" 2>&1)"
    rc=$?
    set -e
    [ "$rc" = "6" ] || { printf '%s\n' "$out" >&2; fail "18. exit=$rc want=6"; }
    if ! printf '%s' "$out" | grep -q "item_impls"; then
        printf '%s\n' "$out" >&2
        fail "18. expected item_impls in diagnostic"
    fi
)
pass "18. storage-dep category trade (per-category +10 enforcement) -> exit 6"

# 19. version drift — same Registry source, version 4.2.0 instead
# of pinned 4.1.0. Source enum doesn't carry version, so per-dep
# match falls through to the version-drift branch, exit 7.
(
    default_env
    set +e
    out="$("$check" "$fixtures_dir/storage-version-drift.json" \
                    "$fixtures_dir/storage-baseline-required-true.json" 2>&1)"
    rc=$?
    set -e
    [ "$rc" = "7" ] || { printf '%s\n' "$out" >&2; fail "19. exit=$rc want=7"; }
    if ! printf '%s' "$out" | grep -qi "version"; then
        printf '%s\n' "$out" >&2
        fail "19. expected 'version' in version-drift diagnostic"
    fi
)
pass "19. storage-dep version drift -> exit 7 (re-pin needed)"

# 20. dep absent from scan, present in Cargo.toml — runs from
# repo root where redb IS a transitive dep. Checker's B4
# disambiguation must hit the "feature-unification" branch.
(
    default_env
    set +e
    out="$("$check" "$fixtures_dir/storage-missing-redb.json" \
                    "$fixtures_dir/storage-baseline-required-true.json" 2>&1)"
    rc=$?
    set -e
    [ "$rc" = "7" ] || { printf '%s\n' "$out" >&2; fail "20. exit=$rc want=7"; }
    if ! printf '%s' "$out" | grep -qi "feature-unification"; then
        printf '%s\n' "$out" >&2
        fail "20. expected feature-unification message"
    fi
)
pass "20. dep absent from scan, present in Cargo.toml -> exit 7 (feature-unification)"

# 21. dep absent from BOTH scan and Cargo.toml. Run inside a
# throwaway crate that has no redb dep. Checker's B4 must hit
# the "removed from Cargo.toml" branch.
(
    cd "$tmp"
    mkdir -p repo-21/src
    cd repo-21
    cat > Cargo.toml <<'EOF'
[package]
name = "geiger-test-throwaway"
version = "0.0.0"
edition = "2021"

[lib]
path = "src/lib.rs"

[dependencies]
EOF
    : > src/lib.rs

    default_env
    set +e
    out="$("$check" "$fixtures_dir/storage-missing-redb.json" \
                    "$fixtures_dir/storage-baseline-required-true.json" 2>&1)"
    rc=$?
    set -e
    [ "$rc" = "7" ] || { printf '%s\n' "$out" >&2; fail "21. exit=$rc want=7"; }
    if ! printf '%s' "$out" | grep -qi "removed from Cargo.toml"; then
        printf '%s\n' "$out" >&2
        fail "21. expected 'removed from Cargo.toml' message"
    fi
)
pass "21. dep absent from both scan and Cargo.toml -> exit 7 (dep removed)"

# 22. workspace-gate interaction — full throwaway-repo setup. The
# PR is label-approved AND bumps the workspace baseline matching
# the scan's workspace totals (so the workspace gate would pass
# alone), but redb grew +11 in the same scan. The storage-dep
# block runs BEFORE the workspace gate so the storage failure
# (exit 6) takes precedence over the would-be workspace PASS.
(
    cd "$tmp"
    git init --quiet repo-22
    cd repo-22
    git config user.email t@t && git config user.name t

    # base commit: workspace baseline at exprs=4 with required:true
    # storage section matching the scan's pinned source.
    cp "$fixtures_dir/storage-baseline-required-true.json" unsafe-baseline.json
    git add -A
    git commit --quiet -m base
    git branch -M main
    git checkout --quiet -b feature

    # head commit: bump workspace baseline to exprs=5 (matches
    # storage-grown-redb.json's mango-loom-demo workspace total)
    # AND keep redb totals at 30. Scan reports redb.exprs=41.
    jq '.totals.exprs                     = 5
       | .crates["mango-loom-demo"].exprs = 5' \
        "$fixtures_dir/storage-baseline-required-true.json" \
        > unsafe-baseline.json
    git add -A
    git commit --quiet -m bump-workspace-only

    git update-ref refs/remotes/origin/main refs/heads/main

    # Synthesize a scan with workspace exprs=5 (matches new
    # workspace baseline) AND redb.exprs=41 (over storage budget).
    scan="$tmp/scan-22.json"
    jq '.packages[0].unsafety.used.exprs.unsafe_ = 5' \
        "$fixtures_dir/storage-grown-redb.json" \
        > "$scan"

    export GITHUB_EVENT_NAME="pull_request"
    export GITHUB_BASE_REF="main"
    export PR_LABELS='["unsafe-growth-approved"]'
    set +e
    out="$("$check" "$scan" unsafe-baseline.json 2>&1)"
    rc=$?
    set -e
    [ "$rc" = "6" ] || { printf '%s\n' "$out" >&2; fail "22. exit=$rc want=6"; }
    if printf '%s' "$out" | grep -q "unsafe growth approved by label"; then
        printf '%s\n' "$out" >&2
        fail "22. workspace gate ran before storage gate (storage failure should preempt)"
    fi
)
pass "22. workspace-gate interaction: storage failure preempts label-approved workspace bump -> exit 6"

# 23. required: true but a required dep entry missing from
# storage_deps. S2 schema bypass-prevention, exit 3.
(
    default_env
    set +e
    out="$("$check" "$fixtures_dir/storage-equal.json" \
                    "$fixtures_dir/storage-baseline-missing-redb.json" 2>&1)"
    rc=$?
    set -e
    [ "$rc" = "3" ] || { printf '%s\n' "$out" >&2; fail "23. exit=$rc want=3"; }
    if ! printf '%s' "$out" | grep -qi "schema"; then
        printf '%s\n' "$out" >&2
        fail "23. expected 'schema' in error"
    fi
)
pass "23. required:true + missing required dep -> exit 3 (schema)"

# 23b. tolerance field rejection — B3. Per-dep tolerance overrides
# would let a maintainer slowly weaken the gate. Hardcoded +10 is
# the policy.
(
    default_env
    set +e
    out="$("$check" "$fixtures_dir/storage-equal.json" \
                    "$fixtures_dir/storage-baseline-tolerance.json" 2>&1)"
    rc=$?
    set -e
    [ "$rc" = "3" ] || { printf '%s\n' "$out" >&2; fail "23b. exit=$rc want=3"; }
    if ! printf '%s' "$out" | grep -qi "tolerance"; then
        printf '%s\n' "$out" >&2
        fail "23b. expected 'tolerance' rejection in error"
    fi
)
pass "23b. per-dep tolerance field rejected -> exit 3"

# stranger detector — same name but Path-rerouted source. Exit 7.
(
    default_env
    set +e
    out="$("$check" "$fixtures_dir/storage-stranger.json" \
                    "$fixtures_dir/storage-baseline-required-true.json" 2>&1)"
    rc=$?
    set -e
    [ "$rc" = "7" ] || { printf '%s\n' "$out" >&2; fail "stranger. exit=$rc want=7"; }
    if ! printf '%s' "$out" | grep -qi "unexpected storage-dep sources"; then
        printf '%s\n' "$out" >&2
        fail "stranger. expected stranger-detector banner"
    fi
)
pass "stranger detector: same name, rerouted source -> exit 7"

# 14b. updater round-trip with storage_deps. Synthesize a merged
# scan, run the updater (test-hatch mode), check the resulting
# baseline preserves required:true + source/version pins and
# refreshes totals. Then mutate the scan's redb version, re-run
# the updater in default mode -> exit 5. REPIN mode accepts.
(
    real_baseline="$repo_root/unsafe-baseline.json"
    stash="$tmp/baseline-stash-14b.json"
    cp "$real_baseline" "$stash"

    # Substrate: a required:true baseline at the canonical pins.
    cp "$fixtures_dir/storage-baseline-required-true.json" "$real_baseline"

    # First pass: storage scan at canonical sources, redb.exprs=33
    # (within +10 budget, baseline pins 30). Updater should refresh
    # totals to 33 in default mode.
    merged="$tmp/14b-merged.json"
    jq '.' "$fixtures_dir/storage-equal.json" \
        | jq '
            ( .packages[]
              | select(.package.id.name == "redb")
              | .unsafety.used.exprs.unsafe_ ) = 33
            | .' \
        > "$merged"

    GEIGER_FROM_MERGED_JSON="$merged" \
    GEIGER_VERSION_OVERRIDE="0.13.0" \
        bash "$updater" >/dev/null

    written_redb="$(jq '.storage_deps.redb.totals.exprs' "$real_baseline")"
    written_raft="$(jq '.storage_deps["raft-engine"].totals.exprs' "$real_baseline")"
    written_required="$(jq '.storage_deps_required' "$real_baseline")"
    if [ "$written_redb" != "33" ]; then
        cp "$stash" "$real_baseline"
        fail "14b. updater wrote redb.exprs=$written_redb want 33"
    fi
    if [ "$written_raft" != "40" ]; then
        cp "$stash" "$real_baseline"
        fail "14b. updater wrote raft-engine.exprs=$written_raft want 40"
    fi
    if [ "$written_required" != "true" ]; then
        cp "$stash" "$real_baseline"
        fail "14b. updater dropped storage_deps_required (got $written_required)"
    fi

    # Second pass: mutate the merged scan's redb to a different
    # Registry version. Default-mode updater must exit 5 because
    # the (name, source) match returns the bumped version, not
    # the pinned 4.1.0. Update both source-equality and version
    # to actually drift the (name,source) match — change Registry
    # url to a synthetic alternate.
    drifted="$tmp/14b-drifted.json"
    jq '
        ( .packages[]
          | select(.package.id.name == "redb")
          | .package.id.source ) = {"Registry": {"name": "alt-index", "url": "https://example.test/alt"}}
        | .' "$merged" > "$drifted"

    set +e
    GEIGER_FROM_MERGED_JSON="$drifted" \
    GEIGER_VERSION_OVERRIDE="0.13.0" \
        bash "$updater" >/dev/null 2>&1
    rc=$?
    set -e
    if [ "$rc" != "5" ]; then
        cp "$stash" "$real_baseline"
        fail "14b. default-mode updater on source-mismatch: exit=$rc want=5"
    fi

    # Third pass: REPIN mode accepts, rewrites source + version.
    MANGO_GEIGER_REPIN=1 \
    GEIGER_FROM_MERGED_JSON="$drifted" \
    GEIGER_VERSION_OVERRIDE="0.13.0" \
        bash "$updater" >/dev/null

    written_src_name="$(jq -r '.storage_deps.redb.source.Registry.name' "$real_baseline")"
    if [ "$written_src_name" != "alt-index" ]; then
        cp "$stash" "$real_baseline"
        fail "14b. REPIN-mode updater did not rewrite source (got name=$written_src_name)"
    fi

    cp "$stash" "$real_baseline"
)
pass "14b. updater <-> checker round trip with storage_deps + REPIN-mode source rewrite"

# 24. nondeterminism re-pin — baseline pins redb.exprs=29, scan
# reports 30 (in-tolerance flake within the +10 budget). Default-
# mode updater accepts and re-anchors totals to 30; no ADR refresh
# required. Documents the recovery procedure for cargo-geiger
# nondeterminism (M7).
(
    real_baseline="$repo_root/unsafe-baseline.json"
    stash="$tmp/baseline-stash-24.json"
    cp "$real_baseline" "$stash"

    cp "$fixtures_dir/storage-baseline-required-true-flake.json" "$real_baseline"

    # Checker before re-anchor: scan reports redb.exprs=30,
    # baseline.redb.exprs=29 -> within +10 -> exit 0.
    default_env
    out="$("$check" "$fixtures_dir/storage-equal.json" "$real_baseline" 2>&1)" || {
        cp "$stash" "$real_baseline"
        fail "24. checker rejected within-tolerance flake"
    }

    # Now re-anchor via default-mode updater. Source matches, so
    # no exit 5; totals refresh to 30.
    GEIGER_FROM_MERGED_JSON="$fixtures_dir/storage-equal.json" \
    GEIGER_VERSION_OVERRIDE="0.13.0" \
        bash "$updater" >/dev/null

    re_anchored="$(jq '.storage_deps.redb.totals.exprs' "$real_baseline")"
    if [ "$re_anchored" != "30" ]; then
        cp "$stash" "$real_baseline"
        fail "24. re-anchor failed: redb.exprs=$re_anchored want 30"
    fi

    cp "$stash" "$real_baseline"
)
pass "24. nondeterminism flake re-pin: default-mode updater re-anchors within tolerance"

echo "ok: all geiger script tests passed"
