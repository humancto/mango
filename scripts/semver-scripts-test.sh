#!/usr/bin/env bash
# scripts/semver-scripts-test.sh
#
# Smoke-test harness for the cargo-semver-checks CI gate.
#
# What this covers:
#   - .github/workflows/semver-checks.yml exists and has the
#     structural invariants the CI gate depends on (version pin
#     format, fetch-depth, sentinel + continue-on-error consistency,
#     loud-warning step, path filters).
#   - docs/semver-policy.md exists (workflow comments link to it;
#     broken links turn a runtime failure into a tooling-trust
#     failure).
#
# What this does NOT cover:
#   - Running `cargo semver-checks` itself. That runs as a dedicated
#     workflow step after this harness — duplicating it here only
#     doubles CI wall-clock.
#   - The tool's own lint logic. cargo-semver-checks has its own
#     extensive test suite upstream; we gain nothing by reimplementing.
#
# Invocation:
#   bash scripts/semver-scripts-test.sh
#   (run from any CWD; script cd's to repo root)

set -u

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

pass_count=0
fail_count=0

pass() {
    printf 'PASS  %s\n' "$1"
    pass_count=$((pass_count + 1))
}

fail() {
    printf 'FAIL  %s\n' "$1" >&2
    fail_count=$((fail_count + 1))
}

workflow=".github/workflows/semver-checks.yml"
policy_doc="docs/semver-policy.md"

# ---------------------------------------------------------------------
# workflow presence
# ---------------------------------------------------------------------
scenario="workflow file exists"
if [ -f "$workflow" ]; then
    pass "$scenario"
else
    fail "$scenario: $workflow missing"
    printf '\n%d passed, %d failed\n' "$pass_count" "$fail_count"
    exit 1
fi

# ---------------------------------------------------------------------
# version pin parseability
# ---------------------------------------------------------------------
# The install step reads this env var verbatim. If the regex drifts,
# contributors would install a version different from the pin, and
# the `semver-checks version sanity` step in the workflow would catch
# it — but this harness fails FIRST, at commit time, with a clearer
# message.
scenario="CARGO_SEMVER_CHECKS_VERSION is parseable as x.y.z"
if grep -qE '^\s*CARGO_SEMVER_CHECKS_VERSION:\s*"[0-9]+\.[0-9]+\.[0-9]+"' "$workflow"; then
    pass "$scenario"
else
    fail "$scenario: CARGO_SEMVER_CHECKS_VERSION line not found or malformed in $workflow"
fi

# ---------------------------------------------------------------------
# fetch-depth on checkout
# ---------------------------------------------------------------------
# --baseline-rev <sha> needs the commit locally. actions/checkout
# defaults to depth 1. Missing fetch-depth: 0 produces runtime
# failures that look unrelated to the gate.
scenario="checkout step sets fetch-depth: 0"
if grep -qE '^\s*fetch-depth:\s*0\s*$' "$workflow"; then
    pass "$scenario"
else
    fail "$scenario: fetch-depth: 0 not found in $workflow"
fi

# ---------------------------------------------------------------------
# advisory/gating sentinel + continue-on-error consistency
# ---------------------------------------------------------------------
# Single-line sentinel `# SEMVER-CHECKS-MODE: <mode>` sits above the
# check step. Phase 6 flip must update BOTH the sentinel AND the
# continue-on-error flag, so this harness asserts they're in sync.
# If the sentinel says `advisory`, continue-on-error must be true.
# If it says `gating`, continue-on-error must be false.

# Normalize whitespace via tr -s so reflows / rebases don't break us.
normalized="$(tr -s ' \t' ' ' < "$workflow")"

mode_line="$(printf '%s\n' "$normalized" | grep -E '# SEMVER-CHECKS-MODE: (advisory|gating)' | head -n 1 || true)"
scenario="SEMVER-CHECKS-MODE sentinel is present"
if [ -n "$mode_line" ]; then
    pass "$scenario"

    mode="$(printf '%s' "$mode_line" | sed -E 's/.*SEMVER-CHECKS-MODE: ([a-z]+).*/\1/')"

    # Find the continue-on-error line that follows the sentinel
    # within the same step.
    coe_line="$(printf '%s\n' "$normalized" | grep -E '^ *continue-on-error: (true|false)' | head -n 1 || true)"
    scenario="continue-on-error matches sentinel (mode=$mode)"
    if [ -z "$coe_line" ]; then
        fail "$scenario: no continue-on-error line found"
    elif [ "$mode" = "advisory" ] && printf '%s' "$coe_line" | grep -q 'true'; then
        pass "$scenario"
    elif [ "$mode" = "gating" ] && printf '%s' "$coe_line" | grep -q 'false'; then
        pass "$scenario"
    else
        fail "$scenario: mode=$mode but continue-on-error line was: $coe_line"
    fi
else
    fail "$scenario: '# SEMVER-CHECKS-MODE: advisory|gating' not found in $workflow"
fi

# ---------------------------------------------------------------------
# loud-warning step: advisory mode without a visible signal is worse
# than no gate (classic "nobody reads the warnings" failure mode).
# The workflow must emit a ::warning:: annotation when the check fails.
# ---------------------------------------------------------------------
scenario="advisory failure is surfaced as ::warning:: annotation"
if grep -qF '::warning title=semver-checks advisory::' "$workflow"; then
    pass "$scenario"
else
    fail "$scenario: no ::warning title=semver-checks advisory:: line in $workflow"
fi

# ---------------------------------------------------------------------
# path filters: the load-bearing paths must be present on pull_request.
# Missing any of these means PRs that should trigger the gate would
# silently skip it.
# ---------------------------------------------------------------------
scenario="pull_request paths include the load-bearing entries"
# Disable shell globbing around the loop so ** patterns are matched
# as literal strings, not expanded against the local filesystem.
set -f
missing=""
for p in 'crates/**/src/**' 'crates/**/Cargo.toml' 'Cargo.toml' 'Cargo.lock'; do
    # Escape the ** / * literals for regex, then match either single-
    # or double-quoted YAML list form.
    pattern_escaped="${p//\*/\\*}"
    if ! grep -qE "^\s*-\s*[\"']${pattern_escaped}[\"']\s*$" "$workflow"; then
        missing="$missing $p"
    fi
done
set +f
if [ -z "$missing" ]; then
    pass "$scenario"
else
    fail "$scenario: missing path filter entries:$missing"
fi

# ---------------------------------------------------------------------
# bare subcommand — not the deprecated `check-release` alias.
# Modern cargo-semver-checks uses `cargo semver-checks` without a
# subcommand. The workflow must not embed the deprecated form.
# ---------------------------------------------------------------------
scenario="workflow uses bare 'cargo semver-checks' (not deprecated check-release)"
if grep -qE 'cargo semver-checks\s+check-release' "$workflow"; then
    fail "$scenario: deprecated 'cargo semver-checks check-release' still present"
else
    pass "$scenario"
fi

# ---------------------------------------------------------------------
# policy doc presence — workflow comments link to it.
# ---------------------------------------------------------------------
scenario="policy doc exists"
if [ -f "$policy_doc" ]; then
    pass "$scenario"
else
    fail "$scenario: $policy_doc missing"
fi

# ---------------------------------------------------------------------
# summary
# ---------------------------------------------------------------------
printf '\n%d passed, %d failed\n' "$pass_count" "$fail_count"
if [ "$fail_count" -ne 0 ]; then
    exit 1
fi
