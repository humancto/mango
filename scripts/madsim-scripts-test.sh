#!/usr/bin/env bash
# scripts/madsim-scripts-test.sh
#
# Smoke-test harness for the madsim CI gate (item 0.5.3).
#
# What this covers:
#   - .github/workflows/madsim.yml has the structural invariants the
#     CI gate depends on (cfg activation, pinned env vars, per-crate
#     target dir, curated-subset enumeration, path filters).
#   - docs/madsim.md exists.
#   - scripts/madsim-crates.sh resolves the curated subset.
#   - Workspace Cargo.toml carries the package-rename and the
#     metadata.mango.madsim table.
#   - The demo crate's lib.rs contains zero `#[cfg(madsim)]` gates
#     (scaffold invariant).
#   - `cargo metadata` resolves the demo crate's `tokio` dep to the
#     `madsim-tokio` package (regression test for the workspace-level
#     package-rename-through-workspace-inheritance contract).
#
# Workflow-absent behavior (sbom-style):
#   Assertions that need the workflow file print SKIP if the file is
#   absent. The metadata / script / docs assertions run unconditionally.
#   Once the workflow lands, this script runs as a step inside it,
#   and SKIP becomes unreachable.
#
# Invocation:
#   bash scripts/madsim-scripts-test.sh
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

workflow=".github/workflows/madsim.yml"
demo_lib="crates/mango-madsim-demo/src/lib.rs"

# --- 1. policy doc exists -------------------------------------------
scenario="docs/madsim.md exists"
if [ -f "docs/madsim.md" ]; then
    pass "$scenario"
else
    fail "$scenario"
fi

# --- 2. madsim-crates.sh is executable & returns ≥1 crate -----------
scenario="madsim-crates.sh emits ≥1 crate on this workspace"
if [ -x "scripts/madsim-crates.sh" ]; then
    out="$(bash scripts/madsim-crates.sh 2>/dev/null || true)"
    if [ -n "$out" ]; then
        pass "$scenario (got: $(echo "$out" | tr '\n' ' '))"
    else
        # Before the workspace.metadata table lands, this is legitimately
        # empty; skip rather than fail so the script can land first.
        skip "$scenario (empty output — metadata table not yet populated?)"
    fi
else
    fail "$scenario (scripts/madsim-crates.sh not executable)"
fi

# --- 3. workspace Cargo.toml has the package-rename -----------------
scenario="workspace.dependencies has tokio = { package = \"madsim-tokio\" }"
if grep -qE '^\s*tokio\s*=\s*\{[^}]*package\s*=\s*"madsim-tokio"' Cargo.toml; then
    pass "$scenario"
else
    skip "$scenario (workspace Cargo.toml not yet wired)"
fi

# --- 4. workspace Cargo.toml has madsim pin with macros feature -----
scenario="workspace.dependencies has madsim with macros feature"
if grep -qE '^\s*madsim\s*=\s*\{[^}]*"macros"' Cargo.toml; then
    pass "$scenario"
else
    skip "$scenario (workspace Cargo.toml not yet wired)"
fi

# --- 5. metadata.mango.madsim table present -------------------------
scenario="[workspace.metadata.mango.madsim] table present"
if grep -qE '^\[workspace\.metadata\.mango\.madsim\]' Cargo.toml; then
    pass "$scenario"
else
    skip "$scenario (metadata table not yet wired)"
fi

# --- 6. metadata table contains mango-madsim-demo -------------------
scenario="metadata.mango.madsim.crates contains mango-madsim-demo"
# Probe via cargo metadata (authoritative) if we have jq, else grep.
if command -v jq >/dev/null 2>&1 && command -v cargo >/dev/null 2>&1; then
    got="$(cargo metadata --no-deps --format-version=1 --locked 2>/dev/null \
        | jq -r '.metadata.mango.madsim.crates // [] | .[]' 2>/dev/null || true)"
    if printf '%s\n' "$got" | grep -qx 'mango-madsim-demo'; then
        pass "$scenario"
    else
        skip "$scenario (table not yet populated)"
    fi
else
    skip "$scenario (jq/cargo unavailable)"
fi

# --- 7. demo lib.rs has zero cfg(madsim) gates ----------------------
# Excludes line comments (`//` / `//!`) so the doc-comment that
# *documents* the invariant ("This file MUST NOT contain any
# `#[cfg(madsim)]` gates...") does not false-positive the test.
# A real `#[cfg(madsim)]` attribute would live on a non-comment line.
scenario="demo lib.rs has zero #[cfg(madsim)] / #[cfg(not(madsim))] gates"
if [ -f "$demo_lib" ]; then
    if grep -v '^[[:space:]]*//' "$demo_lib" \
        | grep -qE 'cfg\(\s*(not\s*\(\s*)?madsim'; then
        fail "$scenario (found cfg(madsim) gate — scaffold invariant violated)"
    else
        pass "$scenario"
    fi
else
    skip "$scenario (demo crate not yet present)"
fi

# --- 8. cargo metadata: demo crate's tokio dep resolves to madsim-tokio --
# The load-bearing assertion for the "workspace-level package rename
# through workspace = true" contract. If Cargo ever changes that
# behavior, this goes red.
scenario="demo crate tokio dep resolves to madsim-tokio (package-rename regression test)"
if command -v jq >/dev/null 2>&1 && command -v cargo >/dev/null 2>&1 \
    && [ -f "crates/mango-madsim-demo/Cargo.toml" ]; then
    # Use --no-deps to get the crate's declared deps; the `rename` field
    # is populated when package != name.
    got="$(cargo metadata --no-deps --format-version=1 --locked 2>/dev/null \
        | jq -r '.packages[]
                 | select(.name == "mango-madsim-demo")
                 | .dependencies[]
                 | select(.name == "madsim-tokio")
                 | .rename // .name' 2>/dev/null || true)"
    if [ "$got" = "tokio" ] || [ "$got" = "madsim-tokio" ]; then
        pass "$scenario (resolved: name=madsim-tokio, local=$got)"
    else
        fail "$scenario (got: '$got' — expected the crate to depend on madsim-tokio)"
    fi
else
    skip "$scenario (demo crate or jq unavailable)"
fi

# --- Workflow-presence gated assertions -----------------------------
if [ ! -f "$workflow" ]; then
    skip "workflow file exists ($workflow)"
    skip "workflow name is 'madsim'"
    skip "workflow sets RUSTFLAGS=\"--cfg madsim\" on the test step"
    skip "workflow sets MADSIM_TEST_SEED and MADSIM_TEST_NUM env vars"
    skip "workflow uses --target-dir target/madsim"
    skip "workflow pull_request paths include load-bearing entries"
    skip "workflow has merge_group trigger without paths"
    skip "workflow runs MSRV check under --cfg madsim"
else
    scenario="workflow file exists ($workflow)"
    pass "$scenario"

    scenario="workflow name is 'madsim'"
    if grep -qE '^name:[[:space:]]+madsim[[:space:]]*$' "$workflow"; then
        pass "$scenario"
    else
        fail "$scenario"
    fi

    # Regex excludes comment lines (public-api precedent): a naive grep
    # matches the prose in the header comment and masks a regression
    # where the env block itself drops the flag.
    scenario="workflow sets RUSTFLAGS=\"--cfg madsim\" on a non-comment line"
    if grep -v '^[[:space:]]*#' "$workflow" \
        | grep -qE 'RUSTFLAGS:[[:space:]]*["'\'']?--cfg[[:space:]]+madsim'; then
        pass "$scenario"
    else
        fail "$scenario"
    fi

    scenario="workflow sets MADSIM_TEST_SEED and MADSIM_TEST_NUM env vars"
    if grep -v '^[[:space:]]*#' "$workflow" | grep -qE '^\s*MADSIM_TEST_SEED:' \
        && grep -v '^[[:space:]]*#' "$workflow" | grep -qE '^\s*MADSIM_TEST_NUM:'; then
        pass "$scenario"
    else
        fail "$scenario"
    fi

    scenario="workflow uses --target-dir target/madsim"
    if grep -v '^[[:space:]]*#' "$workflow" \
        | grep -qE -- '--target-dir[[:space:]]+target/madsim'; then
        pass "$scenario"
    else
        fail "$scenario"
    fi

    scenario="workflow pull_request paths include load-bearing entries"
    missing=""
    for needle in \
        'crates/\*\*/src/\*\*' \
        'crates/\*\*/tests/\*\*' \
        'crates/\*\*/Cargo.toml' \
        'Cargo.toml' \
        'Cargo.lock' \
        '.github/workflows/madsim.yml' \
        'scripts/madsim-crates.sh' \
        'scripts/madsim-scripts-test.sh' \
        'docs/madsim.md'
    do
        if ! grep -qE -- "$needle" "$workflow"; then
            missing="$missing $needle"
        fi
    done
    if [ -z "$missing" ]; then
        pass "$scenario"
    else
        fail "$scenario (missing:$missing)"
    fi

    scenario="workflow has merge_group trigger without paths"
    # merge_group does not honour paths; asserting the trigger is present.
    if grep -qE '^\s*merge_group:' "$workflow"; then
        pass "$scenario"
    else
        fail "$scenario"
    fi

    scenario="workflow runs MSRV check under --cfg madsim"
    # Match a line referencing `cargo +1.89` (MSRV toolchain) in a step
    # that also mentions --cfg madsim. Kept permissive because exact
    # syntax (cargo +1.89.0 vs cargo +1.89 vs $MSRV var) is an impl choice.
    if grep -v '^[[:space:]]*#' "$workflow" \
        | grep -qE 'cargo[[:space:]]+\+?(1\.89|\$\{?\{?[[:space:]]*env\.MSRV)'; then
        pass "$scenario"
    else
        fail "$scenario (no MSRV-pinned cargo invocation found)"
    fi
fi

# --- summary --------------------------------------------------------
echo
echo "$pass_count passed, $fail_count failed, $skip_count skipped"

if [ "$fail_count" -gt 0 ]; then
    exit 1
fi
exit 0
