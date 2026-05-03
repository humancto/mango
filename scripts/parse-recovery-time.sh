#!/usr/bin/env bash
# Parse `MANGO_RECOVERY_TIME ...` lines from `cargo nextest run
# --no-capture` output into a JSON array of measurements.
#
# Usage:
#   scripts/parse-recovery-time.sh <input.log> <output.json>
#
# Input format (one line per scenario, emitted by
# crates/mango-storage/tests/recovery_time.rs):
#
#   Pass:
#     MANGO_RECOVERY_TIME scenario=1GiB wall_ms=12345 cache=cold samples_ok=64
#
#   Skip (insufficient disk):
#     MANGO_RECOVERY_TIME scenario=8GiB skipped=insufficient_disk free_bytes=1024 required_bytes=21474836480
#
# Output: `target/recovery-time.json` — a JSON array of one object
# per recognized line. Both shapes round-trip: the pass shape
# contributes wall_ms/cache/samples_ok keys, the skip shape
# contributes skipped/free_bytes/required_bytes keys.
#
# This script is INTENTIONALLY not pulling jq — the grammar is
# fixed-shape and trivial; bash + sed is enough, and the CI step
# stays portable to environments without jq. Adding jq would create
# a new dependency for one upload step.
#
# This script always exits 0 even if the input has no recognized
# lines (writes `[]`) — a missing measurement is the report; the
# upstream nextest step's exit code is the failure signal. The CI
# step's `if: always()` guarantees this script runs even if the
# test failed, so a budget overrun produces the artifact.

set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <input.log> <output.json>" >&2
  exit 2
fi

IN="$1"
OUT="$2"

mkdir -p "$(dirname "${OUT}")"

if [[ ! -f "${IN}" ]]; then
  echo "[]" > "${OUT}"
  echo "parse-recovery-time: input file ${IN} missing — wrote empty array" >&2
  exit 0
fi

# Build the JSON array. Each line that starts with
# `MANGO_RECOVERY_TIME ` produces one object. Unknown shapes inside
# the prefix are tolerated (kv pairs serialized verbatim) so a
# future schema add doesn't break parsing.
{
  echo "["
  first=1
  # Match only lines with the prefix. `|| true` so an empty match
  # set doesn't trip `set -e` via grep's exit 1.
  while IFS= read -r line; do
    [[ -z "${line}" ]] && continue
    if [[ ${first} -eq 1 ]]; then
      first=0
    else
      echo ","
    fi

    # Drop the prefix; tokenize the rest by spaces.
    rest="${line#MANGO_RECOVERY_TIME }"

    printf '  {'
    inner_first=1
    # shellcheck disable=SC2086
    set -- ${rest}
    for tok in "$@"; do
      key="${tok%%=*}"
      val="${tok#*=}"
      if [[ ${inner_first} -eq 1 ]]; then
        inner_first=0
      else
        printf ', '
      fi
      # Detect numeric vs string. Numeric: pure digits.
      if [[ "${val}" =~ ^[0-9]+$ ]]; then
        printf '"%s": %s' "${key}" "${val}"
      else
        printf '"%s": "%s"' "${key}" "${val}"
      fi
    done
    printf '}'
  done < <(grep '^MANGO_RECOVERY_TIME ' "${IN}" || true)
  echo
  echo "]"
} > "${OUT}"

count=$(grep -c '^MANGO_RECOVERY_TIME ' "${IN}" || true)
echo "parse-recovery-time: wrote ${count} measurement(s) to ${OUT}" >&2
