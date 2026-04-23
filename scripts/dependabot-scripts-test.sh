#!/usr/bin/env bash
# scripts/dependabot-scripts-test.sh
#
# Self-test harness for the Dependabot CI gate (ROADMAP 0.5-renovate-
# dependabot).
#
# What this covers:
#   - .github/dependabot.yml has the structural invariants the
#     Dependabot runtime + our SHA-pin policy depend on.
#   - The madsim-family group is wired (atomicity for `madsim` +
#     `madsim-tokio`, the latter under the workspace package-rename
#     at Cargo.toml → `tokio.workspace = true`).
#   - Cargo ecosystem sets `insecure-external-code-execution: deny`
#     (supply-chain posture).
#   - File validates against Dependabot's vendored JSON Schema.
#   - Every `uses:` line in .github/workflows/*.yml carries a 40-hex
#     SHA pin with a trailing comment (SHA-pin policy regression
#     test).
#
# CI vs local:
#   When $CI is set (GitHub Actions sets CI=true unconditionally),
#   validator tools MUST be installed or the script fails hard. A
#   silent pass in CI on a malformed dependabot.yml would be
#   catastrophic — Dependabot itself silently ignores files it
#   can't parse, so our CI gate is the only alert path.
#
#   Locally the script prints an install hint and skips only the
#   specific assertion requiring the missing tool; other assertions
#   still run. Hard-failing locally on missing tools would break
#   the first-run iteration loop.
#
# Invocation:
#   bash scripts/dependabot-scripts-test.sh
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

# In CI, missing validator tools MUST fail hard. Locally, skip is
# acceptable so first-run iteration works without a full toolchain.
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

config=".github/dependabot.yml"
schema=".github/schemas/dependabot-2.0.json"
policy_doc="docs/dependency-updates.md"

# --- 1. dependabot.yml exists ---------------------------------------
scenario="dependabot.yml exists at $config"
if [ -f "$config" ]; then
    pass "$scenario"
else
    fail "$scenario"
    echo
    echo "$pass_count passed, $fail_count failed, $skip_count skipped"
    exit 1
fi

# --- 2. version: 2 --------------------------------------------------
scenario="dependabot.yml declares version: 2"
if grep -qE '^[[:space:]]*version:[[:space:]]*2[[:space:]]*$' "$config"; then
    pass "$scenario"
else
    fail "$scenario"
fi

# --- 3. github-actions ecosystem present ----------------------------
scenario="github-actions ecosystem present"
if grep -qE '^[[:space:]]*-[[:space:]]*package-ecosystem:[[:space:]]*github-actions[[:space:]]*$' "$config"; then
    pass "$scenario"
else
    fail "$scenario"
fi

# --- 4. cargo ecosystem present -------------------------------------
scenario="cargo ecosystem present"
if grep -qE '^[[:space:]]*-[[:space:]]*package-ecosystem:[[:space:]]*cargo[[:space:]]*$' "$config"; then
    pass "$scenario"
else
    fail "$scenario"
fi

# --- 5. both ecosystems: directory "/" ------------------------------
scenario="both ecosystems set directory: \"/\""
# Expect exactly 2 matches (one per ecosystem).
dir_count=$(grep -cE '^[[:space:]]*directory:[[:space:]]*"/"[[:space:]]*$' "$config" || true)
if [ "$dir_count" = "2" ]; then
    pass "$scenario"
else
    fail "$scenario (expected 2 matches, got $dir_count)"
fi

# --- 6. both ecosystems: schedule.interval: weekly ------------------
scenario="both ecosystems set schedule.interval: weekly"
int_count=$(grep -cE '^[[:space:]]*interval:[[:space:]]*weekly[[:space:]]*$' "$config" || true)
if [ "$int_count" = "2" ]; then
    pass "$scenario"
else
    fail "$scenario (expected 2 matches, got $int_count)"
fi

# --- 7. both ecosystems: open-pull-requests-limit -------------------
scenario="both ecosystems set open-pull-requests-limit"
lim_count=$(grep -cE '^[[:space:]]*open-pull-requests-limit:[[:space:]]+[0-9]+' "$config" || true)
if [ "$lim_count" = "2" ]; then
    pass "$scenario"
else
    fail "$scenario (expected 2 matches, got $lim_count)"
fi

# --- 8. both ecosystems: commit-message.prefix ----------------------
scenario="both ecosystems set commit-message.prefix"
pfx_count=$(grep -cE '^[[:space:]]*prefix:[[:space:]]*"chore\((actions|deps)\)"' "$config" || true)
if [ "$pfx_count" = "2" ]; then
    pass "$scenario"
else
    fail "$scenario (expected 2 matches, got $pfx_count)"
fi

# --- 9. both ecosystems: groups: block ------------------------------
scenario="both ecosystems declare a groups: block"
grp_count=$(grep -cE '^[[:space:]]*groups:[[:space:]]*$' "$config" || true)
if [ "$grp_count" = "2" ]; then
    pass "$scenario"
else
    fail "$scenario (expected 2 matches, got $grp_count)"
fi

# --- 10. madsim-family group wired ----------------------------------
# Load-bearing atomicity check: madsim + madsim-tokio (manifest key
# `tokio` under the workspace rename at Cargo.toml) must be grouped
# so they ship in a single PR. See docs/dependency-updates.md.
scenario="madsim-family group present with patterns madsim / madsim-* / tokio"
# Extract a window starting at `madsim-family:` and check it names all
# three patterns before the next top-level group or ecosystem entry.
if awk '
    /^[[:space:]]*madsim-family:[[:space:]]*$/ { in_group = 1; next }
    in_group && /^[[:space:]]*(cargo-minor-patch|cargo-major|github-actions-|-[[:space:]]+package-ecosystem):/ { exit }
    in_group { print }
' "$config" | grep -qE '"madsim"' \
  && awk '
    /^[[:space:]]*madsim-family:[[:space:]]*$/ { in_group = 1; next }
    in_group && /^[[:space:]]*(cargo-minor-patch|cargo-major|github-actions-|-[[:space:]]+package-ecosystem):/ { exit }
    in_group { print }
' "$config" | grep -qE '"madsim-\*"' \
  && awk '
    /^[[:space:]]*madsim-family:[[:space:]]*$/ { in_group = 1; next }
    in_group && /^[[:space:]]*(cargo-minor-patch|cargo-major|github-actions-|-[[:space:]]+package-ecosystem):/ { exit }
    in_group { print }
' "$config" | grep -qE '"tokio"'; then
    pass "$scenario"
else
    fail "$scenario (madsim-family must list \"madsim\", \"madsim-*\", and \"tokio\" — load-bearing for madsim + madsim-tokio atomicity)"
fi

# --- 11. cargo: insecure-external-code-execution: deny --------------
scenario="cargo ecosystem sets insecure-external-code-execution: deny"
if grep -qE '^[[:space:]]*insecure-external-code-execution:[[:space:]]*deny[[:space:]]*$' "$config"; then
    pass "$scenario"
else
    fail "$scenario (supply-chain posture requires deny — see docs/dependency-updates.md)"
fi

# --- 12. YAML well-formedness ---------------------------------------
scenario="dependabot.yml is valid YAML"
if command -v python3 >/dev/null 2>&1 && python3 -c 'import yaml' >/dev/null 2>&1; then
    if python3 -c "import yaml,sys; yaml.safe_load(open('$config'))" >/dev/null 2>&1; then
        pass "$scenario"
    else
        fail "$scenario (python3 -c yaml.safe_load raised)"
    fi
else
    missing_tool "$scenario" "python3 + pyyaml" "install python3 and 'pip install pyyaml'"
fi

# --- 13. JSON Schema validation -------------------------------------
# A typo like `scheudle:` parses as valid YAML but is silently ignored
# by Dependabot. Schema validation catches it.
scenario="dependabot.yml validates against vendored Dependabot JSON Schema"
if [ ! -f "$schema" ]; then
    fail "$scenario (vendored schema missing at $schema)"
elif command -v check-jsonschema >/dev/null 2>&1; then
    if check-jsonschema --schemafile "$schema" "$config" >/dev/null 2>&1; then
        pass "$scenario"
    else
        # Re-run to surface the error in logs.
        err=$(check-jsonschema --schemafile "$schema" "$config" 2>&1 || true)
        fail "$scenario
$err"
    fi
else
    missing_tool "$scenario" "check-jsonschema" "pipx install check-jsonschema (or pip install in a venv)"
fi

# --- 14. policy doc exists ------------------------------------------
scenario="policy doc exists at $policy_doc"
if [ -f "$policy_doc" ]; then
    pass "$scenario"
else
    fail "$scenario"
fi

# --- 15. every workflow `uses:` line is SHA-pinned ------------------
# Regression test for the SHA-pin policy that Dependabot is here to
# preserve. A PR that adds `uses: actions/checkout@v4` short form
# would cause Dependabot to start bumping to floating tags — this
# check catches that before Dependabot's next run.
#
# Regex (POSIX ERE, portable between macOS BSD grep and Linux GNU
# grep): line may start with whitespace, optional `- ` list marker,
# `uses: ` then `<owner>/<repo>@<40-hex>` then optional `# <ref>`.
scenario="all workflow uses: lines are SHA-pinned with 40-hex"
shopt -s nullglob
workflows=(.github/workflows/*.yml)
shopt -u nullglob
if [ "${#workflows[@]}" -eq 0 ]; then
    skip "$scenario (no workflow files found)"
else
    violators=""
    for wf in "${workflows[@]}"; do
        # Lines that contain a `uses:` directive but DO NOT match the
        # SHA-pinned pattern. Comment lines starting with `#` are
        # excluded (header prose can legitimately mention uses: ...).
        bad=$(grep -nE '^[[:space:]]*(-[[:space:]]+)?uses:' "$wf" \
            | grep -v '^[^:]*:[[:space:]]*#' \
            | grep -vE '^[^:]*:[[:space:]]*(-[[:space:]]+)?uses:[[:space:]]+[^@[:space:]]+@[0-9a-f]{40}([[:space:]]+#.*)?[[:space:]]*$' \
            || true)
        if [ -n "$bad" ]; then
            violators="${violators}${wf}:
${bad}
"
        fi
    done
    if [ -z "$violators" ]; then
        pass "$scenario"
    else
        fail "$scenario
$violators"
    fi
fi

# --- summary --------------------------------------------------------
echo
echo "$pass_count passed, $fail_count failed, $skip_count skipped"

if [ "$fail_count" -gt 0 ]; then
    exit 1
fi
exit 0
