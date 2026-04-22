#!/usr/bin/env bash
# benches/runner/hwsig-lib.sh
#
# Portable helpers shared by the bench-harness shell scripts. Sourced,
# never executed directly. Responsible for the cross-platform concerns
# that would otherwise be scattered across fetch.sh, run.sh, and
# hardware-signature.sh.
#
# All functions print their answer on stdout and return 0/1 for
# success/failure. Diagnostics go to stderr.
#
# Shell dialect: bash, no bashisms beyond what's portable to bash 3.2
# (macOS's system bash). No arrays-of-arrays, no `readarray`.

# sha256_of <file>
#
# Prints the hex sha256 digest of <file> on stdout, nothing else.
# Picks the right tool per platform: `sha256sum` on Linux (coreutils),
# `shasum -a 256` on macOS (ships by default as a Perl script).
sha256_of() {
    local file="$1"
    if [ ! -r "$file" ]; then
        echo "sha256_of: cannot read file: $file" >&2
        return 1
    fi
    case "$(uname -s)" in
        Linux)
            if command -v sha256sum >/dev/null 2>&1; then
                sha256sum "$file" | cut -d' ' -f1
            else
                echo "sha256_of: sha256sum not found on Linux; install coreutils" >&2
                return 1
            fi
            ;;
        Darwin)
            if command -v shasum >/dev/null 2>&1; then
                shasum -a 256 "$file" | cut -d' ' -f1
            else
                echo "sha256_of: shasum not found on macOS (should ship by default)" >&2
                return 1
            fi
            ;;
        *)
            echo "sha256_of: unsupported platform: $(uname -s)" >&2
            return 1
            ;;
    esac
}

# sha256_of_string <string>
#
# Prints the hex sha256 digest of the string (no trailing newline in
# the hashed input). Uses a temp file because `printf | sha256sum` has
# subtle cross-platform buffering differences and we want the hash
# path to be identical to sha256_of's.
sha256_of_string() {
    local tmp
    tmp=$(mktemp)
    printf '%s' "$1" > "$tmp"
    local digest
    digest=$(sha256_of "$tmp") || { rm -f "$tmp"; return 1; }
    rm -f "$tmp"
    printf '%s' "$digest"
}

# uname_arch_normalize
#
# Normalizes `uname -m` output to the names etcd uses in its release
# artifacts: amd64, arm64. Exits 1 on unknown arch so callers can
# detect rather than silently get `unknown`.
uname_arch_normalize() {
    case "$(uname -m)" in
        x86_64|amd64)  echo amd64 ;;
        aarch64|arm64) echo arm64 ;;
        *)
            echo "uname_arch_normalize: unsupported arch: $(uname -m)" >&2
            return 1
            ;;
    esac
}

# uname_os_normalize
#
# Lowercases `uname -s` and restricts to {linux, darwin}. Exits 1 on
# anything else (Windows, BSDs) so the scaffold fails loudly.
uname_os_normalize() {
    case "$(uname -s)" in
        Linux)  echo linux ;;
        Darwin) echo darwin ;;
        *)
            echo "uname_os_normalize: unsupported OS: $(uname -s)" >&2
            return 1
            ;;
    esac
}

# trim_ws <string>
#
# Strips leading and trailing whitespace (spaces and tabs) from the
# argument. Required before hashing field values because /proc/cpuinfo's
# `model name` is tab-padded on some distros and the padding varies by
# kernel version.
trim_ws() {
    # Parameter-expansion based strip — portable to bash 3.2.
    local s="$1"
    # Leading
    s="${s#"${s%%[![:space:]]*}"}"
    # Trailing
    s="${s%"${s##*[![:space:]]}"}"
    printf '%s' "$s"
}

# value_encode <string>
#
# Percent-encodes a signature field value so it contains no
# whitespace and no field-separator characters. This is what makes
# space-separated field parsing safe without quoting: the encoded
# value is guaranteed to be a single shell-word.
#
# Encoding is minimal (not full RFC 3986) — only characters that would
# break the BENCH_HW line format are encoded:
#   `%` → `%25`  (must be first, or subsequent escapes collide)
#   ` ` → `%20`
#   TAB → `%09`
#   `=` → `%3D`  (so a value can't accidentally look like a new key)
#
#   value_encode "AMD EPYC 7B13"  →  AMD%20EPYC%207B13
value_encode() {
    local s="$1"
    s="${s//%/%25}"
    s="${s// /%20}"
    s="${s//	/%09}"
    s="${s//=/%3D}"
    printf '%s' "$s"
}
