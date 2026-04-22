#!/usr/bin/env bash
# scripts/test-bench-oracle-fetch.sh
#
# Tests for benches/oracles/etcd/fetch.sh:
#   Part A — verify_sha accepts a correct hash and rejects a mutated one.
#   Part B — VERSIONS ↔ HARDWARE.md platform coverage: every supported
#            platform has a pinned sha, and no stray shas exist.
#
# No network. No actual etcd download. The integration path
# (curl + verify_sha) is one-liner glue; the load-bearing logic is
# the verifier and the VERSIONS file contents.

set -euo pipefail

_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
_repo="$(cd "$_here/.." && pwd)"

fetch_sh="$_repo/benches/oracles/etcd/fetch.sh"
versions="$_repo/benches/oracles/etcd/VERSIONS"
hardware_md="$_repo/benches/runner/HARDWARE.md"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "ok: $*"; }

# -----------------------------------------------------------------
# Part A — verify_sha
# -----------------------------------------------------------------

# shellcheck source=../benches/oracles/etcd/fetch.sh
. "$fetch_sh"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fixture="$tmp/fixture.bin"
printf 'mango-bench-harness-fixture\n' > "$fixture"
correct=$(sha256_of "$fixture")

# A1: correct sha passes.
if verify_sha "$fixture" "$correct" >/dev/null 2>&1; then
    pass "A1 verify_sha accepts correct hash"
else
    fail "A1 verify_sha rejected correct hash"
fi

# A2: mutated sha fails.
wrong="0000000000000000000000000000000000000000000000000000000000000000"
if verify_sha "$fixture" "$wrong" >/dev/null 2>&1; then
    fail "A2 verify_sha accepted wrong hash"
else
    pass "A2 verify_sha rejects wrong hash"
fi

# A3: nonexistent file fails cleanly.
if verify_sha "$tmp/nonexistent" "$correct" >/dev/null 2>&1; then
    fail "A3 verify_sha accepted nonexistent file"
else
    pass "A3 verify_sha rejects nonexistent file"
fi

# -----------------------------------------------------------------
# Part B — VERSIONS ↔ HARDWARE.md platform coverage
# -----------------------------------------------------------------

# Extract supported-platform list from HARDWARE.md's table. Accept any
# whitespace between | separators. The table header row is
# `| OS | Arch |` so we key off that to find the start.
#
# Output: one "os_arch" per line.
extract_hardware_platforms() {
    awk '
        BEGIN { in_table = 0 }
        /^\| OS[ ]+\| Arch/ { in_table = 1; next }
        in_table && /^\| ---/ { next }
        in_table && /^\|/ {
            gsub(/[ \t]+/, "")
            # Row looks like |linux|amd64|  → split on |
            n = split($0, f, "|")
            # f[1] is empty (leading |), f[2]=os, f[3]=arch, f[4] empty (trailing |)
            if (n >= 3 && f[2] != "" && f[3] != "") {
                print f[2] "_" f[3]
            }
            next
        }
        in_table && !/^\|/ { in_table = 0 }
    ' "$hardware_md"
}

# Extract VERSIONS keys of the form ETCD_SHA256_<os>_<arch> (not the
# file-itself hash, ETCD_SHA256SUMS_SHA256).
extract_versions_platforms() {
    awk -F= '
        /^ETCD_SHA256_[a-z]+_[a-z0-9]+=/ {
            key = $1
            # strip ETCD_SHA256_ prefix
            sub(/^ETCD_SHA256_/, "", key)
            # exclude SUMS_SHA256 if the regex above missed — it should not
            if (key == "SUMS_SHA256") next
            print key
        }
    ' "$versions"
}

hw_platforms=$(extract_hardware_platforms | LC_ALL=C sort -u)
ver_platforms=$(extract_versions_platforms | LC_ALL=C sort -u)

if [ -z "$hw_platforms" ]; then
    fail "B parser found no platforms in HARDWARE.md — parser broken or doc restructured"
fi
if [ -z "$ver_platforms" ]; then
    fail "B parser found no ETCD_SHA256_<os>_<arch> keys in VERSIONS"
fi

# B1: every HARDWARE.md platform has a pinned sha.
missing_from_versions=$(comm -23 <(echo "$hw_platforms") <(echo "$ver_platforms"))
if [ -n "$missing_from_versions" ]; then
    echo "B1 HARDWARE.md lists platforms with no ETCD_SHA256 pin:" >&2
    printf '  %s\n' $missing_from_versions >&2
    exit 1
fi
pass "B1 every HARDWARE.md platform is pinned in VERSIONS"

# B2: every VERSIONS pin has a declared supported platform.
extra_in_versions=$(comm -13 <(echo "$hw_platforms") <(echo "$ver_platforms"))
if [ -n "$extra_in_versions" ]; then
    echo "B2 VERSIONS pins platforms not listed in HARDWARE.md:" >&2
    printf '  %s\n' $extra_in_versions >&2
    exit 1
fi
pass "B2 every pinned platform appears in HARDWARE.md"

# B3: ETCD_VERSION is set and looks like vX.Y.Z
if ! grep -Eq '^ETCD_VERSION=v[0-9]+\.[0-9]+\.[0-9]+$' "$versions"; then
    fail "B3 ETCD_VERSION missing or malformed in VERSIONS"
fi
pass "B3 ETCD_VERSION is present and well-formed"

# B4: ETCD_SHA256SUMS_SHA256 is present and looks like sha256.
if ! grep -Eq '^ETCD_SHA256SUMS_SHA256=[0-9a-f]{64}$' "$versions"; then
    fail "B4 ETCD_SHA256SUMS_SHA256 missing or malformed in VERSIONS"
fi
pass "B4 ETCD_SHA256SUMS_SHA256 is present and 64-hex"

# B5: every per-platform sha is 64-hex.
bad=$(awk -F= '/^ETCD_SHA256_[a-z]+_[a-z0-9]+=/ { if ($2 !~ /^[0-9a-f]{64}$/) print $1 }' "$versions")
if [ -n "$bad" ]; then
    echo "B5 VERSIONS has malformed sha values:" >&2
    printf '  %s\n' $bad >&2
    exit 1
fi
pass "B5 all per-platform shas are 64-hex"

echo "all oracle-fetch tests passed"
