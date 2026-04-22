#!/usr/bin/env bash
# scripts/test-msrv-pin.sh
#
# Asserts the MSRV pin in .github/workflows/ci.yml matches the
# rust-version declared in Cargo.toml (workspace-wide, via
# `cargo metadata` so workspace inheritance is resolved).
#
# Runs in CI as a step of the `msrv` job and is runnable locally by
# a contributor who bumps either source of truth.
#
# Requires: bash, cargo, jq, yq.
set -euo pipefail

workflow=".github/workflows/ci.yml"

# Extract the toolchain pinned in the msrv job via its
# dtolnay/rust-toolchain step.
workflow_msrv=$(yq -r \
    '.jobs.msrv.steps[] | select(.uses? | test("dtolnay/rust-toolchain")) | .with.toolchain' \
    "$workflow")
if [ -z "$workflow_msrv" ] || [ "$workflow_msrv" = "null" ]; then
    echo "error: could not extract msrv job toolchain from $workflow" >&2
    exit 1
fi

# Every workspace package's rust_version, deduplicated.
# `cargo metadata --no-deps` + workspace inheritance resolution
# gives us the real per-package MSRV without TOML parsing.
manifest_msrvs=$(cargo metadata --format-version=1 --no-deps \
    | jq -r '.packages[].rust_version' \
    | grep -v '^null$' \
    | sort -u)

if [ -z "$manifest_msrvs" ]; then
    echo "error: no rust-version declared in any workspace package" >&2
    exit 1
fi

count=$(printf '%s\n' "$manifest_msrvs" | wc -l | tr -d ' ')
if [ "$count" != "1" ]; then
    echo "error: inconsistent rust-version across workspace packages:" >&2
    printf '%s\n' "$manifest_msrvs" >&2
    exit 1
fi

if [ "$workflow_msrv" != "$manifest_msrvs" ]; then
    echo "error: msrv drift detected" >&2
    echo "  $workflow msrv job toolchain: '$workflow_msrv'" >&2
    echo "  Cargo.toml rust-version:      '$manifest_msrvs'" >&2
    echo "Bump both deliberately and rerun." >&2
    exit 1
fi

echo "ok: MSRV pin matches ($workflow_msrv)"
