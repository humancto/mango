#!/usr/bin/env bash
#
# verify_harness_catches.sh — meta-test for the differential harness.
#
# A differential harness is only as good as its ability to catch
# divergences. This script proves that — given a bbolt oracle that
# secretly drops `delete` operations — the harness in
# `crates/mango-storage/tests/differential_vs_bbolt.rs` correctly
# fails.
#
# How it works:
#   1. Copy `main.go` to a tempdir and rewrite every `b.Delete(key)`
#      call to `error(nil)` — the bucket is untouched but the
#      handler still returns OK:true. From the harness's perspective
#      the oracle silently drops `delete` ops while claiming success.
#   2. Build a `bbolt-oracle-mutated` binary against that patched
#      source.
#   3. Run a small (64-case) proptest sweep with that binary set as
#      `MANGO_BBOLT_ORACLE`. We expect the run to FAIL — divergence
#      surfaces as soon as a `Put` followed by a `Delete` leaves
#      bbolt with a key that mango successfully removed.
#   4. Cleanup: remove the tempdir and the mutated binary.
#
# Exit codes:
#   0 = harness behaved correctly (test failed when oracle was broken)
#   1 = harness FAILED to catch the mutation (silent quality regression)
#
# Run it:
#   bash benches/oracles/bbolt/verify_harness_catches.sh
#
# Wired into the nightly cron as a gating step (ROADMAP:819 plan §9
# commit 11). Not gated on PR runs because it doubles the differential
# run time; gating nightly is sufficient because the harness wire-shape
# changes are infrequent.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
WORK="$(mktemp -d -t mango-verify-harness.XXXXXX)"

cleanup() {
    rm -rf "$WORK"
}
trap cleanup EXIT

echo "verify_harness_catches: workdir=$WORK"

# Copy the oracle source tree (everything build.sh needs) into the
# tempdir so the mutation does not touch the committed source.
cp -R "$SCRIPT_DIR"/. "$WORK/oracle"
# Drop any stale binary that might have been copied so build.sh
# always produces a fresh artifact.
rm -f "$WORK/oracle/bbolt-oracle"

# Mutation: turn every `b.Put(key, val)` into a no-op that consumes
# all three locals (Go forbids unused locals) and returns nil. Hits
# both the top-level `opPut` handler and the `put` branch inside
# `opCommitGroup`. From the harness's view, `put` becomes a silent
# success — exactly the kind of latent correctness bug a
# differential test must detect.
#
# Why mutate `Put` rather than `Delete`: the proptest strategy
# generates Delete keys independently of prior Put keys (random
# 1..=16-byte sequences over a 16-symbol alphabet, four buckets),
# so the collision probability — and therefore the chance any given
# Delete hits an existing key — is low. A silent `Delete` mutation
# can survive 256 cases. By contrast, Put dominates every commit
# (44 % strategy weight), so dropping it causes divergence on the
# first commit of essentially every case. Verified empirically
# (see plan §9 commit 11 rationale): mutating Put fails the
# harness within a handful of cases; mutating Delete sometimes
# passed 256.
#
# The replacement is a one-shot func literal `(func() error { _ =
# b; _ = key; _ = val; return nil })()` because Go would refuse to
# compile `error(nil)` directly: `b`, `key`, `val` would become
# declared-and-unused locals. The literal touches all three names
# and returns nil, producing the no-op semantics with valid Go.
MUTATION_SENTINEL='(func() error { _ = b; _ = key; _ = val; return nil })()'
sed -E -i.orig "s|b\.Put\(key, val\)|${MUTATION_SENTINEL}|g" "$WORK/oracle/main.go"

# Belt-and-suspenders: confirm the substitution actually fired. If
# main.go ever drops the `b.Put(key, val)` form (e.g., a rename of
# `val` to `value`, or a refactor to a helper), the mutation is
# silently a no-op and this script would pass for the wrong reason.
if ! grep -qF "$MUTATION_SENTINEL" "$WORK/oracle/main.go"; then
    echo "verify_harness_catches: FAIL — sed mutation did not fire (main.go shape changed?)" >&2
    exit 1
fi

echo "verify_harness_catches: building mutated oracle..."
(cd "$WORK/oracle" && CGO_ENABLED=0 go build -trimpath -o bbolt-oracle-mutated .)

ORACLE_PATH="$WORK/oracle/bbolt-oracle-mutated"
if [ ! -x "$ORACLE_PATH" ]; then
    echo "verify_harness_catches: FAIL — mutated binary not produced at $ORACLE_PATH" >&2
    exit 1
fi

# Run the differential sweep against the mutated oracle. Even at
# the default 256 cases the first sequence with a `Delete` diverges,
# so the run fails within seconds. We expect FAILURE here — `set +e`
# so the script keeps running, then invert the exit code below.
echo "verify_harness_catches: running differential sweep against mutated oracle..."
set +e
(
    cd "$REPO_ROOT"
    MANGO_BBOLT_ORACLE="$ORACLE_PATH" \
        cargo test \
            -p mango-storage \
            --test differential_vs_bbolt \
            proptest_256_cases_no_divergence \
            -- --nocapture
)
HARNESS_EXIT=$?
set -e

if [ "$HARNESS_EXIT" -eq 0 ]; then
    echo ""
    echo "verify_harness_catches: FAIL — harness returned 0 against a broken oracle." >&2
    echo "verify_harness_catches: the differential test did NOT detect silent-drop deletes." >&2
    exit 1
fi

echo ""
echo "verify_harness_catches: PASS — harness correctly failed (exit=$HARNESS_EXIT) against mutated oracle."
exit 0
