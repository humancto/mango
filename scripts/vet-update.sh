#!/usr/bin/env bash
# scripts/vet-update.sh
#
# Contributor helper for the cargo-vet supply-chain audit gate.
#
# What it does, in order:
#   1. Verifies the local cargo-vet binary matches the pin in
#      .github/workflows/vet.yml (CARGO_VET_VERSION). CI is exact;
#      there's no point running this with a different local
#      version because the result will be non-deterministic.
#   2. Runs `cargo vet regenerate imports` to refresh
#      supply-chain/imports.lock from the three canonical feeds.
#      This is the "pull in new audits" step — an audit that was
#      not present when a contributor last ran this might now
#      cover a previously-exempted crate, shrinking the diff.
#   3. Runs `cargo vet check --locked --frozen` — the exact gate
#      the CI workflow runs. Must pass before pushing.
#   4. Runs xtask-vet-ttl to verify every exemption has a
#      non-expired review-by token.
#   5. Prints a diff summary so the contributor knows what
#      supply-chain/ changes to commit.
#
# Usage (from the repo root):
#   bash scripts/vet-update.sh
#
# Exit codes:
#   0  everything clean, supply-chain/ may or may not have changed
#   1  cargo-vet version mismatch, or a gate failed
#
# NOT for CI use. The workflow at .github/workflows/vet.yml runs
# `cargo vet check` directly; this script is for human contributors
# after editing Cargo.toml or touching supply-chain/.

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

# Extract the pin from vet.yml. Single source of truth.
workflow_file=".github/workflows/vet.yml"
if [ ! -f "$workflow_file" ]; then
    printf 'error: %s not found (run from repo root)\n' "$workflow_file" >&2
    exit 1
fi

want_version="$(
    grep -E '^\s*CARGO_VET_VERSION:\s*"[^"]+"' "$workflow_file" \
        | head -n 1 \
        | sed -E 's/.*"([^"]+)".*/\1/'
)"
if [ -z "${want_version:-}" ]; then
    printf 'error: could not extract CARGO_VET_VERSION from %s\n' "$workflow_file" >&2
    exit 1
fi

if ! command -v cargo-vet >/dev/null 2>&1; then
    printf 'error: cargo-vet is not installed.\n  install with: cargo install cargo-vet --version %s --locked\n' "$want_version" >&2
    exit 1
fi

got_version="$(cargo vet --version | awk '{print $2}')"
if [ "$got_version" != "$want_version" ]; then
    printf 'error: cargo-vet version mismatch.\n  want: %s (from %s)\n  got:  %s\n  bump with: cargo install cargo-vet --version %s --locked --force\n' \
        "$want_version" "$workflow_file" "$got_version" "$want_version" >&2
    exit 1
fi

printf '==> cargo vet regenerate imports\n'
cargo vet regenerate imports

printf '\n==> cargo vet check --locked --frozen\n'
cargo vet check --locked --frozen

printf '\n==> xtask-vet-ttl\n'
cargo run -q -p xtask-vet-ttl

printf '\n==> supply-chain/ diff vs HEAD\n'
if git diff --quiet -- supply-chain/; then
    printf '  (no changes)\n'
else
    git diff --stat -- supply-chain/
    printf '\nReview the diff and commit:\n'
    printf '  git add supply-chain/\n'
    printf '  git commit -m "chore(supply-chain): refresh vet imports"\n'
fi

printf '\nAll gates PASS.\n'
