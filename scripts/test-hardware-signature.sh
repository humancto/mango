#!/usr/bin/env bash
# scripts/test-hardware-signature.sh
#
# Tests for benches/runner/hardware-signature.sh:
#   A — line shape: matches the BENCH_HW v1: canonical format.
#   B — field sorting: keys appear lexically in the output.
#   C — short-term determinism: re-running in the same shell within
#       5 seconds produces an identical line.
#   D — tier handling: unset (soft warn for non-bench argv), invalid
#       (hard fail), 1 or 2 (ok).
#   E — canonicalization round-trip: recompute sha from the rest of
#       the line; assert match.

set -euo pipefail

_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
_repo="$(cd "$_here/.." && pwd)"

sig_sh="$_repo/benches/runner/hardware-signature.sh"
lib_sh="$_repo/benches/runner/hwsig-lib.sh"

# shellcheck source=../benches/runner/hwsig-lib.sh
. "$lib_sh"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "ok: $*"; }

# -----------------------------------------------------------------
# A — line shape
# -----------------------------------------------------------------

line_ok=$(BENCH_TIER=1 bash "$sig_sh" 2>/dev/null)

if ! printf '%s' "$line_ok" | grep -Eq '^BENCH_HW v1: ([a-z_]+=[^[:space:]]+ )+sha=[0-9a-f]{64}$'; then
    echo "A line shape mismatch:" >&2
    echo "  $line_ok" >&2
    exit 1
fi
pass "A line shape matches BENCH_HW v1 regex"

# -----------------------------------------------------------------
# B — fields sorted lexically by key
# -----------------------------------------------------------------

# Strip the "BENCH_HW v1: " prefix and the trailing " sha=..." chunk.
body=${line_ok#BENCH_HW v1: }
body_without_sha=${body% sha=*}

prev=""
while IFS= read -r kv; do
    key=${kv%%=*}
    if [ -n "$prev" ] && [ "$prev" \> "$key" ]; then
        fail "B fields out of order: '$prev' > '$key' in line: $line_ok"
    fi
    prev=$key
done < <(printf '%s\n' "$body_without_sha" | tr ' ' '\n')
pass "B fields are lexically sorted by key"

# -----------------------------------------------------------------
# C — short-term determinism
# -----------------------------------------------------------------

line_again=$(BENCH_TIER=1 bash "$sig_sh" 2>/dev/null)
if [ "$line_ok" != "$line_again" ]; then
    echo "C determinism failure (two runs within the same second differ):" >&2
    echo "  first:  $line_ok" >&2
    echo "  second: $line_again" >&2
    exit 1
fi
pass "C same-host same-shell run is deterministic"

# -----------------------------------------------------------------
# D — tier handling
# -----------------------------------------------------------------

# D1: BENCH_TIER=1 produces tier=1.
if ! printf '%s' "$line_ok" | grep -q ' tier=1 '; then
    fail "D1 expected tier=1 in output, got: $line_ok"
fi
pass "D1 BENCH_TIER=1 → tier=1"

# D2: BENCH_TIER=2 produces tier=2.
line_t2=$(BENCH_TIER=2 bash "$sig_sh" 2>/dev/null)
if ! printf '%s' "$line_t2" | grep -q ' tier=2 '; then
    fail "D2 expected tier=2, got: $line_t2"
fi
pass "D2 BENCH_TIER=2 → tier=2"

# D3: BENCH_TIER unset + non-bench argv produces tier=unknown + stderr warning.
out=$(env -u BENCH_TIER bash "$sig_sh" 2>/tmp/hwsig-stderr-$$.txt)
if ! printf '%s' "$out" | grep -q ' tier=unknown '; then
    rm -f "/tmp/hwsig-stderr-$$.txt"
    fail "D3 expected tier=unknown in stdout, got: $out"
fi
if ! grep -q 'BENCH_TIER unset' "/tmp/hwsig-stderr-$$.txt"; then
    rm -f "/tmp/hwsig-stderr-$$.txt"
    fail "D3 expected BENCH_TIER-unset warning on stderr"
fi
rm -f "/tmp/hwsig-stderr-$$.txt"
pass "D3 BENCH_TIER unset → tier=unknown + stderr warning"

# D4: BENCH_TIER=3 is a hard error.
if BENCH_TIER=3 bash "$sig_sh" >/dev/null 2>&1; then
    fail "D4 BENCH_TIER=3 should have failed but exited 0"
fi
pass "D4 BENCH_TIER=3 → exit non-zero"

# D5: BENCH_TIER unset + argv containing 'bench' is a hard error (exit 2).
set +e
env -u BENCH_TIER bash "$sig_sh" cargo bench --bench foo >/dev/null 2>/dev/null
rc=$?
set -e
if [ $rc -ne 2 ]; then
    fail "D5 expected exit 2 with argv containing 'bench', got $rc"
fi
pass "D5 BENCH_TIER unset + bench argv → exit 2"

# -----------------------------------------------------------------
# E — canonicalization round-trip
# -----------------------------------------------------------------

# Extract claimed sha.
claimed=${line_ok##* sha=}
# body_without_sha computed above.
recomputed=$(sha256_of_string "$body_without_sha")

if [ "$claimed" != "$recomputed" ]; then
    echo "E round-trip mismatch:" >&2
    echo "  line:       $line_ok" >&2
    echo "  body-hashed: $body_without_sha" >&2
    echo "  claimed:    $claimed" >&2
    echo "  recomputed: $recomputed" >&2
    exit 1
fi
pass "E canonicalization round-trip matches"

echo "all hardware-signature tests passed"
