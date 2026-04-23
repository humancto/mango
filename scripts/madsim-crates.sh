#!/usr/bin/env bash
# scripts/madsim-crates.sh
#
# Emits the madsim curated subset — one crate name per line — as
# declared in the workspace manifest:
#
#   [workspace.metadata.mango.madsim]
#   crates = ["mango-madsim-demo", ...]
#
# Consumed by `.github/workflows/madsim.yml` to build the
# `-p <crate>` list passed to `cargo nextest run` under
# RUSTFLAGS="--cfg madsim". See docs/madsim.md.
#
# Fails on empty / missing metadata table rather than silently
# no-opping — an empty curated subset means the gate is doing
# nothing, and that regression should be loud, not silent.
#
# Requires: bash, cargo, jq.
set -euo pipefail

command -v jq >/dev/null 2>&1 || {
    echo "error: jq not found on PATH (required to parse cargo metadata)" >&2
    exit 2
}

crates="$(
    cargo metadata --format-version=1 --no-deps \
        | jq -r '.metadata.mango.madsim.crates // [] | .[]'
)"

if [ -z "$crates" ]; then
    echo "error: [workspace.metadata.mango.madsim].crates is empty or missing" >&2
    echo "       update workspace Cargo.toml per docs/madsim.md" >&2
    exit 1
fi

printf '%s\n' "$crates"
