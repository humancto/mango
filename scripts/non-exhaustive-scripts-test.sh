#!/usr/bin/env bash
# scripts/non-exhaustive-scripts-test.sh
#
# Self-test harness for the `#[non_exhaustive]` policy gate.
#
# Two layers under test:
#
#   1. Structural backstop — `scripts/non-exhaustive-check.sh`
#      asserts workspace lint entry + per-enum escape discipline on
#      every publishable crate. This script runs the backstop and
#      verifies it both passes on the real tree AND fails on a bad
#      fixture (negative test).
#
#   2. Clippy regression fixture —
#      `tests/fixtures/non-exhaustive/` is a self-contained
#      3-member workspace (`compliant`, `bad`, `allowed`). This
#      script runs `cargo clippy -- -D clippy::exhaustive_enums`
#      against each member and asserts the expected outcome. Locks
#      in clippy's behavior on the lint against restriction-
#      category drift across clippy point releases.
#
# CI vs local:
#   $CI: missing `cargo` fails hard. Locally: skip with install hint.
#
# Invocation:
#   bash scripts/non-exhaustive-scripts-test.sh

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

# --- 1. policy doc exists -------------------------------------------
scenario="policy doc exists at docs/api-stability.md"
if [ -f "docs/api-stability.md" ]; then
    pass "$scenario"
else
    fail "$scenario"
fi

# --- 2. workspace lint entry in Cargo.toml --------------------------
scenario="Cargo.toml declares exhaustive_enums = deny"
if grep -qE '^[[:space:]]*exhaustive_enums[[:space:]]*=[[:space:]]*\{[[:space:]]*level[[:space:]]*=[[:space:]]*"deny"' Cargo.toml; then
    pass "$scenario"
else
    fail "$scenario"
fi

# --- 3. backstop script is executable -------------------------------
scenario="scripts/non-exhaustive-check.sh is executable"
if [ -x "scripts/non-exhaustive-check.sh" ]; then
    pass "$scenario"
else
    fail "$scenario"
fi

# --- 4. backstop passes on the real tree ----------------------------
scenario="scripts/non-exhaustive-check.sh passes on current tree"
if bash scripts/non-exhaustive-check.sh >/dev/null 2>&1; then
    pass "$scenario"
else
    fail "$scenario"
    # Re-run to surface output for debugging.
    bash scripts/non-exhaustive-check.sh || true
fi

# --- 5. fixture workspace exists ------------------------------------
scenario="fixture workspace at tests/fixtures/non-exhaustive/"
fixture_root="tests/fixtures/non-exhaustive"
if [ -d "$fixture_root" ] \
    && [ -f "$fixture_root/Cargo.toml" ] \
    && [ -f "$fixture_root/compliant/src/lib.rs" ] \
    && [ -f "$fixture_root/bad/src/lib.rs" ] \
    && [ -f "$fixture_root/allowed/src/lib.rs" ]; then
    pass "$scenario"
else
    fail "$scenario (expected Cargo.toml + compliant/bad/allowed members)"
fi

# --- 6. fixture oracle: clippy accepts `compliant` ------------------
# Uses `-D clippy::exhaustive_enums` directly on the command line
# rather than relying on the fixture's own lint table (the fixture
# has none). This matches how the upstream workspace uses the lint
# via `[workspace.lints.clippy]` but keeps the fixture dependency-
# free. See tests/fixtures/non-exhaustive/Cargo.toml.
scenario="fixture oracle: clippy accepts compliant (#[non_exhaustive])"
if ! command -v cargo >/dev/null 2>&1; then
    missing_tool "$scenario" "cargo" "install rustup + cargo"
else
    if ( cd "$fixture_root" && cargo clippy --package compliant --quiet -- -D clippy::exhaustive_enums >/dev/null 2>&1 ); then
        pass "$scenario"
    else
        fail "$scenario"
    fi
fi

# --- 7. fixture oracle: clippy accepts `allowed` --------------------
scenario="fixture oracle: clippy accepts allowed (escape with // reason:)"
if ! command -v cargo >/dev/null 2>&1; then
    missing_tool "$scenario" "cargo" "install rustup + cargo"
else
    if ( cd "$fixture_root" && cargo clippy --package allowed --quiet -- -D clippy::exhaustive_enums >/dev/null 2>&1 ); then
        pass "$scenario"
    else
        fail "$scenario"
    fi
fi

# --- 7.5. fixture oracle: clippy accepts `allowed-derive-after` -----
# Covers the attribute-cluster case: #[derive(...)] immediately above
# #[non_exhaustive] above pub enum. Clippy accepts (attribute order on
# the enum is semantically free). The backstop's awk must also accept
# — the pre-cluster prev1/prev2 scheme false-rejected this shape.
scenario="fixture oracle: clippy accepts allowed-derive-after (#[derive] + #[non_exhaustive])"
if ! command -v cargo >/dev/null 2>&1; then
    missing_tool "$scenario" "cargo" "install rustup + cargo"
else
    if ( cd "$fixture_root" && cargo clippy --package allowed-derive-after --quiet -- -D clippy::exhaustive_enums >/dev/null 2>&1 ); then
        pass "$scenario"
    else
        fail "$scenario"
    fi
fi

# --- 8. fixture oracle: clippy rejects `bad` ------------------------
# If this ever starts passing, either the lint is not firing (point-
# release regression) or the fixture's `bad/src/lib.rs` got silently
# edited. Both are exactly what the self-test is here to catch.
scenario="fixture oracle: clippy rejects bad (bare pub enum)"
if ! command -v cargo >/dev/null 2>&1; then
    missing_tool "$scenario" "cargo" "install rustup + cargo"
else
    if ( cd "$fixture_root" && cargo clippy --package bad --quiet -- -D clippy::exhaustive_enums >/dev/null 2>&1 ); then
        fail "$scenario (clippy accepted a bare pub enum — lint regression!)"
    else
        pass "$scenario"
    fi
fi

# --- 9. backstop negative test --------------------------------------
# The backstop script must reject a publishable crate with a naked
# pub enum. We don't touch the real tree — we construct a synthetic
# mini-workspace in a tmpdir and point the script at it via the
# $NON_EXHAUSTIVE_REPO_ROOT env var (see scripts/non-exhaustive-check.sh).
#
# This exercises the awk cluster buffer that the on-tree test in
# step 4 cannot reach (because the real tree is clean today). If
# someone breaks the cluster scan, this test catches it.
scenario="backstop negative test: rejects naked pub enum"
tmpdir="$(mktemp -d)"
cleanup_tmp() { rm -rf "$tmpdir"; }
trap cleanup_tmp EXIT

cat > "$tmpdir/Cargo.toml" <<'TOML'
[workspace]
resolver = "2"
members = ["naked"]

[workspace.lints.clippy]
exhaustive_enums = { level = "deny", priority = 1 }
TOML
mkdir -p "$tmpdir/naked/src"
cat > "$tmpdir/naked/Cargo.toml" <<'TOML'
[package]
name = "naked"
version = "0.0.0"
edition = "2021"
# No publish=false on purpose — this crate must count as publishable.

[lib]
path = "src/lib.rs"
TOML
# Stub policy doc so the check script's "policy doc exists" assertion
# is satisfied. Orthogonal to the awk cluster scan we're testing here.
mkdir -p "$tmpdir/docs"
: > "$tmpdir/docs/api-stability.md"

cat > "$tmpdir/naked/src/lib.rs" <<'RS'
pub enum Naked { A, B }
RS

# Expect the check to flag the naked enum with a FAIL_LINE mention.
if NON_EXHAUSTIVE_REPO_ROOT="$tmpdir" bash scripts/non-exhaustive-check.sh 2>&1 \
    | grep -q 'pub enum without #\[non_exhaustive\]'; then
    pass "$scenario"
else
    fail "$scenario (backstop did not flag the synthetic naked enum)"
    NON_EXHAUSTIVE_REPO_ROOT="$tmpdir" bash scripts/non-exhaustive-check.sh 2>&1 || true
fi

# --- 10. backstop negative test: allow without reason ---------------
# `#[allow(clippy::exhaustive_enums)]` without a preceding
# `// reason:` comment must be rejected. Distinct code path from #9.
scenario="backstop negative test: rejects #[allow] without // reason:"
cat > "$tmpdir/naked/src/lib.rs" <<'RS'
#[allow(clippy::exhaustive_enums)]
pub enum NoReason { A, B }
RS

if NON_EXHAUSTIVE_REPO_ROOT="$tmpdir" bash scripts/non-exhaustive-check.sh 2>&1 \
    | grep -q 'line-comment on preceding line'; then
    pass "$scenario"
else
    fail "$scenario (backstop accepted an #[allow] without // reason:)"
    NON_EXHAUSTIVE_REPO_ROOT="$tmpdir" bash scripts/non-exhaustive-check.sh 2>&1 || true
fi

# --- 11. backstop positive test: #[derive] + #[non_exhaustive] ------
# The cluster rewrite must accept `#[derive(Debug)] #[non_exhaustive]
# pub enum X` — the shape the pre-cluster prev1/prev2 scheme false-
# rejected. Positive oracle for the awk cluster buffer rewrite. Without
# this assertion, the rewrite is only dynamically verified by "the real
# tree still passes", which is a weak oracle (the real tree's enums are
# all bare `#[non_exhaustive]` today).
scenario="backstop positive test: accepts #[derive] + #[non_exhaustive]"
cat > "$tmpdir/naked/src/lib.rs" <<'RS'
#[derive(Debug)]
#[non_exhaustive]
pub enum OkAfter { A, B }
RS

if NON_EXHAUSTIVE_REPO_ROOT="$tmpdir" bash scripts/non-exhaustive-check.sh >/dev/null 2>&1; then
    pass "$scenario"
else
    fail "$scenario (backstop rejected a valid attribute-cluster shape)"
    NON_EXHAUSTIVE_REPO_ROOT="$tmpdir" bash scripts/non-exhaustive-check.sh 2>&1 || true
fi

# --- 12. backstop positive test: #[non_exhaustive] + #[derive] ------
# Reverse attribute order. Rustc accepts attributes in any order on an
# item; the backstop must too. Asymmetric acceptance would mean the
# backstop flags code clippy is happy with.
scenario="backstop positive test: accepts #[non_exhaustive] + #[derive]"
cat > "$tmpdir/naked/src/lib.rs" <<'RS'
#[non_exhaustive]
#[derive(Debug)]
pub enum OkBefore { A, B }
RS

if NON_EXHAUSTIVE_REPO_ROOT="$tmpdir" bash scripts/non-exhaustive-check.sh >/dev/null 2>&1; then
    pass "$scenario"
else
    fail "$scenario (backstop rejected reverse-order attribute cluster)"
    NON_EXHAUSTIVE_REPO_ROOT="$tmpdir" bash scripts/non-exhaustive-check.sh 2>&1 || true
fi

# --- summary --------------------------------------------------------
echo
echo "$pass_count passed, $fail_count failed, $skip_count skipped"

if [ "$fail_count" -gt 0 ]; then
    exit 1
fi
exit 0
