#!/usr/bin/env bash
# scripts/ct-comparison-check.sh
#
# Scan `auth*` / `crypto*` / `token*` / `hash_chain*` source files
# for byte-compare patterns that produce timing oracles (P1-P5 in
# docs/ct-comparison-policy.md), and enforce the label-gated escape
# hatch for `// ct-allow:` annotations.
#
# Usage:
#   bash scripts/ct-comparison-check.sh
#   bash scripts/ct-comparison-check.sh --list-scope
#
# The script reads three environment variables to adapt to the CI
# event context (set by `.github/workflows/ct-comparison.yml`):
#
#   GITHUB_EVENT_NAME   "pull_request" | "push" | "merge_group" | ""
#   GITHUB_BASE_REF     PR base branch name (only meaningful on pull_request)
#   PR_LABELS           JSON array of label names (set only on pull_request)
#
# All three are optional for local use.
#
# Exit codes:
#   0  PASS — no violations, or every violation is annotated, or new
#             annotations carry the `ct-allow-approved` label
#   1  FAIL — unannotated P1-P5 match in a scoped file
#   2  FAIL — PR adds new `// ct-allow:` annotations without the
#             `ct-allow-approved` label
#   3  FAIL — `.ct-comparison-ignore` names a file that does not exist
#
# Requires: bash, awk (POSIX), find, git (only on pull_request), jq.

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

event="${GITHUB_EVENT_NAME:-}"
base_ref="${GITHUB_BASE_REF:-}"
pr_labels_raw="${PR_LABELS:-[]}"

list_scope_only=0
if [ "${1:-}" = "--list-scope" ]; then
    list_scope_only=1
elif [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    sed -n '2,/^set -/p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
elif [ -n "${1:-}" ]; then
    printf 'error: unknown argument: %s\n' "$1" >&2
    exit 64
fi

# ---------------------------------------------------------------------
# 1. Validate .ct-comparison-ignore (if present)
# ---------------------------------------------------------------------
ignore_file=".ct-comparison-ignore"
ignore_entries=""
if [ -f "$ignore_file" ]; then
    while IFS= read -r raw; do
        # strip comments and whitespace
        line="${raw%%#*}"
        line="$(printf '%s' "$line" | awk '{$1=$1;print}')"
        [ -z "$line" ] && continue
        if [ ! -f "$line" ]; then
            printf 'error: .ct-comparison-ignore references missing file: %s\n' "$line" >&2
            printf '       either remove this entry, or restore the file.\n' >&2
            exit 3
        fi
        ignore_entries="$ignore_entries $line"
    done < "$ignore_file"
fi

# ---------------------------------------------------------------------
# 2. Build scope set
# ---------------------------------------------------------------------
scope_re='(^|/)(auth|crypto|token|hash_chain)([^/]*\.rs$|[^/]*/)'

# emit all *.rs under crates/, excluding test paths, then filter by
# scope_re and subtract ignore entries.
build_scope() {
    find crates -type f -name '*.rs' 2>/dev/null \
        | awk -v re="$scope_re" '
            {
                # exclude test paths
                if ($0 ~ /\/tests\//) next
                if ($0 ~ /_test\.rs$/) next
                if ($0 ~ /_tests\.rs$/) next
                # scope match: any path component after "crates/*/src/"
                # starting with one of the 4 prefixes
                if ($0 ~ re) print
            }' \
        | while IFS= read -r f; do
            skip=0
            for ig in $ignore_entries; do
                if [ "$f" = "$ig" ]; then skip=1; break; fi
            done
            [ $skip -eq 0 ] && printf '%s\n' "$f"
          done \
        | sort -u
}

scope="$(build_scope || true)"

if [ "$list_scope_only" -eq 1 ]; then
    if [ -z "$scope" ]; then
        printf 'scope is empty.\n'
        exit 2
    fi
    printf '%s\n' "$scope"
    exit 0
fi

# ---------------------------------------------------------------------
# 3. Scan each scoped file for P1-P5 patterns
# ---------------------------------------------------------------------
violations_tmp="$(mktemp)"
trap 'rm -f "$violations_tmp"' EXIT

scan_awk='
BEGIN {
    suffix_re = "_(hash|hmac|mac|tag|token|digest|signature|nonce|secret|key)"
    sec_type_re = "^(Token|Hmac|Mac|Tag|Secret|Password|Key|Credential|Nonce|Digest|Signature)"
    in_derive = 0
    derive_buf = ""
    derive_has_pe = 0
    awaiting_type = 0
    violations = 0
}

function find_comment_col(s,    i, L) {
    L = length(s)
    for (i = 1; i < L; i++) {
        if (substr(s, i, 2) == "//") return i
    }
    return 0
}

function has_annotation(line,    c, comment) {
    c = find_comment_col(line)
    if (c == 0) return 0
    comment = substr(line, c)
    if (match(comment, /\/\/[[:space:]]*ct-allow:/)) return 1
    return 0
}

function code_portion(line,    c) {
    c = find_comment_col(line)
    if (c == 0) return line
    return substr(line, 1, c - 1)
}

function emit(lineno, tag, origline) {
    printf "%s:%d: [%s] %s\n", FILENAME, lineno, tag, origline
    violations++
}

{
    origline = $0
    ann = has_annotation(origline)
    code = code_portion(origline)

    # ---- P4: multi-line derive state machine ----
    if (in_derive) {
        derive_buf = derive_buf " " code
        if (index(code, ")]") > 0) {
            if (match(derive_buf, /PartialEq/) \
                || match(derive_buf, /(^|[^A-Za-z0-9_])Eq([^A-Za-z0-9_]|$)/)) {
                derive_has_pe = 1
            } else {
                derive_has_pe = 0
            }
            in_derive = 0
            awaiting_type = derive_has_pe
            derive_buf = ""
        }
        next
    }

    if (match(code, /#\[derive\(/)) {
        in_derive = 1
        derive_buf = code
        if (index(code, ")]") > 0) {
            if (match(derive_buf, /PartialEq/) \
                || match(derive_buf, /(^|[^A-Za-z0-9_])Eq([^A-Za-z0-9_]|$)/)) {
                derive_has_pe = 1
            } else {
                derive_has_pe = 0
            }
            in_derive = 0
            awaiting_type = derive_has_pe
            derive_buf = ""
        }
        next
    }

    if (awaiting_type) {
        if (match(code, /(struct|enum)[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/)) {
            decl = substr(code, RSTART, RLENGTH)
            sub(/^(struct|enum)[[:space:]]+/, "", decl)
            if (match(decl, sec_type_re)) {
                tname = substr(decl, RSTART, RLENGTH)
                if (!ann) {
                    emit(NR, "P4 derive(PartialEq)/Eq on secret-named type " tname, origline)
                }
            }
            awaiting_type = 0
        }
    }

    # ---- P1: byte-literal compare ----
    if (match(code, /(==|!=)[[:space:]]*b"/) \
        || match(code, /b"[^"]*"[[:space:]]*(==|!=)/)) {
        if (!ann) { emit(NR, "P1 byte-literal compare", origline); next }
    }

    # ---- P2: method-form byte compare ----
    # P2a: <secret-suffix-ident>.eq(  /  .ne(
    # P2b: .as_bytes().eq(  /  .ne(
    # P2c: .eq(b"…")  /  .ne(b"…")  /  .eq(&b"…")
    if (match(code, "[A-Za-z_][A-Za-z0-9_]*" suffix_re "\\.(eq|ne)\\(") \
        || match(code, /\.as_bytes\(\)\.(eq|ne)\(/) \
        || match(code, /\.(eq|ne)\([[:space:]]*&?[[:space:]]*b"/)) {
        if (!ann) { emit(NR, "P2 method-form byte compare", origline); next }
    }

    # ---- P3: .as_bytes() in a compare ----
    if (match(code, /\.as_bytes\(\)/) && match(code, /(==|!=)/)) {
        if (!ann) { emit(NR, "P3 .as_bytes() compare", origline); next }
    }

    # ---- P5: secret-suffix identifier in ==/!= ----
    # Requires both the operator and a secret-suffix word in the code.
    if (match(code, /(==|!=)/) \
        && match(code, "[A-Za-z_][A-Za-z0-9_]*" suffix_re "([^A-Za-z0-9_]|$)")) {
        if (!ann) { emit(NR, "P5 secret-named identifier in compare", origline); next }
    }
}

END {
    exit (violations > 0) ? 1 : 0
}
'

total_violations=0
scanned_files=0
if [ -n "$scope" ]; then
    # iterate files (one per line in $scope)
    OLDIFS="$IFS"
    IFS='
'
    for f in $scope; do
        scanned_files=$((scanned_files + 1))
        awk "$scan_awk" "$f" >> "$violations_tmp" || true
    done
    IFS="$OLDIFS"
fi

total_violations="$(wc -l < "$violations_tmp" | awk '{print $1}')"

# ---------------------------------------------------------------------
# 4. PR-mode new-annotation label check
# ---------------------------------------------------------------------
# Detect new `// ct-allow:` annotations added in this PR (vs base),
# using set-difference on (file, normalized-reason) keys. Rustfmt
# reflows and line-number drift don't trigger false "new annotation"
# readings.
#
# Only run on pull_request events with a base_ref we can resolve.
new_annotations_count=0
new_annotations_tmp="$(mktemp)"
trap 'rm -f "$violations_tmp" "$new_annotations_tmp"' EXIT

extract_annotations_from_tree() {
    # $1 = git tree-ish ("HEAD" or "origin/$base_ref") or "__worktree__"
    # Outputs: one line per annotation: "file|normalized-reason"
    local treeish="$1"
    local files
    if [ "$treeish" = "__worktree__" ]; then
        # Use scope as-is from working tree.
        files="$scope"
        for f in $files; do
            awk -v F="$f" '
                {
                    # only lines that contain the annotation marker
                    if (match($0, /\/\/[[:space:]]*ct-allow:[[:space:]]*(.*)$/)) {
                        reason = substr($0, RSTART, RLENGTH)
                        sub(/^\/\/[[:space:]]*ct-allow:[[:space:]]*/, "", reason)
                        gsub(/[[:space:]]+/, " ", reason)
                        sub(/[[:space:]]+$/, "", reason)
                        printf "%s|%s\n", F, reason
                    }
                }' "$f" 2>/dev/null || true
        done
    else
        # List files in the tree matching our scope set and extract.
        # Use git ls-tree for the tree, re-apply scope filter, use
        # `git show treeish:file` to read contents.
        local tree_files
        tree_files="$(git ls-tree -r --name-only "$treeish" -- 'crates/' 2>/dev/null \
            | awk -v re="$scope_re" '
                { if ($0 ~ /\/tests\//) next
                  if ($0 ~ /_test\.rs$/) next
                  if ($0 ~ /_tests\.rs$/) next
                  if ($0 !~ /\.rs$/) next
                  if ($0 ~ re) print }' \
            | sort -u)"
        local f
        for f in $tree_files; do
            # skip ignored
            local skip=0
            for ig in $ignore_entries; do
                if [ "$f" = "$ig" ]; then skip=1; break; fi
            done
            [ "$skip" -eq 1 ] && continue
            git show "$treeish:$f" 2>/dev/null \
                | awk -v F="$f" '
                    {
                        if (match($0, /\/\/[[:space:]]*ct-allow:[[:space:]]*(.*)$/)) {
                            reason = substr($0, RSTART, RLENGTH)
                            sub(/^\/\/[[:space:]]*ct-allow:[[:space:]]*/, "", reason)
                            gsub(/[[:space:]]+/, " ", reason)
                            sub(/[[:space:]]+$/, "", reason)
                            printf "%s|%s\n", F, reason
                        }
                    }' || true
        done
    fi
}

do_pr_check=0
if [ "$event" = "pull_request" ] && [ -n "$base_ref" ]; then
    if git rev-parse --verify "origin/$base_ref" >/dev/null 2>&1; then
        do_pr_check=1
    fi
fi

if [ "$do_pr_check" -eq 1 ]; then
    head_anns="$(mktemp)"
    base_anns="$(mktemp)"
    trap 'rm -f "$violations_tmp" "$new_annotations_tmp" "$head_anns" "$base_anns"' EXIT

    extract_annotations_from_tree "__worktree__" | sort -u > "$head_anns"
    extract_annotations_from_tree "origin/$base_ref" | sort -u > "$base_anns"

    # Set difference: lines in HEAD not in BASE
    comm -23 "$head_anns" "$base_anns" > "$new_annotations_tmp"
    new_annotations_count="$(wc -l < "$new_annotations_tmp" | awk '{print $1}')"
fi

# ---------------------------------------------------------------------
# 5. Decide outcome
# ---------------------------------------------------------------------
printf '=== ct-comparison gate ===\n'
printf 'scope: %d scoped files\n' "$scanned_files"
printf 'violations: %d\n' "$total_violations"

if [ "$total_violations" -gt 0 ]; then
    printf '\nunannotated violations:\n'
    cat "$violations_tmp"
    printf '\nsee docs/ct-comparison-policy.md for remediation.\n'
    summary_write() {
        [ -n "${GITHUB_STEP_SUMMARY:-}" ] || return 0
        {
            printf '### ct-comparison: FAIL\n\n'
            printf '%d violation(s) in scoped files.\n\n' "$total_violations"
            printf '```\n'
            cat "$violations_tmp"
            printf '```\n'
        } >> "$GITHUB_STEP_SUMMARY"
    }
    summary_write
    exit 1
fi

if [ "$new_annotations_count" -gt 0 ]; then
    # Parse PR labels from JSON array
    has_label=0
    if printf '%s' "$pr_labels_raw" | jq -e 'if type == "array" then any(. == "ct-allow-approved") else false end' >/dev/null 2>&1; then
        has_label=1
    fi
    if [ "$has_label" -eq 0 ]; then
        printf '\nnew ct-allow annotations added without `ct-allow-approved` label:\n'
        awk -F'|' '{ printf "  %s: %s\n", $1, $2 }' "$new_annotations_tmp"
        printf '\nadd the `ct-allow-approved` label to the PR if the annotations are justified.\n'
        if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
            {
                printf '### ct-comparison: FAIL (missing label)\n\n'
                printf '%d new ct-allow annotation(s) without `ct-allow-approved` label.\n\n' "$new_annotations_count"
            } >> "$GITHUB_STEP_SUMMARY"
        fi
        exit 2
    fi
    printf '\nnew ct-allow annotations approved via `ct-allow-approved` label (%d).\n' "$new_annotations_count"
fi

printf 'PASS\n'
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
        printf '### ct-comparison: PASS\n\n'
        printf '| metric | value |\n|---|---|\n'
        printf '| scoped files | %d |\n' "$scanned_files"
        printf '| violations | 0 |\n'
        printf '| new annotations | %d |\n' "$new_annotations_count"
    } >> "$GITHUB_STEP_SUMMARY"
fi
exit 0
