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

# Repo root discovery. Default: resolve from the script's own location,
# so `bash scripts/non-exhaustive-check.sh` works from any CWD. The
# $NON_EXHAUSTIVE_REPO_ROOT env var overrides for the self-test's
# synthetic-tmpdir negative tests — see
# scripts/non-exhaustive-scripts-test.sh tests #9/#10.
if [ -n "${NON_EXHAUSTIVE_REPO_ROOT:-}" ]; then
    repo_root="$NON_EXHAUSTIVE_REPO_ROOT"
else
    repo_root="$(cd "$(dirname "$0")/.." && pwd)"
fi
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
#   (a) any line in the attribute/comment cluster immediately above
#       the `pub enum` is `#[non_exhaustive]`;
#   (b) any line in that cluster is
#       `#[allow(clippy::exhaustive_enums)]` AND the line immediately
#       preceding it in the cluster is a `// reason:` line-comment;
#   (c) the crate's lib.rs (or main.rs) carries a crate-level
#       `#![allow(clippy::exhaustive_enums)]`.
#
# Cluster definition (for (a) and (b)):
#   A cluster is a run of consecutive lines matching any of:
#     - `#[...]` outer attribute
#     - `///` or `//!` doc-comment
#     - `//` line-comment (including the `// reason:` form)
#   The cluster ends (resets) on:
#     - a blank line
#     - any non-attribute, non-comment content (item, brace, etc.)
#
# Cluster is stored in source order: cluster[0] is the top of the
# cluster (farthest from `pub enum`); cluster[n-1] is the line
# immediately above `pub enum`. The reason↔allow adjacency rule reads
# `cluster[i]` (the allow) and `cluster[i-1]` (the reason).
#
# Rationale for rewriting away from the prev1/prev2 scheme: that
# scheme false-rejected `#[derive(Debug)]\n#[non_exhaustive]\npub enum`,
# because `#[non_exhaustive]` landed on `prev2` behind `#[derive]` on
# `prev1` and prev2 was only checked as a `// reason:` candidate.
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
    # run the cluster-buffer check.
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
            # Accepts multi-lint allow forms like
            # `#[allow(clippy::exhaustive_enums, dead_code)]` — the
            # `[^)]*` on either side of `clippy::exhaustive_enums` lets
            # other lint names share the attribute call. Intentional:
            # real-world escapes often group related lints.
            function is_exhaustive_allow(s) {
                return (s ~ /^[[:space:]]*#\[allow\([^)]*clippy::exhaustive_enums[^)]*\)\][[:space:]]*$/)
            }
            function is_reason_comment(s) {
                return (s ~ /^[[:space:]]*\/\/[[:space:]]*reason:/)
            }
            function is_attr(s) {
                return (s ~ /^[[:space:]]*#\[/)
            }
            function is_line_comment(s) {
                # Matches `//`, `///`, `//!`, and `// reason:` — any
                # line-comment form. Block comments `/* */` reset the
                # cluster here even though rustc tolerates them between
                # attributes; backstops are allowed to be stricter than
                # the compiler, and no one writes block-comments in an
                # attribute cluster in practice.
                return (s ~ /^[[:space:]]*\/\//)
            }
            function is_blank(s) {
                return (s ~ /^[[:space:]]*$/)
            }
            function cluster_reset() {
                ncluster = 0
                delete cluster
            }
            function cluster_push(s) {
                cluster[ncluster] = s
                ncluster++
            }
            function cluster_scan_pub_enum() {
                # Returns 1 on accept, 0 on reject. Emits fail on 0.
                for (i = 0; i < ncluster; i++) {
                    if (is_non_exhaustive(cluster[i])) return 1
                }
                # No #[non_exhaustive] in the cluster; look for an
                # exhaustive_allow escape with a reason immediately
                # preceding it in the cluster (source-adjacent).
                for (i = 0; i < ncluster; i++) {
                    if (is_exhaustive_allow(cluster[i])) {
                        if (i >= 1 && is_reason_comment(cluster[i-1])) {
                            return 1
                        }
                        emit_fail("#[allow(clippy::exhaustive_enums)] without `// reason:` line-comment on preceding line")
                        return 0
                    }
                }
                emit_fail("pub enum without #[non_exhaustive] or documented escape")
                return 0
            }
            BEGIN { ncluster = 0; fails = 0 }
            # Track only `pub enum <Ident>` — not `pub(crate) enum`,
            # not `enum` (crate-private), not enum variant field
            # refs. The lint only fires on truly public enums.
            {
                if ($0 ~ /^[[:space:]]*pub[[:space:]]+enum[[:space:]]+[A-Za-z_]/) {
                    cluster_scan_pub_enum()
                    cluster_reset()
                } else if (is_blank($0)) {
                    cluster_reset()
                } else if (is_attr($0) || is_line_comment($0)) {
                    cluster_push($0)
                } else {
                    cluster_reset()
                }
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

# --- 5. MSRV tripwire for `// reason:` line-comments ----------------
# The `// reason:` line-comment convention exists because the inline
# `#[allow(lint, reason = "...")]` form is stable only from rustc 1.81.
# This tripwire is now ACTIVE: mango MSRV is at 1.89 (ADR 0003), and
# the inline form is available across the entire supported floor, so
# any surviving `// reason:` line-comment in a publishable crate's
# `src/**/*.rs` is a drift-from-policy and fails the check.
#
# Kept as a live rail (not deleted) because it also enforces the
# *forward* invariant: if anyone ever lowers MSRV below 1.81 again
# the comparison below falls through to no-op naturally, and on any
# supported MSRV it catches reintroductions of the workaround form.
#
# Scope is intentionally narrow: publishable `<manifest_dir>/src/**`
# only. Not `tests/`, not `examples/`, not `benches/`, not fixture
# workspaces (which are separate workspaces `cargo metadata --no-deps`
# on the root won't enumerate), not `docs/`.
msrv_raw=$(grep -E '^[[:space:]]*rust-version[[:space:]]*=[[:space:]]*"[0-9]+\.[0-9]+' "$cargo_toml" 2>/dev/null | head -1 || true)
if [ -n "$msrv_raw" ] && [ -n "$publishable_crates" ]; then
    # Extract major.minor with sed. Handles both "1.89" and "1.89.0".
    msrv_major=$(printf '%s' "$msrv_raw" | sed -nE 's/.*"([0-9]+)\.([0-9]+).*/\1/p')
    msrv_minor=$(printf '%s' "$msrv_raw" | sed -nE 's/.*"([0-9]+)\.([0-9]+).*/\2/p')
    if [ -n "$msrv_major" ] && [ -n "$msrv_minor" ]; then
        if [ "$msrv_major" -gt 1 ] || { [ "$msrv_major" -eq 1 ] && [ "$msrv_minor" -ge 81 ]; }; then
            scenario="MSRV tripwire: no \`// reason:\` line-comments in publishable src/ at MSRV >= 1.81"
            tripwire_hits=""
            OLDIFS="$IFS"
            IFS='
'
            for row in $publishable_crates; do
                path=$(printf '%s' "$row" | cut -f2)
                src="$path/src"
                if [ -d "$src" ]; then
                    hits=$(grep -rEn '^[[:space:]]*//[[:space:]]*reason:' "$src" 2>/dev/null || true)
                    if [ -n "$hits" ]; then
                        tripwire_hits="${tripwire_hits}${hits}
"
                    fi
                fi
            done
            IFS="$OLDIFS"
            if [ -n "$tripwire_hits" ]; then
                fail "$scenario"
                echo "    MSRV has advanced to ${msrv_major}.${msrv_minor}; the inline" >&2
                echo "    \`#[allow(lint, reason = \"...\")]\` form is stable — migrate:" >&2
                printf '%s' "$tripwire_hits" | sed 's/^/        /' >&2
            else
                pass "$scenario"
            fi
        fi
    fi
fi

# --- summary --------------------------------------------------------
echo
echo "$pass_count passed, $fail_count failed, $skip_count skipped"

if [ "$fail_count" -gt 0 ]; then
    exit 1
fi
exit 0
