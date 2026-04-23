#!/usr/bin/env bash
# scripts/miri-changed-crates.sh
#
# Intersects the Miri curated subset (from miri-crates.sh) with the
# set of workspace crates whose files changed against a base ref.
# One crate name per line on stdout. If the intersection is empty,
# exits 0 with no output.
#
# Usage:
#   scripts/miri-changed-crates.sh <base-ref>
#
# `<base-ref>` is typically `origin/${GITHUB_BASE_REF}` on a
# pull_request or merge_group event. The diff uses the three-dot
# form (`base...HEAD`), i.e. changes introduced on this branch
# since it diverged from the merge base — not all changes on `base`
# since the fork point. This matches GitHub's "Files changed" view.
#
# Do NOT invoke this on `push` / `schedule` / `workflow_dispatch`:
# base-ref is meaningless there and `miri.yml` routes those events
# straight at the full subset via miri-crates.sh.
#
# Requires: bash, git, jq, cargo (via miri-crates.sh).
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <base-ref>" >&2
    exit 2
fi

base_ref="$1"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Files changed between merge base and HEAD.
changed_files=$(git diff --name-only "${base_ref}...HEAD" || true)
if [ -z "$changed_files" ]; then
    exit 0
fi

# Full curated subset.
curated=$("${script_dir}/miri-crates.sh")
if [ -z "$curated" ]; then
    exit 0
fi

# Workspace map: crate-name -> manifest-dir (relative to repo root).
# jq resolves `manifest_path` which is absolute; strip the repo root
# prefix so we can substring-match against `git diff` output.
repo_root=$(git rev-parse --show-toplevel)
workspace_map=$(cargo metadata --format-version=1 --no-deps \
    | jq -r --arg root "${repo_root}/" \
          '.packages[] | [.name, (.manifest_path | sub($root; "") | sub("/Cargo\\.toml$"; ""))] | @tsv')

# For each curated crate, check whether any changed file lives under
# that crate's manifest directory. If so, emit the crate name.
while IFS= read -r crate; do
    [ -z "$crate" ] && continue
    crate_dir=$(printf '%s\n' "$workspace_map" \
        | awk -F'\t' -v c="$crate" '$1 == c { print $2; exit }')
    if [ -z "$crate_dir" ]; then
        echo "warning: curated crate '$crate' not found in workspace metadata" >&2
        continue
    fi
    # Match any changed file whose path begins with the crate dir +
    # '/'. Anchored to avoid 'foo' matching 'foo-bar'.
    if printf '%s\n' "$changed_files" | grep -q "^${crate_dir}/"; then
        printf '%s\n' "$crate"
    fi
done <<< "$curated"
