#!/usr/bin/env bash
# scripts/ct-comparison-scripts-test.sh
#
# Test harness for scripts/ct-comparison-check.sh.
#
# Each scenario sets up a synthetic repo in a mktemp'd dir, copies
# the check script in, writes fixture files inline via heredocs,
# invokes the script, and asserts exit code + stdout substring.
#
# The fixtures live inline so there is no separate fixture tree to
# drift out of sync — the test IS the fixture specification.
#
# Scenarios cover P1-P5 detection, annotation-accept, commented-out
# code skip, negative cases (enum/Duration compares that must NOT
# flag), scope exclusions, PR-mode label flow, and
# .ct-comparison-ignore drift.

set -u

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
check_script="$repo_root/scripts/ct-comparison-check.sh"

if [ ! -f "$check_script" ]; then
    printf 'error: check script not found at %s\n' "$check_script" >&2
    exit 2
fi

pass_count=0
fail_count=0
skip_count=0

# ---------------------------------------------------------------------
# assertion helpers
# ---------------------------------------------------------------------
# usage: run_check <workdir> [env-setup]
#   Runs the check script in <workdir> (cd first).
#   Captures stdout+stderr in $RUN_OUT and exit in $RUN_EXIT.
RUN_OUT=""
RUN_EXIT=0
run_check() {
    local workdir="$1"
    shift
    local out
    out="$(cd "$workdir" && env "$@" bash "$workdir/scripts/ct-comparison-check.sh" 2>&1)"
    RUN_EXIT=$?
    RUN_OUT="$out"
}

# same but with --list-scope
run_check_list_scope() {
    local workdir="$1"
    local out
    out="$(cd "$workdir" && bash "$workdir/scripts/ct-comparison-check.sh" --list-scope 2>&1)"
    RUN_EXIT=$?
    RUN_OUT="$out"
}

assert_exit() {
    local scenario="$1" want="$2"
    if [ "$RUN_EXIT" -ne "$want" ]; then
        printf '  FAIL [exit %d, want %d]\n' "$RUN_EXIT" "$want"
        printf '  --- output:\n%s\n  ---\n' "$RUN_OUT"
        fail_count=$((fail_count + 1))
        return 1
    fi
    return 0
}

assert_contains() {
    local scenario="$1" needle="$2"
    if ! printf '%s' "$RUN_OUT" | grep -qF -- "$needle"; then
        printf '  FAIL [stdout missing: %s]\n' "$needle"
        printf '  --- output:\n%s\n  ---\n' "$RUN_OUT"
        fail_count=$((fail_count + 1))
        return 1
    fi
    return 0
}

pass() {
    printf '  PASS\n'
    pass_count=$((pass_count + 1))
}

setup_workdir() {
    local d
    d="$(mktemp -d -t ct-cmp-test.XXXXXX)"
    mkdir -p "$d/scripts"
    cp "$check_script" "$d/scripts/ct-comparison-check.sh"
    chmod +x "$d/scripts/ct-comparison-check.sh"
    printf '%s' "$d"
}

cleanup_workdir() {
    local d="$1"
    [ -n "$d" ] && [ -d "$d" ] && rm -rf "$d"
}

# Set up a synthetic git repo with origin/main as the base.
# Call AFTER writing the base tree; this commits it to origin/main.
# Then further edits land on a feature branch.
setup_pr_repo() {
    local d="$1"
    (cd "$d" \
        && git init -q \
        && git config user.email 'ct@mango.test' \
        && git config user.name 'ct-test' \
        && git checkout -q -b main \
        && git add -A \
        && git commit -q -m 'base' \
        && git clone -q --bare . .bare.git >/dev/null 2>&1 \
        && git remote add origin ./.bare.git \
        && git fetch -q origin \
        && git checkout -q -b feature
    ) >/dev/null 2>&1
}

# ---------------------------------------------------------------------
# scenario 1: empty scope (no scoped files under crates/)
# ---------------------------------------------------------------------
printf 'scenario 01: empty scope -> PASS\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/lib.rs" <<'EOF'
pub fn add(a: u32, b: u32) -> u32 { a.wrapping_add(b) }
EOF
run_check "$d"
assert_exit 01 0 && assert_contains 01 "PASS" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 2: scoped file using ct_eq only -> PASS
# ---------------------------------------------------------------------
printf 'scenario 02: clean ct_eq -> PASS\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
use subtle::ConstantTimeEq;
pub fn verify(a: &[u8; 32], b: &[u8; 32]) -> bool {
    bool::from(a.ct_eq(b))
}
EOF
run_check "$d"
assert_exit 02 0 && assert_contains 02 "PASS" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 3: P1 byte-literal compare -> FAIL exit 1
# ---------------------------------------------------------------------
printf 'scenario 03: P1 byte-literal -> FAIL 1\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
pub fn is_admin(name: &[u8]) -> bool {
    name == b"admin"
}
EOF
run_check "$d"
assert_exit 03 1 && assert_contains 03 "P1 byte-literal" && assert_contains 03 "auth.rs:2" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 4: P1 annotated with ct-allow -> PASS
# ---------------------------------------------------------------------
printf 'scenario 04: P1 annotated -> PASS\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
pub fn is_admin_literal(name: &[u8]) -> bool {
    name == b"admin" // ct-allow: role-name compare, not a secret
}
EOF
run_check "$d"
assert_exit 04 0 && assert_contains 04 "PASS" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 5: P2 secret-suffix receiver .eq( -> FAIL 1
# ---------------------------------------------------------------------
printf 'scenario 05: P2 suffix-receiver .eq -> FAIL 1\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/crypto.rs" <<'EOF'
pub fn check(session_hmac: &[u8], expected: &[u8]) -> bool {
    session_hmac.eq(expected)
}
EOF
run_check "$d"
assert_exit 05 1 && assert_contains 05 "P2 method-form" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 6: P3 .as_bytes() compare -> FAIL 1
# ---------------------------------------------------------------------
printf 'scenario 06: P3 as_bytes compare -> FAIL 1\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/token.rs" <<'EOF'
pub fn matches(tok: &str, expected: &[u8]) -> bool {
    tok.as_bytes() == expected
}
EOF
run_check "$d"
assert_exit 06 1 && assert_contains 06 "P3 .as_bytes" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 7: P4 multi-line derive(PartialEq) on Token -> FAIL 1
# ---------------------------------------------------------------------
printf 'scenario 07: P4 multiline-derive -> FAIL 1\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
#[derive(
    Debug,
    Clone,
    PartialEq,
)]
pub struct TokenTag {
    pub bytes: [u8; 32],
}
EOF
run_check "$d"
assert_exit 07 1 && assert_contains 07 "P4 derive" && assert_contains 07 "TokenTag" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 8: P5 secret-suffix identifier in == -> FAIL 1
# ---------------------------------------------------------------------
printf 'scenario 08: P5 suffix-ident in == -> FAIL 1\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/hash_chain.rs" <<'EOF'
pub fn matches(prev: &[u8], computed_hash: &[u8]) -> bool {
    computed_hash == prev
}
EOF
run_check "$d"
assert_exit 08 1 && assert_contains 08 "P5" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 9: commented-out code "// foo == bar" -> PASS (not flagged)
# ---------------------------------------------------------------------
printf 'scenario 09: commented-out == -> PASS\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
pub fn dummy() -> u32 {
    // old: let x = secret_hmac == received;
    42
}
EOF
run_check "$d"
assert_exit 09 0 && assert_contains 09 "PASS" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 10: negative — role.eq(&Role::Admin) in scoped file -> PASS
# ---------------------------------------------------------------------
printf 'scenario 10: negative role.eq -> PASS\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
#[derive(Debug, PartialEq)]
pub enum Role { Admin, User }

pub fn is_admin(role: &Role) -> bool {
    role.eq(&Role::Admin)
}
EOF
run_check "$d"
assert_exit 10 0 && assert_contains 10 "PASS" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 11: negative — enum/Duration compare in scoped file -> PASS
# ---------------------------------------------------------------------
printf 'scenario 11: negative enum/duration == -> PASS\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/token.rs" <<'EOF'
use std::time::Duration;

pub enum Color { Red, Blue }

pub fn pick(c: Color, d: Duration) -> u32 {
    if matches!(c, Color::Red) && d == Duration::ZERO {
        1
    } else if d != Duration::from_secs(30) {
        2
    } else {
        3
    }
}
EOF
run_check "$d"
assert_exit 11 0 && assert_contains 11 "PASS" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 12: --list-scope on non-empty scope -> exit 0, files listed
# ---------------------------------------------------------------------
printf 'scenario 12: --list-scope non-empty -> 0\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
pub fn stub() -> u32 { 0 }
EOF
cat > "$d/crates/mango/src/not_scoped.rs" <<'EOF'
pub fn stub() -> u32 { 0 }
EOF
run_check_list_scope "$d"
assert_exit 12 0 && assert_contains 12 "auth.rs" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 13: non-scoped file with P1 patterns -> PASS (out of scope)
# ---------------------------------------------------------------------
printf 'scenario 13: non-scoped file has ==b"" -> PASS\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/unrelated.rs" <<'EOF'
pub fn is_admin(name: &[u8]) -> bool {
    let computed_hmac = name;
    name == b"admin" && computed_hmac != b""
}
EOF
run_check "$d"
assert_exit 13 0 && assert_contains 13 "PASS" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 14: PR mode — new ct-allow annotation without label -> exit 2
# ---------------------------------------------------------------------
printf 'scenario 14: PR new-annot no label -> 2\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
pub fn stub() -> u32 { 0 }
EOF
setup_pr_repo "$d"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
pub fn is_admin(name: &[u8]) -> bool {
    name == b"admin" // ct-allow: role-name compare, not a secret
}
EOF
run_check "$d" GITHUB_EVENT_NAME=pull_request GITHUB_BASE_REF=main PR_LABELS='[]'
assert_exit 14 2 && assert_contains 14 "ct-allow-approved" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 15: PR mode — new ct-allow annotation WITH label -> 0
# ---------------------------------------------------------------------
printf 'scenario 15: PR new-annot with label -> 0\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
pub fn stub() -> u32 { 0 }
EOF
setup_pr_repo "$d"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
pub fn is_admin(name: &[u8]) -> bool {
    name == b"admin" // ct-allow: role-name compare, not a secret
}
EOF
run_check "$d" GITHUB_EVENT_NAME=pull_request GITHUB_BASE_REF=main PR_LABELS='["ct-allow-approved"]'
assert_exit 15 0 && assert_contains 15 "PASS" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 16: PR mode — reformatted (not new) ct-allow -> 0
# BASE has `x == y // ct-allow: reason-a`; HEAD has same annotation
# but line-moved within file and whitespace-normalized. No label.
# Must NOT trip the new-annotation check.
# ---------------------------------------------------------------------
printf 'scenario 16: PR reformatted annot no label -> 0\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
pub fn a() -> bool {
    let x = b"a";
    let y = b"b";
    x == y // ct-allow: literal compare for scenario 16 test
}
EOF
setup_pr_repo "$d"
# HEAD: insert a blank line before, and double-space before the comment
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
pub fn a() -> bool {

    let x = b"a";
    let y = b"b";
    x == y  // ct-allow: literal compare for scenario 16 test
}
EOF
run_check "$d" GITHUB_EVENT_NAME=pull_request GITHUB_BASE_REF=main PR_LABELS='[]'
assert_exit 16 0 && assert_contains 16 "PASS" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 17: .ct-comparison-ignore references missing file -> exit 3
# ---------------------------------------------------------------------
printf 'scenario 17: stale .ct-comparison-ignore -> 3\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/auth.rs" <<'EOF'
pub fn stub() -> u32 { 0 }
EOF
cat > "$d/.ct-comparison-ignore" <<'EOF'
# Stale entry — this file does not exist
crates/mango/src/does_not_exist.rs
EOF
run_check "$d"
assert_exit 17 3 && assert_contains 17 "references missing file" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# scenario 18: test-path exemption — file ends in _test.rs, not scoped
# ---------------------------------------------------------------------
printf 'scenario 18: _test.rs path exemption -> PASS\n'
d="$(setup_workdir)"
mkdir -p "$d/crates/mango/src"
cat > "$d/crates/mango/src/auth_test.rs" <<'EOF'
pub fn check() -> bool {
    let secret_key = b"abc";
    secret_key == b"abc"
}
EOF
run_check "$d"
assert_exit 18 0 && assert_contains 18 "PASS" && pass
cleanup_workdir "$d"

# ---------------------------------------------------------------------
# summary
# ---------------------------------------------------------------------
printf '\n====================\n'
printf 'PASS: %d   FAIL: %d   SKIP: %d\n' "$pass_count" "$fail_count" "$skip_count"

if [ "$fail_count" -gt 0 ]; then
    exit 1
fi
exit 0
