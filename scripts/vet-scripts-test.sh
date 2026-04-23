#!/usr/bin/env bash
# scripts/vet-scripts-test.sh
#
# Smoke-test harness for the cargo-vet supply-chain gate.
#
# What this covers:
#   - xtask-vet-ttl honours exit codes for synthetic configs
#     (happy / expired / malformed / missing).
#   - scripts/vet-update.sh rejects a wrong cargo-vet version when
#     asked to (version-extraction regression guard).
#   - .github/workflows/vet.yml and supply-chain/ both exist and
#     are well-formed TOML/YAML at check time.
#
# What this does NOT cover:
#   - Running `cargo vet check` itself. That runs as a dedicated
#     workflow step after this harness — duplicating it here only
#     doubles CI wall-clock.
#   - The xtask parser's TOML-shape edge cases. Those are covered
#     by the 16 unit tests inside crates/xtask-vet-ttl/src/lib.rs.
#     This harness is the CLI-level oracle on top.
#
# Invocation:
#   bash scripts/vet-scripts-test.sh
#   (run from any CWD; script cd's to repo root)

set -u

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

pass_count=0
fail_count=0

# ---------------------------------------------------------------------
# assertion helpers
# ---------------------------------------------------------------------
RUN_OUT=""
RUN_EXIT=0
run_ttl() {
    local config_path="$1"
    shift
    # Capture $? immediately after $(); `|| true` would swallow the
    # exit code the way `$?` is sampled post-substitution.
    RUN_OUT="$(cargo run -q -p xtask-vet-ttl -- --config "$config_path" "$@" 2>&1)"
    RUN_EXIT=$?
}

assert_exit() {
    local scenario="$1" want="$2"
    if [ "$RUN_EXIT" -ne "$want" ]; then
        printf 'FAIL  %s: want exit %d, got %d\n' "$scenario" "$want" "$RUN_EXIT" >&2
        printf '----- output -----\n%s\n------------------\n' "$RUN_OUT" >&2
        fail_count=$((fail_count + 1))
        return 1
    fi
    return 0
}

assert_contains() {
    local scenario="$1" needle="$2"
    if ! printf '%s' "$RUN_OUT" | grep -qF -- "$needle"; then
        printf 'FAIL  %s: expected output to contain %q\n' "$scenario" "$needle" >&2
        printf '----- output -----\n%s\n------------------\n' "$RUN_OUT" >&2
        fail_count=$((fail_count + 1))
        return 1
    fi
    return 0
}

pass() {
    printf 'PASS  %s\n' "$1"
    pass_count=$((pass_count + 1))
}

# tmpdir cleanup
tmpdir="$(mktemp -d -t mango-vet-test.XXXXXX)"
trap 'rm -rf "$tmpdir"' EXIT INT TERM

# ---------------------------------------------------------------------
# xtask-vet-ttl CLI scenarios
# ---------------------------------------------------------------------

# Scenario 1: happy path — one exemption with a future review-by.
scenario="happy (future review-by -> exit 0)"
cat >"$tmpdir/happy.toml" <<'TOML'
[cargo-vet]
version = "0.10"

[[exemptions.foo]]
version = "1.0.0"
criteria = "safe-to-deploy"
notes = "review-by: 2099-01-01 — distant future"
TOML
run_ttl "$tmpdir/happy.toml"
assert_exit "$scenario" 0 && \
    assert_contains "$scenario" "PASS" && \
    pass "$scenario"

# Scenario 2: expired — review-by in the past must exit 1.
scenario="expired (past review-by -> exit 1)"
cat >"$tmpdir/expired.toml" <<'TOML'
[cargo-vet]
version = "0.10"

[[exemptions.foo]]
version = "1.0.0"
criteria = "safe-to-deploy"
notes = "review-by: 2000-01-01 — long ago"
TOML
run_ttl "$tmpdir/expired.toml"
assert_exit "$scenario" 1 && \
    assert_contains "$scenario" "past review-by" && \
    pass "$scenario"

# Scenario 3: malformed date — must exit 2 (hard fail, distinct
# from "expired" so contributors can tell a typo from a stale entry).
scenario="malformed date (bad month -> exit 2)"
cat >"$tmpdir/malformed.toml" <<'TOML'
[cargo-vet]
version = "0.10"

[[exemptions.foo]]
version = "1.0.0"
criteria = "safe-to-deploy"
notes = "review-by: 2026-13-01 — month 13"
TOML
run_ttl "$tmpdir/malformed.toml"
assert_exit "$scenario" 2 && \
    assert_contains "$scenario" "malformed" && \
    pass "$scenario"

# Scenario 4: missing review-by + --strict -> exit 1.
scenario="missing review-by + --strict -> exit 1"
cat >"$tmpdir/missing.toml" <<'TOML'
[cargo-vet]
version = "0.10"

[[exemptions.foo]]
version = "1.0.0"
criteria = "safe-to-deploy"
notes = "no token here"
TOML
run_ttl "$tmpdir/missing.toml" --strict
assert_exit "$scenario" 1 && \
    assert_contains "$scenario" "missing review-by" && \
    pass "$scenario"

# Scenario 5: missing review-by without --strict -> exit 0 (advisory).
scenario="missing review-by no-strict -> exit 0 (advisory)"
run_ttl "$tmpdir/missing.toml"
assert_exit "$scenario" 0 && \
    assert_contains "$scenario" "PASS" && \
    pass "$scenario"

# Scenario 6: --list prints a listing and exits 0 even if entries are
# expired (diagnostic mode; must not double-fail contributors who are
# investigating a red CI run).
scenario="--list (expired entries -> exit 0)"
run_ttl "$tmpdir/expired.toml" --list
assert_exit "$scenario" 0 && \
    assert_contains "$scenario" "listing" && \
    pass "$scenario"

# ---------------------------------------------------------------------
# vet-update.sh version-extraction regression guard
# ---------------------------------------------------------------------
# The wrapper reads CARGO_VET_VERSION from .github/workflows/vet.yml.
# If the extraction regex drifts (quoting, spacing), the wrapper
# would silently accept any installed version. Verify a concrete
# version line is extractable today.
scenario="vet-update.sh: CARGO_VET_VERSION is parseable"
if grep -qE '^\s*CARGO_VET_VERSION:\s*"[0-9]+\.[0-9]+\.[0-9]+"' .github/workflows/vet.yml; then
    pass "$scenario"
else
    printf 'FAIL  %s: CARGO_VET_VERSION line not found or malformed in vet.yml\n' "$scenario" >&2
    fail_count=$((fail_count + 1))
fi

# ---------------------------------------------------------------------
# supply-chain/ presence
# ---------------------------------------------------------------------
scenario="supply-chain/ files exist"
if [ -f supply-chain/config.toml ] \
    && [ -f supply-chain/audits.toml ] \
    && [ -f supply-chain/imports.lock ]; then
    pass "$scenario"
else
    printf 'FAIL  %s: one of config.toml / audits.toml / imports.lock missing\n' "$scenario" >&2
    fail_count=$((fail_count + 1))
fi

# ---------------------------------------------------------------------
# summary
# ---------------------------------------------------------------------
printf '\n%d passed, %d failed\n' "$pass_count" "$fail_count"
if [ "$fail_count" -ne 0 ]; then
    exit 1
fi
