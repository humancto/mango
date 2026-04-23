#!/usr/bin/env bash
# scripts/sbom-check.sh
#
# Validates a single CycloneDX JSON SBOM emitted by
# `cargo cyclonedx` against the contract documented in
# docs/sbom-policy.md.
#
# Usage:
#   bash scripts/sbom-check.sh <sbom.json> <expected-root-crate-name>
#
# Exit codes:
#   0  SBOM satisfies every per-file assertion.
#   2  Usage error (missing args, unreadable file).
#   3  Contract violation.
#
# Integration-level assertions (exact file count, Cargo.lock
# cross-check, reproducibility diff) are NOT this script's
# concern — they run inline in `.github/workflows/sbom.yml`
# because they span multiple SBOMs and cargo state.
#
# Requires: bash, jq. Intentionally dependency-light so the
# self-test under scripts/sbom-scripts-test.sh runs with no
# cargo in the loop.
set -euo pipefail

usage() {
    cat >&2 <<EOF
usage: $0 <sbom.json> <expected-root-crate-name>
EOF
    exit 2
}

if [ $# -lt 2 ]; then
    usage
fi

sbom="$1"
expected_name="$2"

if [ ! -r "$sbom" ]; then
    echo "error: cannot read SBOM file: $sbom" >&2
    exit 2
fi

# Tool version pin the SBOM should claim it was produced by. The
# workflow exports this via CARGO_CYCLONEDX_VERSION; the self-test
# sets it explicitly per fixture. Absence is not an error here —
# the validator only enforces equality WHEN the caller specifies
# an expected version, because this script is shared between the
# CI workflow (which has a pin) and the self-test (which hits
# fixtures regenerated from the pin). If neither provides one, we
# skip the provenance assertion.
expected_tool_version="${EXPECTED_TOOL_VERSION:-}"

command -v jq >/dev/null 2>&1 || { echo "error: jq required" >&2; exit 2; }

fail() {
    printf 'sbom-check: FAIL (%s): %s\n' "$sbom" "$1" >&2
    exit 3
}

# Assertion 1: JSON parseable. `jq -e .` returns non-zero on a
# parse failure OR on a `false` / `null` top-level value; an SBOM
# is always an object, so the nuance doesn't bite us here.
if ! jq -e . "$sbom" >/dev/null 2>&1; then
    fail "not valid JSON"
fi

# Assertion 2: bomFormat == "CycloneDX".
bom_format="$(jq -r '.bomFormat // empty' "$sbom")"
if [ "$bom_format" != "CycloneDX" ]; then
    fail "bomFormat != CycloneDX (got \"$bom_format\")"
fi

# Assertion 3: specVersion == "1.5".
spec_version="$(jq -r '.specVersion // empty' "$sbom")"
if [ "$spec_version" != "1.5" ]; then
    fail "specVersion != 1.5 (got \"$spec_version\") — default is 1.3, flag is load-bearing"
fi

# Assertion 4: metadata.component.name matches the caller's
# expected root crate name. This is the single assertion that
# binds a file to a workspace member — everything else is shape.
component_name="$(jq -r '.metadata.component.name // empty' "$sbom")"
if [ "$component_name" != "$expected_name" ]; then
    fail "metadata.component.name != $expected_name (got \"$component_name\")"
fi

# Assertion 5: metadata.tools[] contains cargo-cyclonedx with the
# expected pinned version. Skipped if EXPECTED_TOOL_VERSION is not
# set by the caller.
if [ -n "$expected_tool_version" ]; then
    # cargo-cyclonedx 0.5.9 emits:
    #   metadata.tools = [{vendor, name, version}, ...]
    # In CycloneDX 1.5 this schema is deprecated in favor of
    # `metadata.tools.components[]`, but the pinned tool uses the
    # legacy array form. If a future bump flips this, the assertion
    # will fail loudly rather than silently pass.
    tool_version="$(
        jq -r --arg n "cargo-cyclonedx" '
          .metadata.tools
          | if type == "array" then
              (map(select(.name == $n)) | first // empty).version // empty
            elif type == "object" then
              (.components // [] | map(select(.name == $n)) | first // empty).version // empty
            else empty end
        ' "$sbom"
    )"
    if [ "$tool_version" != "$expected_tool_version" ]; then
        fail "metadata.tools[cargo-cyclonedx].version != $expected_tool_version (got \"$tool_version\")"
    fi
fi

# Assertion 6: volatile-field shapes.
#   - metadata.timestamp is a string (ISO-8601 when not overridden;
#     "1970-01-01T00:00:01..." when SOURCE_DATE_EPOCH=1).
#   - serialNumber is either a "urn:uuid:<36-char>" string OR null
#     (deterministic run under SOURCE_DATE_EPOCH=1 emits null).
ts_type="$(jq -r '.metadata.timestamp | type' "$sbom")"
if [ "$ts_type" != "string" ]; then
    fail "metadata.timestamp is not a string (type=$ts_type)"
fi

serial_type="$(jq -r '.serialNumber | type' "$sbom")"
case "$serial_type" in
    "null")
        # Deterministic run under SOURCE_DATE_EPOCH=1. Accepted.
        ;;
    "string")
        serial="$(jq -r '.serialNumber' "$sbom")"
        # urn:uuid: prefix + 36-char canonical UUID.
        if ! printf '%s' "$serial" \
                | grep -Eq '^urn:uuid:[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'; then
            fail "serialNumber malformed: \"$serial\""
        fi
        ;;
    *)
        fail "serialNumber has unexpected type $serial_type"
        ;;
esac

printf 'sbom-check: OK (%s, root=%s)\n' "$sbom" "$expected_name"
