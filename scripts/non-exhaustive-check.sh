#!/usr/bin/env bash
# scripts/non-exhaustive-check.sh
#
# Structural backstop for the `#[non_exhaustive]` policy on public
# enums (ROADMAP.md:804, docs/api-stability.md).
#
# Primary enforcement is `clippy::exhaustive_enums = "deny"` in
# `[workspace.lints.clippy]` — clippy catches every `pub enum` in
# every publishable crate at PR time. This script is a
# defense-in-depth backstop that catches the failure mode where
# someone removes the workspace lint entry (clippy silently stops
# firing; this script keeps the invariant visible).
#
# What it asserts:
#   1. `[workspace.lints.clippy]` in `Cargo.toml` carries
#      `exhaustive_enums = "deny"`.
#   2. For every publishable crate (as reported by `cargo metadata`
#      + the jq filter shared with public-api / semver-checks),
#      every `pub enum` in `src/**/*.rs` either:
#      (a) has `#[non_exhaustive]` on the enum, OR
#      (b) has `#[allow(clippy::exhaustive_enums)]` preceded
#          immediately by a `// reason: ...` line-comment, OR
#      (c) is covered by a crate-level
#          `#![allow(clippy::exhaustive_enums)]` in lib.rs.
#
# Predicate for "publishable": `publish != []` AND `source == null`.
# Same shape used by `scripts/public-api-scripts-test.sh` and
# `public-api.yml`'s jq filter — keeping it identical across the
# three callers prevents drift.
#
# Exit codes:
#   0  PASS
#   1  FAIL — workspace lint entry missing
#   2  FAIL — pub enum without attribute or escape
#   3  FAIL — escape present but `// reason:` comment missing
#
# CI vs local:
#   When $CI is set, missing `cargo` or `jq` is a hard failure
#   (silent pass in CI would miss regressions). Locally, the
#   script prints an install hint and skips only the specific
#   assertion requiring the missing tool.
#
# Invocation:
#   bash scripts/non-exhaustive-check.sh
#   (run from any CWD; script cd's to repo root)

set -u

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

pass_count=0
fail_count=0
skip_count=0

pass() {
    printf 'PASS  %s\n' "$1"
    pass_count=$((pass_count + 1))
}
fail() {
    printf 'FAIL  %s\n' "$1" >&2
    fail_count=$((fail_count + 1))
}
skip() {
    printf 'SKIP  %s\n' "$1"
    skip_count=$((skip_count + 1))
}

missing_tool() {
    local scenario="$1"
    local tool="$2"
    local hint="$3"
    if [ -n "${CI:-}" ]; then
        fail "$scenario ($tool missing in CI — $hint)"
    else
        skip "$scenario ($tool missing locally — $hint)"
    fi
}

cargo_toml="Cargo.toml"
policy_doc="docs/api-stability.md"

# --- 1. workspace lint entry present --------------------------------
# Match the TOML line `exhaustive_enums = { level = "deny", ... }`
# inside `[workspace.lints.clippy]`. We look for the specific level
# = "deny" shape rather than any `exhaustive_enums =` to avoid
# accidentally accepting a `"warn"` or `"allow"` downgrade.
scenario="Cargo.toml declares clippy::exhaustive_enums at deny level"
if grep -qE '^[[:space:]]*exhaustive_enums[[:space:]]*=[[:space:]]*\{[[:space:]]*level[[:space:]]*=[[:space:]]*"deny"' "$cargo_toml"; then
    pass "$scenario"
else
    fail "$scenario (expected \`exhaustive_enums = { level = \"deny\", priority = 1 }\` in [workspace.lints.clippy])"
fi

# --- 2. policy doc exists -------------------------------------------
scenario="policy doc exists at $policy_doc"
if [ -f "$policy_doc" ]; then
    pass "$scenario"
else
    fail "$scenario"
fi

# --- 3. enumerate publishable crates --------------------------------
# Uses `cargo metadata --no-deps` (stays inside workspace members,
# doesn't fetch dependency metadata) + jq to filter to publishable
# paths. Predicate: `publish != []` AND `source == null`.
#
# Guard with CI-vs-local semantics: if either tool is missing,
# CI fails hard and local skips.
publishable_crates=""
have_tools=1
if ! command -v cargo >/dev/null 2>&1; then
    missing_tool "enumerate publishable crates" "cargo" "install rustup + cargo"
    have_tools=0
fi
if ! command -v jq >/dev/null 2>&1; then
    missing_tool "enumerate publishable crates" "jq" "brew install jq or apt-get install jq"
    have_tools=0
fi

if [ "$have_tools" = "1" ]; then
    # One line per publishable crate: "<name>\t<manifest_dir>"
    # Each path is absolute. The `manifest_path` returned by cargo
    # metadata points at Cargo.toml; we strip the filename with
    # parameter expansion in awk to keep jq-style filters simple.
    publishable_crates="$(
        cargo metadata --no-deps --format-version=1 2>/dev/null \
        | jq -r '.packages[]
                 | select(.source == null)
                 | select(.publish != [])
                 | "\(.name)\t\(.manifest_path)"' \
        | awk -F'\t' '{
            mp = $2
            sub(/\/Cargo\.toml$/, "", mp)
            printf "%s\t%s\n", $1, mp
        }'
    )"
    count=$(printf '%s' "$publishable_crates" | grep -c . || true)
    pass "enumerated $count publishable crate(s)"
fi

# --- 4. scan each publishable crate for pub enum compliance ---------
# For each publishable crate, walk every `.rs` file under `src/`
# and for every `pub enum` line, assert one of:
#   (a) the enum has `#[non_exhaustive]` on the preceding non-blank,
#       non-comment line;
#   (b) the enum has `#[allow(clippy::exhaustive_enums)]` on the
#       preceding non-blank line, and the line immediately before
#       that `#[allow]` is a `// reason:` line-comment;
#   (c) the crate's lib.rs (or main.rs) carries a crate-level
#       `#![allow(clippy::exhaustive_enums)]`.
#
# The awk below implements (a) and (b) via a small state machine
# that remembers the last 2 non-blank, non-test-mode-boundary lines.
# (c) is tested per-crate before entering the scan.
#
# Note: this is a backstop. If clippy is doing its job the scan
# is vacuously satisfied on the publishable set. The script's job
# is to make the invariant visible at the repository level —
# "I can audit the gate without running rustc."
scan_crate() {
    # $1 = crate name, $2 = crate dir
    local name="$1"
    local dir="$2"
    local src_dir="$dir/src"
    if [ ! -d "$src_dir" ]; then
        return 0
    fi

    # Check for crate-level allow in lib.rs OR main.rs.
    local root=""
    if [ -f "$src_dir/lib.rs" ]; then
        root="$src_dir/lib.rs"
    elif [ -f "$src_dir/main.rs" ]; then
        root="$src_dir/main.rs"
    fi
    if [ -n "$root" ] \
        && grep -qE '^[[:space:]]*#!\[allow\(clippy::exhaustive_enums\)\]' "$root"; then
        pass "crate-level escape at $root (crate $name)"
        return 0
    fi

    # Walk every .rs file under src/. For each `pub enum` line,
    # run the state-machine check.
    local files
    files="$(find "$src_dir" -type f -name '*.rs' -print | sort)"
    local file
    for file in $files; do
        awk -v FNAME="$file" '
            function emit_fail(msg) {
                printf "FAIL_LINE %s:%d: %s\n", FNAME, NR, msg
                fails++
            }
            function is_non_exhaustive(s) {
                return (s ~ /^[[:space:]]*#\[non_exhaustive\][[:space:]]*$/)
            }
            function is_exhaustive_allow(s) {
                return (s ~ /^[[:space:]]*#\[allow\([^)]*clippy::exhaustive_enums[^)]*\)\][[:space:]]*$/)
            }
            function is_reason_comment(s) {
                return (s ~ /^[[:space:]]*\/\/[[:space:]]*reason:/)
            }
            BEGIN { prev1 = ""; prev2 = ""; fails = 0 }
            # Track only `pub enum <Ident>` — not `pub(crate) enum`,
            # not `enum` (crate-private), not enum variant field
            # refs. The lint only fires on truly public enums.
            {
                if ($0 ~ /^[[:space:]]*pub[[:space:]]+enum[[:space:]]+[A-Za-z_]/) {
                    # Match option (a)
                    if (is_non_exhaustive(prev1)) {
                        # ok
                    } else if (is_exhaustive_allow(prev1)) {
                        # Match option (b): reason comment must be
                        # the line before the #[allow].
                        if (is_reason_comment(prev2)) {
                            # ok
                        } else {
                            emit_fail("#[allow(clippy::exhaustive_enums)] without `// reason:` line-comment on preceding line")
                        }
                    } else {
                        emit_fail("pub enum without #[non_exhaustive] or documented escape")
                    }
                }
                prev2 = prev1
                prev1 = $0
            }
            END { exit (fails > 0) ? 1 : 0 }
        ' "$file"
    done
}

if [ -n "$publishable_crates" ]; then
    # Split publishable_crates on newline, iterate.
    OLDIFS="$IFS"
    IFS='
'
    all_clean=1
    for row in $publishable_crates; do
        name=$(printf '%s' "$row" | cut -f1)
        path=$(printf '%s' "$row" | cut -f2)
        out="$(scan_crate "$name" "$path" 2>&1)"
        if printf '%s' "$out" | grep -q '^FAIL_LINE '; then
            fail "crate $name has pub enum(s) without #[non_exhaustive] or escape:"
            printf '%s\n' "$out" | grep '^FAIL_LINE ' | sed 's/^FAIL_LINE /    /'
            all_clean=0
        else
            # Echo any PASS lines emitted for crate-level escapes.
            printf '%s\n' "$out" | grep -E '^PASS  ' || true
        fi
    done
    IFS="$OLDIFS"
    if [ "$all_clean" = "1" ]; then
        pass "all publishable crates satisfy #[non_exhaustive] policy"
    fi
fi

# --- summary --------------------------------------------------------
echo
echo "$pass_count passed, $fail_count failed, $skip_count skipped"

if [ "$fail_count" -gt 0 ]; then
    exit 1
fi
exit 0
