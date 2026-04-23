#!/usr/bin/env bash
# Test-watchdog regression test.
#
# Proves that `.config/nextest.toml`'s `slow-timeout = { period = "…",
# terminate-after = 1 }` actually kills a runaway test and reports it
# as failed. Without this script, a future refactor could silently
# drop `terminate-after` and downgrade the watchdog to a warning-
# only mode; CI would stay green and the timeout policy would rot.
#
# How it works:
#   1. Invoke `cargo nextest run --run-ignored only` scoped to the
#      `watchdog_kill_smoke` test in `crates/mango` (which sleeps
#      90s — past the 30s unit-class budget).
#   2. Assert the nextest run exits non-zero.
#   3. Assert the output contains `TIMEOUT` (nextest's kill-marker
#      token — empirically verified against cargo-nextest 0.9.x).
#   4. Assert the run completed in ~30s (not 90s) — i.e., the test
#      was actually killed, not allowed to finish.
#
# Runs in < 35 seconds. Invoked inline from the required `test`
# CI job — if this script fails, the PR is blocked. See
# `.github/workflows/ci.yml` and docs/testing.md.

set -euo pipefail

# Locate the repo root so this script is callable from anywhere.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

if ! command -v cargo-nextest >/dev/null 2>&1; then
  echo "test-watchdog: cargo-nextest is not installed. Install with:"
  echo "  cargo install cargo-nextest --locked"
  exit 2
fi

OUT_FILE="$(mktemp -t mango-watchdog-XXXXXX.log)"
trap 'rm -f "${OUT_FILE}"' EXIT

echo "test-watchdog: running watchdog_kill_smoke under --profile ci"
START_TS=$(date +%s)
set +e
cargo nextest run \
  --profile ci \
  --run-ignored only \
  --locked \
  -E 'test(~watchdog_kill_smoke)' \
  -p mango \
  >"${OUT_FILE}" 2>&1
NEXTEST_EXIT=$?
set -e
END_TS=$(date +%s)
ELAPSED=$((END_TS - START_TS))

echo "test-watchdog: nextest exited with ${NEXTEST_EXIT} after ${ELAPSED}s"

# Assertion 1: non-zero exit (the test was recorded as failed).
if [ "${NEXTEST_EXIT}" = "0" ]; then
  echo "test-watchdog: FAIL — nextest exit code was 0 (expected non-zero)."
  echo "test-watchdog: the watchdog did NOT kill the runaway test."
  echo "--- nextest output ---"
  cat "${OUT_FILE}"
  exit 1
fi

# Assertion 2: TIMEOUT token present (nextest's kill marker).
if ! grep -q 'TIMEOUT' "${OUT_FILE}"; then
  echo "test-watchdog: FAIL — 'TIMEOUT' marker not found in nextest output."
  echo "test-watchdog: this likely means terminate-after was dropped from"
  echo "               .config/nextest.toml, downgrading kills to warnings."
  echo "--- nextest output ---"
  cat "${OUT_FILE}"
  exit 1
fi

# Assertion 3: the run was killed within a bounded window, not after
# the full 90s sleep. Allow generous headroom for slow CI runners
# (compile/link overhead before the test starts) — 60s is the ceiling;
# anything below that proves a kill rather than a natural exit.
if [ "${ELAPSED}" -ge 60 ]; then
  echo "test-watchdog: FAIL — elapsed ${ELAPSED}s exceeds 60s ceiling."
  echo "test-watchdog: the test likely ran to natural completion (90s)"
  echo "               instead of being killed at 30s."
  echo "--- nextest output ---"
  cat "${OUT_FILE}"
  exit 1
fi

echo "test-watchdog: PASS — watchdog killed the runaway test at ~30s."
