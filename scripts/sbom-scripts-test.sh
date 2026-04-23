#!/usr/bin/env bash
# scripts/sbom-scripts-test.sh
#
# Self-test for scripts/sbom-check.sh. Runs the validator against
# every fixture under tests/fixtures/sbom/ and asserts each one
# lands on the expected exit code.
#
# Designed to run with no cargo in the loop — just bash + jq +
# the committed fixtures. CI invokes this as the first step of
# .github/workflows/sbom.yml so a broken validator fails fast,
# before we pay for a real SBOM generation.
#
# What is NOT covered here:
#   - The real `cargo cyclonedx` invocation. That's the workflow's
#     job — duplicating it locally requires the tool installed.
#   - The Cargo.lock cross-check + reproducibility diff. Those
#     are integration assertions that live in the workflow; they
#     span multiple SBOMs and require cargo state.
#
# Invocation:
#   bash scripts/sbom-scripts-test.sh
#   (run from any CWD; script cd's to repo root)
set -u

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

check="$repo_root/scripts/sbom-check.sh"
fixtures="$repo_root/tests/fixtures/sbom"

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

# run <fixture> <expected-name> <want-exit> <scenario-name>
# Optional trailing args are extra env settings for the invocation.
run() {
    local fixture="$1" expected_name="$2" want_exit="$3" name="$4"
    shift 4
    local out rc
    out="$(bash "$check" "$fixture" "$expected_name" 2>&1)"
    rc=$?
    if [ "$rc" -ne "$want_exit" ]; then
        printf 'FAIL  %s: want exit %d, got %d\n' "$name" "$want_exit" "$rc" >&2
        printf '----- output -----\n%s\n------------------\n' "$out" >&2
        fail_count=$((fail_count + 1))
        return 1
    fi
    pass "$name"
}

# --------------------------------------------------------------
# 1. Valid fixture passes (no tool-version pin required).
# --------------------------------------------------------------
run "$fixtures/valid.json" "mango-proto" 0 \
    "valid.json with expected name 'mango-proto' -> exit 0"

# --------------------------------------------------------------
# 2. Valid fixture fails if expected name mismatches.
#    This is the assertion-4 guard: a correctly-shaped SBOM is
#    still invalid if wired to the wrong workspace member.
# --------------------------------------------------------------
run "$fixtures/valid.json" "not-a-real-crate" 3 \
    "valid.json with wrong expected name -> exit 3"

# --------------------------------------------------------------
# 3. specVersion missing -> fail (assertion 3).
# --------------------------------------------------------------
run "$fixtures/invalid-missing-spec-version.json" "mango-proto" 3 \
    "invalid-missing-spec-version.json -> exit 3"

# --------------------------------------------------------------
# 4. specVersion == "1.3" -> fail (default-leak assertion 3).
# --------------------------------------------------------------
run "$fixtures/invalid-wrong-spec-version.json" "mango-proto" 3 \
    "invalid-wrong-spec-version.json -> exit 3"

# --------------------------------------------------------------
# 5. bomFormat == "SPDX" -> fail (assertion 2).
# --------------------------------------------------------------
run "$fixtures/invalid-wrong-format.json" "mango-proto" 3 \
    "invalid-wrong-format.json -> exit 3"

# --------------------------------------------------------------
# 6. Empty components is valid at the per-file layer. The
#    non-empty floor lives in the workflow (integration-level),
#    not sbom-check.sh. This assertion guards the separation: a
#    valid-shape-empty-components SBOM must NOT fail the per-file
#    validator.
# --------------------------------------------------------------
run "$fixtures/invalid-empty-components.json" "mango-proto" 0 \
    "invalid-empty-components.json -> exit 0 (per-file shape only)"

# --------------------------------------------------------------
# 7. Tool-version pin check: with EXPECTED_TOOL_VERSION set, the
#    validator enforces metadata.tools[].version. Fixture was
#    regenerated from cargo-cyclonedx 0.5.9, so matching pin
#    passes and a different pin must fail.
# --------------------------------------------------------------
scenario="valid.json with matching EXPECTED_TOOL_VERSION -> exit 0"
out="$(EXPECTED_TOOL_VERSION=0.5.9 bash "$check" "$fixtures/valid.json" "mango-proto" 2>&1)"
rc=$?
if [ "$rc" -eq 0 ]; then
    pass "$scenario"
else
    fail "$scenario (got exit $rc)"
    printf '%s\n' "$out" >&2
fi

scenario="valid.json with wrong EXPECTED_TOOL_VERSION -> exit 3"
out="$(EXPECTED_TOOL_VERSION=9.9.9 bash "$check" "$fixtures/valid.json" "mango-proto" 2>&1)"
rc=$?
if [ "$rc" -eq 3 ]; then
    pass "$scenario"
else
    fail "$scenario (got exit $rc)"
    printf '%s\n' "$out" >&2
fi

# --------------------------------------------------------------
# 8. Usage error: missing args -> exit 2.
# --------------------------------------------------------------
scenario="no args -> exit 2"
out="$(bash "$check" 2>&1)"
rc=$?
if [ "$rc" -eq 2 ]; then
    pass "$scenario"
else
    fail "$scenario (got exit $rc)"
    printf '%s\n' "$out" >&2
fi

# --------------------------------------------------------------
# 9. Usage error: missing file -> exit 2.
# --------------------------------------------------------------
scenario="missing file -> exit 2"
out="$(bash "$check" /nonexistent-sbom.json mango 2>&1)"
rc=$?
if [ "$rc" -eq 2 ]; then
    pass "$scenario"
else
    fail "$scenario (got exit $rc)"
    printf '%s\n' "$out" >&2
fi

# --------------------------------------------------------------
# 10. Garbage input (not JSON) -> exit 3.
# --------------------------------------------------------------
scenario="non-JSON file -> exit 3"
garbage="$(mktemp)"
trap 'rm -f "$garbage"' EXIT
printf 'this is not JSON\n' > "$garbage"
out="$(bash "$check" "$garbage" mango-proto 2>&1)"
rc=$?
if [ "$rc" -eq 3 ]; then
    pass "$scenario"
else
    fail "$scenario (got exit $rc)"
    printf '%s\n' "$out" >&2
fi

# --------------------------------------------------------------
# 11. fixture README exists + regenerator script is executable.
#     Contract: a contributor finds the fixtures and knows how
#     to refresh them.
# --------------------------------------------------------------
if [ -f "$fixtures/README.md" ]; then
    pass "fixtures/README.md exists"
else
    fail "fixtures/README.md missing"
fi

if [ -x "$repo_root/scripts/sbom-gen-fixtures.sh" ]; then
    pass "sbom-gen-fixtures.sh is executable"
else
    fail "sbom-gen-fixtures.sh not executable"
fi

# --------------------------------------------------------------
# 12. fixture tool-version matches sbom-check's current pin in
#     sbom-gen-fixtures.sh — guards against the fixture being
#     regenerated with a different tool than the workflow pins.
#     If the workflow file doesn't exist yet (initial PR), we
#     accept that and skip the assertion.
# --------------------------------------------------------------
scenario="fixture vs workflow pin agree"
workflow="$repo_root/.github/workflows/sbom.yml"
fixture_tool_version="$(
    jq -r '
      .metadata.tools
      | if type == "array" then
          (map(select(.name == "cargo-cyclonedx")) | first // empty).version // ""
        elif type == "object" then
          (.components // [] | map(select(.name == "cargo-cyclonedx")) | first // empty).version // ""
        else "" end
    ' "$fixtures/valid.json"
)"
if [ -f "$workflow" ]; then
    workflow_pin="$(
        awk -F'"' '/CARGO_CYCLONEDX_VERSION:/ {print $2; exit}' "$workflow"
    )"
    if [ -z "$workflow_pin" ]; then
        fail "$scenario: could not parse CARGO_CYCLONEDX_VERSION from workflow"
    elif [ "$fixture_tool_version" != "$workflow_pin" ]; then
        fail "$scenario: fixture=$fixture_tool_version, workflow=$workflow_pin"
    else
        pass "$scenario (both $workflow_pin)"
    fi
else
    printf 'SKIP  %s (workflow file not yet present)\n' "$scenario"
fi

# --------------------------------------------------------------
# summary
# --------------------------------------------------------------
printf '\n%d passed, %d failed\n' "$pass_count" "$fail_count"
if [ "$fail_count" -ne 0 ]; then
    exit 1
fi
