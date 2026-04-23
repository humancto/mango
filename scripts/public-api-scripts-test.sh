#!/usr/bin/env bash
# scripts/public-api-scripts-test.sh
#
# Smoke-test harness for the cargo-public-api CI gate.
#
# What this covers:
#   - .github/workflows/public-api.yml has the structural invariants
#     the CI gate depends on (version pins, fetch-depth, sentinel
#     tri-consistency with continue-on-error and --deny, loud-warning
#     step, path filters, dual-toolchain install).
#   - docs/public-api-policy.md exists (workflow comments link to it;
#     broken links turn a runtime failure into a tooling-trust
#     failure).
#   - The publishable-member jq filter works against a synthetic
#     cargo-metadata fixture.
#
# Workflow-absent behavior:
#   This script is committed BEFORE the workflow file lands (to keep
#   commits atomic). Workflow-shape assertions print SKIP when the
#   workflow file is absent. Non-workflow assertions (jq filter
#   fixture, policy doc existence) run unconditionally. Once the
#   workflow lands, the self-test runs as a step inside the workflow
#   where the file is always present, so SKIP is no longer reachable
#   and the assertions are as strong as semver-scripts-test.sh's.
#
# What this does NOT cover:
#   - Running `cargo public-api` itself. That runs as a dedicated
#     workflow step after this harness — duplicating it here only
#     doubles CI wall-clock.
#   - The tool's own diff logic. cargo-public-api has its own
#     extensive test suite upstream; we gain nothing by reimplementing.
#
# Invocation:
#   bash scripts/public-api-scripts-test.sh
#   (run from any CWD; script cd's to repo root)

set -u

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

pass_count=0
fail_count=0
skip_count=0

pass() {
    printf 'PASS  %s\n' "$1"
    pass_count=$((pass_count + 1))
}

fail() {
    printf 'FAIL  %s\n' "$1" >&2
    fail_count=$((fail_count + 1))
}

skip() {
    printf 'SKIP  %s\n' "$1"
    skip_count=$((skip_count + 1))
}

workflow=".github/workflows/public-api.yml"
policy_doc="docs/public-api-policy.md"

# ---------------------------------------------------------------------
# workflow presence — sbom-style skip pattern (graceful absence).
# Once the workflow lands in commit 3, this branch is unreachable.
# ---------------------------------------------------------------------
workflow_present=0
scenario="workflow file exists"
if [ -f "$workflow" ]; then
    pass "$scenario"
    workflow_present=1
else
    skip "$scenario (will assert once commit 3 lands)"
fi

# Normalize whitespace for pattern-based assertions below.
# Only meaningful when workflow_present=1; guard each block.
if [ "$workflow_present" = "1" ]; then
    normalized="$(tr -s ' \t' ' ' < "$workflow")"
fi

# ---------------------------------------------------------------------
# CARGO_PUBLIC_API_VERSION: parseable as x.y.z
# ---------------------------------------------------------------------
scenario="CARGO_PUBLIC_API_VERSION is parseable as x.y.z"
if [ "$workflow_present" = "1" ]; then
    if grep -qE '^\s*CARGO_PUBLIC_API_VERSION:\s*"[0-9]+\.[0-9]+\.[0-9]+"' "$workflow"; then
        pass "$scenario"
    else
        fail "$scenario: CARGO_PUBLIC_API_VERSION line not found or malformed in $workflow"
    fi
else
    skip "$scenario"
fi

# ---------------------------------------------------------------------
# PUBLIC_API_NIGHTLY: parseable as nightly-YYYY-MM-DD
# Dual-pin coupling: tool version and nightly toolchain must move
# together. Upstream bumps the minimum-supported nightly periodically;
# bumping the tool without the nightly produces rustdoc-JSON-format
# errors that look unrelated. The policy doc's bump procedure mandates
# updating both; this assertion keeps the pin well-formed.
# ---------------------------------------------------------------------
scenario="PUBLIC_API_NIGHTLY is parseable as nightly-YYYY-MM-DD"
if [ "$workflow_present" = "1" ]; then
    if grep -qE '^\s*PUBLIC_API_NIGHTLY:\s*"nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}"' "$workflow"; then
        pass "$scenario"
    else
        fail "$scenario: PUBLIC_API_NIGHTLY line not found or malformed in $workflow"
    fi
else
    skip "$scenario"
fi

# ---------------------------------------------------------------------
# fetch-depth on checkout
# ---------------------------------------------------------------------
# `diff BASE..HEAD` needs the base commit present locally.
# actions/checkout defaults to depth 1.
scenario="checkout step sets fetch-depth: 0"
if [ "$workflow_present" = "1" ]; then
    if grep -qE '^\s*fetch-depth:\s*0\s*$' "$workflow"; then
        pass "$scenario"
    else
        fail "$scenario: fetch-depth: 0 not found in $workflow"
    fi
else
    skip "$scenario"
fi

# ---------------------------------------------------------------------
# PUBLIC-API-MODE sentinel present
# ---------------------------------------------------------------------
mode=""
scenario="PUBLIC-API-MODE sentinel is present"
if [ "$workflow_present" = "1" ]; then
    mode_line="$(printf '%s\n' "$normalized" | grep -E '# PUBLIC-API-MODE: (advisory|gating)' | head -n 1 || true)"
    if [ -n "$mode_line" ]; then
        pass "$scenario"
        mode="$(printf '%s' "$mode_line" | sed -E 's/.*PUBLIC-API-MODE: ([a-z]+).*/\1/')"
    else
        fail "$scenario: '# PUBLIC-API-MODE: advisory|gating' not found in $workflow"
    fi
else
    skip "$scenario"
fi

# ---------------------------------------------------------------------
# Tri-consistency: sentinel ↔ continue-on-error ↔ --deny
#
# --deny all is MANDATORY in both advisory and gating modes. Without
# it, `cargo public-api diff` returns 0 on a non-empty diff and the
# continue-on-error / ::warning:: wiring becomes a silent no-op. The
# Phase-6 flip is sentinel + continue-on-error (two lines); --deny
# stays in place across the flip.
#
# - sentinel=advisory → continue-on-error: true AND --deny present
# - sentinel=gating   → continue-on-error: false AND --deny present
# ---------------------------------------------------------------------
scenario="tri-consistency: sentinel ↔ continue-on-error ↔ --deny"
if [ "$workflow_present" = "1" ] && [ -n "$mode" ]; then
    coe_line="$(printf '%s\n' "$normalized" | grep -E '^ *continue-on-error: (true|false)' | head -n 1 || true)"
    has_deny=0
    # Match --deny followed by a DenyMethod variant on a non-comment
    # line only. The workflow has a header comment mentioning --deny,
    # and a naive `grep '--deny'` would match it — masking a real
    # regression where the invocation itself drops the flag.
    if grep -v '^[[:space:]]*#' "$workflow" \
        | grep -qE -- '--deny[[:space:]]+(all|added|changed|removed)'; then
        has_deny=1
    fi

    coe_ok=0
    if [ "$mode" = "advisory" ] && printf '%s' "$coe_line" | grep -q 'true'; then
        coe_ok=1
    elif [ "$mode" = "gating" ] && printf '%s' "$coe_line" | grep -q 'false'; then
        coe_ok=1
    fi

    if [ -z "$coe_line" ]; then
        fail "$scenario: no continue-on-error line found"
    elif [ "$coe_ok" != "1" ]; then
        fail "$scenario: mode=$mode but continue-on-error line was: $coe_line"
    elif [ "$has_deny" != "1" ]; then
        fail "$scenario: --deny flag missing from workflow (exit-code semantics require it in BOTH modes)"
    else
        pass "$scenario (mode=$mode, continue-on-error ok, --deny present)"
    fi
else
    skip "$scenario"
fi

# ---------------------------------------------------------------------
# Loud-warning step: advisory mode without a visible signal is worse
# than no gate. The workflow must emit a ::warning:: annotation on
# check failure.
# ---------------------------------------------------------------------
scenario="advisory failure is surfaced as ::warning:: annotation"
if [ "$workflow_present" = "1" ]; then
    if grep -qF '::warning title=public-api advisory::' "$workflow"; then
        pass "$scenario"
    else
        fail "$scenario: no ::warning title=public-api advisory:: line in $workflow"
    fi
else
    skip "$scenario"
fi

# ---------------------------------------------------------------------
# Path filters: load-bearing paths on pull_request.
# ---------------------------------------------------------------------
scenario="pull_request paths include the load-bearing entries"
if [ "$workflow_present" = "1" ]; then
    # Disable shell globbing around the loop so ** patterns match as
    # literal strings, not against the local filesystem.
    set -f
    missing=""
    for p in 'crates/**/src/**' 'crates/**/Cargo.toml' 'Cargo.toml' 'Cargo.lock'; do
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
else
    skip "$scenario"
fi

# ---------------------------------------------------------------------
# Dual-toolchain install: stable active + nightly installed.
# `cargo-public-api` consumes nightly rustdoc JSON but stable must
# be active (the rest of CI assumes stable). Two dtolnay/rust-toolchain
# uses: blocks, one per channel.
# ---------------------------------------------------------------------
scenario="dual toolchain install present (stable + nightly)"
if [ "$workflow_present" = "1" ]; then
    # Count distinct dtolnay/rust-toolchain uses: blocks.
    uses_count="$(grep -cE '^\s*-\s*uses:\s*dtolnay/rust-toolchain' "$workflow" || true)"
    if [ "${uses_count:-0}" -ge 2 ]; then
        # And both `toolchain:` values must appear: stable and the
        # PUBLIC_API_NIGHTLY env reference.
        has_stable=0
        has_nightly=0
        if grep -qE '^\s*toolchain:\s*stable\s*$' "$workflow"; then
            has_stable=1
        fi
        if grep -qE 'toolchain:\s*\$\{\{\s*env\.PUBLIC_API_NIGHTLY\s*\}\}' "$workflow"; then
            has_nightly=1
        fi
        if [ "$has_stable" = "1" ] && [ "$has_nightly" = "1" ]; then
            pass "$scenario"
        else
            fail "$scenario: 2+ toolchain uses: blocks but stable=$has_stable, nightly=$has_nightly"
        fi
    else
        fail "$scenario: expected 2+ dtolnay/rust-toolchain uses: blocks, got ${uses_count:-0}"
    fi
else
    skip "$scenario"
fi

# ---------------------------------------------------------------------
# Publishable-member jq filter — unit test against synthetic fixture.
# Runs unconditionally (no workflow dependency): this guards the jq
# filter documented in docs/public-api-policy.md and embedded in the
# workflow. If the filter ever drifts, new publishable crates would
# be silently skipped (or publish=false crates included).
# ---------------------------------------------------------------------
scenario="publishable-member jq filter against synthetic fixture"
if ! command -v jq >/dev/null 2>&1; then
    fail "$scenario: jq not found on PATH"
else
    # Three packages:
    #   a: publish=null         (default, publishable)      → INCLUDE
    #   b: publish=[]           (publish = false)           → EXCLUDE
    #   c: publish=["crates-io"] (registry allow-list)      → INCLUDE
    # The `source` filter excludes dependencies pulled from registries
    # (source != null for those; source == null for workspace members).
    got="$(printf '%s' '{"packages":[
        {"publish":null,"source":null,"name":"a"},
        {"publish":[],"source":null,"name":"b"},
        {"publish":["crates-io"],"source":null,"name":"c"},
        {"publish":null,"source":"registry+https://github.com/rust-lang/crates.io-index","name":"d"}
    ]}' \
        | jq -r '.packages[]
                 | select(.source == null)
                 | select(.publish != [])
                 | .name' \
        | tr '\n' ' ' | sed 's/ *$//')"
    want="a c"
    if [ "$got" = "$want" ]; then
        pass "$scenario (got: $got)"
    else
        fail "$scenario: want '$want', got '$got'"
    fi
fi

# ---------------------------------------------------------------------
# Policy doc presence — workflow comments link to it.
# Runs unconditionally.
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
printf '\n%d passed, %d failed, %d skipped\n' "$pass_count" "$fail_count" "$skip_count"
if [ "$fail_count" -ne 0 ]; then
    exit 1
fi
