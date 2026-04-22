#!/usr/bin/env bash
# scripts/test-bench-run-wrapper.sh
#
# Tests for benches/runner/run.sh:
#   A — signature goes to stderr, stdout is exactly the wrapped
#       command's output (no prepended signature line).
#   B — sidecar signature.txt is written when BENCH_OUT_DIR is set.
#   C — non-zero exit of the wrapped command propagates.
#   D — BENCH_TIER unset + bench argv is a hard fail from run.sh
#       (exit 2, wrapped command never runs).
#   E — no argv → print signature to stderr and exit 0.

set -euo pipefail

_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
_repo="$(cd "$_here/.." && pwd)"

run_sh="$_repo/benches/runner/run.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "ok: $*"; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# -----------------------------------------------------------------
# A — stdout clean, stderr has signature
# -----------------------------------------------------------------

BENCH_TIER=1 bash "$run_sh" echo hello world \
    >"$tmp/out" 2>"$tmp/err"

if [ "$(cat "$tmp/out")" != "hello world" ]; then
    echo "A stdout was not clean:" >&2
    cat "$tmp/out" >&2
    exit 1
fi
pass "A stdout contains exactly the wrapped command output"

if ! grep -q '^BENCH_HW v1: ' "$tmp/err"; then
    echo "A stderr missing signature:" >&2
    cat "$tmp/err" >&2
    exit 1
fi
pass "A stderr contains BENCH_HW signature line"

# -----------------------------------------------------------------
# B — sidecar signature.txt
# -----------------------------------------------------------------

outdir="$tmp/outdir"
BENCH_TIER=1 BENCH_OUT_DIR="$outdir" bash "$run_sh" echo sidecar-test \
    >/dev/null 2>"$tmp/err2"

if [ ! -f "$outdir/signature.txt" ]; then
    fail "B BENCH_OUT_DIR set but signature.txt not written"
fi
if ! grep -q '^BENCH_HW v1: ' "$outdir/signature.txt"; then
    fail "B signature.txt content not a BENCH_HW line: $(cat "$outdir/signature.txt")"
fi
# Sidecar must match what went to stderr.
if ! diff -q "$outdir/signature.txt" <(grep '^BENCH_HW v1: ' "$tmp/err2") >/dev/null; then
    fail "B sidecar content differs from stderr signature"
fi
pass "B BENCH_OUT_DIR → signature.txt written and matches stderr"

# -----------------------------------------------------------------
# C — exit propagation
# -----------------------------------------------------------------

set +e
BENCH_TIER=1 bash "$run_sh" false >/dev/null 2>/dev/null
rc=$?
set -e
if [ "$rc" = 0 ]; then
    fail "C expected non-zero exit from wrapped 'false', got $rc"
fi
pass "C non-zero exit of wrapped command propagates (rc=$rc)"

# -----------------------------------------------------------------
# D — BENCH_TIER unset + bench argv → exit 2 before exec
# -----------------------------------------------------------------

set +e
env -u BENCH_TIER bash "$run_sh" cargo bench --bench foo \
    >"$tmp/d-out" 2>"$tmp/d-err"
rc=$?
set -e
if [ "$rc" != 2 ]; then
    fail "D expected exit 2, got $rc"
fi
if ! grep -q 'BENCH_TIER' "$tmp/d-err"; then
    fail "D expected BENCH_TIER error on stderr"
fi
# The wrapped command must NOT have run — cargo would emit something
# to stdout or stderr if it had. The stdout must be empty.
if [ -s "$tmp/d-out" ]; then
    fail "D wrapped command appears to have run despite BENCH_TIER unset; stdout: $(cat "$tmp/d-out")"
fi
pass "D BENCH_TIER unset + bench argv → exit 2, wrapped cmd not executed"

# -----------------------------------------------------------------
# E — no argv → signature + exit 0
# -----------------------------------------------------------------

set +e
BENCH_TIER=1 bash "$run_sh" >"$tmp/e-out" 2>"$tmp/e-err"
rc=$?
set -e
if [ "$rc" != 0 ]; then
    fail "E expected exit 0 with no argv, got $rc"
fi
if ! grep -q '^BENCH_HW v1: ' "$tmp/e-err"; then
    fail "E no-argv run should emit signature to stderr"
fi
if [ -s "$tmp/e-out" ]; then
    fail "E no-argv run wrote to stdout (should be empty): $(cat "$tmp/e-out")"
fi
pass "E no argv → signature on stderr, stdout empty, exit 0"

echo "all run-wrapper tests passed"
