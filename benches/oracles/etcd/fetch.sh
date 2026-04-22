#!/usr/bin/env bash
# benches/oracles/etcd/fetch.sh
#
# Downloads the etcd oracle binary pinned in VERSIONS and verifies its
# sha256 against the locally-committed hash. Also fetches the release's
# SHA256SUMS file and verifies both (a) the file's own sha against the
# pin, and (b) that the file agrees with us on the tarball hash. See
# README.md for the full TOFU threat model.
#
# Usage: fetch.sh [dest-dir]
#   dest-dir  where to place the downloaded artifact. Defaults to
#             `./cache` relative to this script's directory.
#
# Exit codes:
#   0  artifact downloaded and verified; path printed on stdout
#   1  any step failed (network, hash mismatch, missing pin)
#
# Testable: `verify_sha <file> <expected-hex>` is exported as a
# function when this script is sourced. See scripts/test-bench-oracle-fetch.sh.

set -euo pipefail

_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../runner/hwsig-lib.sh
. "$_here/../../runner/hwsig-lib.sh"

# verify_sha <file> <expected-hex>
#   Returns 0 on match, 1 on mismatch. On mismatch, prints both
#   hashes to stderr. Uses the cross-platform sha256_of helper.
verify_sha() {
    local file="$1" expected="$2" actual
    actual=$(sha256_of "$file") || return 1
    if [ "$actual" != "$expected" ]; then
        echo "verify_sha: hash mismatch for $file" >&2
        echo "  expected: $expected" >&2
        echo "  actual:   $actual" >&2
        return 1
    fi
    return 0
}

# resolve_platform_key
#   Prints the VERSIONS key for the current platform (e.g.
#   ETCD_SHA256_linux_amd64). Errors out on unsupported platforms.
resolve_platform_key() {
    local os arch
    os=$(uname_os_normalize) || return 1
    arch=$(uname_arch_normalize) || return 1
    printf 'ETCD_SHA256_%s_%s' "$os" "$arch"
}

# platform_extension <os>
#   Etcd uses .tar.gz for linux and .zip for darwin.
platform_extension() {
    case "$1" in
        linux)  echo tar.gz ;;
        darwin) echo zip ;;
        *)      echo "platform_extension: unsupported os: $1" >&2; return 1 ;;
    esac
}

# do_fetch [dest-dir]
#   The real entry point. Extracted from main so the test suite can
#   exercise verify_sha in isolation without running the full
#   download.
do_fetch() {
    local dest="${1:-$_here/cache}"
    mkdir -p "$dest"

    # Load pins.
    # shellcheck source=./VERSIONS
    . "$_here/VERSIONS"

    local key
    key=$(resolve_platform_key)
    local expected_sha="${!key:-}"
    if [ -z "$expected_sha" ]; then
        echo "fetch.sh: no pinned sha for platform key '$key'" >&2
        echo "  add to $_here/VERSIONS and rerun" >&2
        return 1
    fi

    local os arch ext
    os=$(uname_os_normalize)
    arch=$(uname_arch_normalize)
    ext=$(platform_extension "$os")

    local artifact_name="etcd-${ETCD_VERSION}-${os}-${arch}.${ext}"
    local artifact_url="https://github.com/etcd-io/etcd/releases/download/${ETCD_VERSION}/${artifact_name}"
    local artifact_path="$dest/$artifact_name"

    echo "fetch.sh: downloading $artifact_url" >&2
    if ! curl -fsSL "$artifact_url" -o "$artifact_path"; then
        echo "fetch.sh: download failed" >&2
        return 1
    fi

    echo "fetch.sh: verifying tarball sha against VERSIONS" >&2
    verify_sha "$artifact_path" "$expected_sha" || return 1

    # Defense-in-depth: also verify SHA256SUMS's own sha, and that it
    # agrees with us on the tarball.
    local sums_url="https://github.com/etcd-io/etcd/releases/download/${ETCD_VERSION}/SHA256SUMS"
    local sums_path="$dest/SHA256SUMS"
    echo "fetch.sh: downloading $sums_url" >&2
    if ! curl -fsSL "$sums_url" -o "$sums_path"; then
        echo "fetch.sh: SHA256SUMS download failed" >&2
        return 1
    fi

    echo "fetch.sh: verifying SHA256SUMS sha against VERSIONS" >&2
    verify_sha "$sums_path" "$ETCD_SHA256SUMS_SHA256" || return 1

    echo "fetch.sh: cross-checking SHA256SUMS agrees on tarball hash" >&2
    local sums_entry
    sums_entry=$(awk -v name="$artifact_name" '$2 == name { print $1 }' "$sums_path")
    if [ -z "$sums_entry" ]; then
        echo "fetch.sh: SHA256SUMS has no entry for $artifact_name" >&2
        return 1
    fi
    if [ "$sums_entry" != "$expected_sha" ]; then
        echo "fetch.sh: SHA256SUMS disagrees with VERSIONS on $artifact_name" >&2
        echo "  VERSIONS:     $expected_sha" >&2
        echo "  SHA256SUMS:   $sums_entry" >&2
        return 1
    fi

    echo "fetch.sh: ok, verified $artifact_path" >&2
    printf '%s\n' "$artifact_path"
}

# When executed directly (not sourced), run the full fetch.
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    do_fetch "$@"
fi
