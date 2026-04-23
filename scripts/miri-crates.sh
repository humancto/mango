#!/usr/bin/env bash
# scripts/miri-crates.sh
#
# Emits the Miri curated subset — one crate name per line — as
# declared in the workspace manifest:
#
#   [workspace.metadata.mango.miri]
#   crates = ["mango-loom-demo", ...]
#
# This is the single source of truth consumed by
# `.github/workflows/miri.yml` (full subset job) and by
# `scripts/miri-changed-crates.sh` (PR job). See docs/miri.md.
#
# Requires: bash, cargo, jq.
set -euo pipefail

command -v jq >/dev/null 2>&1 || {
    echo "error: jq not found on PATH (required to parse cargo metadata)" >&2
    exit 2
}

cargo metadata --format-version=1 --no-deps \
    | jq -r '.metadata.mango.miri.crates // [] | .[]'
