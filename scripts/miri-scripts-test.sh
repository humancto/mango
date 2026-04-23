#!/usr/bin/env bash
# scripts/miri-scripts-test.sh
#
# Regression test for the Miri helper scripts. Asserts:
#   1. miri-crates.sh outputs exactly the curated subset today
#      (currently: `mango-loom-demo`).
#   2. miri-changed-crates.sh returns empty when diffed against HEAD
#      (no changes) AND exits 0.
#   3. miri-changed-crates.sh returns `mango-loom-demo` when diffed
#      against a ref that predates a change to the crate's files.
#
# CI invokes this as part of the `miri-pr` job (script_test step).
# The asserts check both stdout AND exit code per review nit: a
# script that exits 1 with the right stdout is still broken.
#
# Requires: bash, git, jq, cargo.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(git rev-parse --show-toplevel)"

pass() { printf 'ok: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }

# --- test 1 -----------------------------------------------------------
# miri-crates.sh must emit exactly `mango-loom-demo` today.
expected="mango-loom-demo"
actual=$("${script_dir}/miri-crates.sh")
if [ "$actual" != "$expected" ]; then
    fail "miri-crates.sh output mismatch (expected '$expected', got '$actual')"
fi
pass "miri-crates.sh emits curated subset exactly"

# --- test 2 -----------------------------------------------------------
# miri-changed-crates.sh with base ref == HEAD must be empty & exit 0.
set +e
out=$("${script_dir}/miri-changed-crates.sh" HEAD)
rc=$?
set -e
if [ "$rc" -ne 0 ]; then
    fail "miri-changed-crates.sh HEAD exit=$rc (expected 0)"
fi
if [ -n "$out" ]; then
    fail "miri-changed-crates.sh HEAD stdout non-empty: '$out'"
fi
pass "miri-changed-crates.sh empty-diff -> empty, exit 0"

# --- test 3 -----------------------------------------------------------
# Pick a base ref that predates the creation of the curated crate's
# files. The crate was introduced on commit 5b19f8d~1..05beadb range
# (PR #27). We use a stable anchor: the latest commit on `main` where
# `crates/mango-loom-demo` does NOT yet exist.
#
# Strategy: find the parent of the first commit that touched
# crates/mango-loom-demo/, via `git log --diff-filter=A --reverse`.
first_commit=$(cd "$repo_root" \
    && git log --diff-filter=A --format='%H' --reverse \
               -- crates/mango-loom-demo \
    | head -n1)
if [ -z "$first_commit" ]; then
    fail "could not locate first commit introducing crates/mango-loom-demo"
fi
base_ref="${first_commit}^"

set +e
out=$("${script_dir}/miri-changed-crates.sh" "$base_ref")
rc=$?
set -e
if [ "$rc" -ne 0 ]; then
    fail "miri-changed-crates.sh $base_ref exit=$rc"
fi
if [ "$out" != "mango-loom-demo" ]; then
    fail "miri-changed-crates.sh $base_ref output mismatch: '$out'"
fi
pass "miri-changed-crates.sh picks up curated crate when its files changed"

echo "ok: all miri script tests passed"
