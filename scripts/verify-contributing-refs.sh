#!/usr/bin/env bash
# verify-contributing-refs.sh
#
# Audits CONTRIBUTING.md against the rest of the repo. Three checks:
#
#   1. Forward-reference reciprocity: every in-repo doc that mentions
#      `CONTRIBUTING.md` must have a backward link from CONTRIBUTING.md
#      to that doc (by path). Catches "plan promised the policy would be
#      linked from CONTRIBUTING.md but CONTRIBUTING.md doesn't link it."
#
#   2. Broken relative links: every relative markdown link in
#      CONTRIBUTING.md must resolve to an existing file. Catches
#      link-rot from file moves / renames.
#
#   3. Broken ROADMAP anchors: every `./ROADMAP.md#<anchor>` link in
#      CONTRIBUTING.md must map to a real `## `..`###### ` heading in
#      ROADMAP.md. Catches anchor drift when headings are renamed.
#
# Exit 0 on clean; exit 1 with a diff-style list on any failure.
#
# Portable to bash 3.2 (macOS default) — no `mapfile`, no arrays with
# process substitution, no bash-4+ features. Only external deps are
# `grep`, `sed`, `awk`, `sort`.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

contributing="CONTRIBUTING.md"
roadmap="ROADMAP.md"

if [ ! -f "$contributing" ]; then
  echo "FAIL: $contributing not found at repo root" >&2
  exit 1
fi
if [ ! -f "$roadmap" ]; then
  echo "FAIL: $roadmap not found at repo root" >&2
  exit 1
fi

failures=0

# ---------------------------------------------------------------------
# Check 1: forward-reference reciprocity.
#
# Scope: `docs/` and `benches/` — the durable policy-source docs.
# Planning docs under `.planning/` are intentionally *not* audited
# here: plans promise that their POLICY is linked from CONTRIBUTING,
# not that the plan-file itself is linked. The policy-source doc
# (docs/arithmetic-policy.md, docs/time.md, etc.) is what CONTRIBUTING
# links, and that's what this check enforces.
# ---------------------------------------------------------------------

echo "== Check 1: forward-reference reciprocity =="

referrers="$(
  grep -rIl --exclude-dir=.git --exclude-dir=target \
    --exclude="CONTRIBUTING.md" \
    --exclude="verify-contributing-refs.sh" \
    "CONTRIBUTING\.md" docs benches \
    | sed 's|^\./||' \
    | sort -u
)"

while IFS= read -r ref; do
  [ -z "$ref" ] && continue
  if ! grep -qF "$ref" "$contributing"; then
    echo "  FAIL: $ref mentions CONTRIBUTING.md but CONTRIBUTING.md does not link $ref" >&2
    failures=$((failures + 1))
  fi
done <<EOF
$referrers
EOF

# ---------------------------------------------------------------------
# Check 2: broken relative links.
# ---------------------------------------------------------------------

echo "== Check 2: broken relative links =="

# Extract the contents of every `](...)` in CONTRIBUTING.md. sed prints
# one link target per line. We then drop lines matching external
# schemes or pure-anchor links.
links="$(
  sed -n 's|.*\](\([^)]*\)).*|\1|gp' "$contributing" \
    | grep -vE '^(https?|mailto|ftp|tel|git):' \
    | grep -v '^#' \
    | sort -u
)"

while IFS= read -r link; do
  [ -z "$link" ] && continue
  # Strip any #anchor suffix for existence check.
  path="${link%%#*}"
  [ -z "$path" ] && continue
  if [ ! -e "$path" ]; then
    echo "  FAIL: broken relative link: $link (target '$path' not found)" >&2
    failures=$((failures + 1))
  fi
done <<EOF
$links
EOF

# ---------------------------------------------------------------------
# Check 3: broken ROADMAP anchors.
#
# GitHub slug rule (empirically verified): lowercase, spaces -> '-',
# strip any character that isn't a-z, 0-9, '-', or '_'. Multi-word
# punctuation collapses; e.g. "Crate inventory & non-rolled stack"
# becomes "crate-inventory--non-rolled-stack" (the `&` drops out,
# leaving the double '-' from "inventory - & -").
# ---------------------------------------------------------------------

echo "== Check 3: broken ROADMAP anchors =="

# Compute the expected anchor set from ROADMAP.md headings h2..h6.
roadmap_anchors="$(
  awk '/^#{2,6} / {
        sub(/^#+ +/, "");
        s = tolower($0);
        gsub(/ /, "-", s);
        gsub(/[^a-z0-9_-]/, "", s);
        print s
      }' "$roadmap" \
    | sort -u
)"

# Extract every ROADMAP.md anchor reference from CONTRIBUTING.md.
contributing_anchors="$(
  sed -n 's|.*\](\.\?/\?ROADMAP\.md#\([^)]*\)).*|\1|gp' "$contributing" \
    | sort -u
)"

while IFS= read -r anchor; do
  [ -z "$anchor" ] && continue
  if ! echo "$roadmap_anchors" | grep -qxF "$anchor"; then
    echo "  FAIL: CONTRIBUTING.md links ROADMAP.md#$anchor but no heading in ROADMAP.md slugifies to that anchor" >&2
    failures=$((failures + 1))
  fi
done <<EOF
$contributing_anchors
EOF

# ---------------------------------------------------------------------
# Summary.
# ---------------------------------------------------------------------

if [ "$failures" -gt 0 ]; then
  echo ""
  echo "verify-contributing-refs.sh: $failures failure(s)" >&2
  exit 1
fi

echo ""
echo "verify-contributing-refs.sh: all checks passed"
exit 0
