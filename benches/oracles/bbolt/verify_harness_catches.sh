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
#   1. Copy the oracle source tree to a tempdir and rewrite every
#      `b.Put(key, val)` call to a self-consuming no-op func literal
#      `(func() error { _ = b; _ = key; _ = val; return nil })()`.
#      Go forbids unused locals, so the literal touches all three
#      names and returns nil — semantically the bucket is untouched
#      but the handler still returns OK:true. From the harness's
#      perspective the oracle silently drops `Put` ops while claiming
#      success.
#
#      Why mutate `Put` rather than `Delete`: the proptest strategy
#      generates Delete keys independently of prior Put keys, so the
#      collision probability is low — a silent `Delete` mutation can
#      survive 256 cases. By contrast, Put dominates every commit
#      (44 % strategy weight), so dropping it forces divergence on
#      the first commit of essentially every case.
#   2. Build `bbolt-oracle-mutated` against that patched source.
#   3. Run the default proptest sweep with that binary as
#      `MANGO_BBOLT_ORACLE`. We expect the run to FAIL with a
#      proptest divergence (Rust test runner exit code 101). Capture
#      stdout+stderr so we can distinguish "harness caught the
#      divergence" from "build/toolchain noise".
#   4. Cleanup: tempdir removed via `trap`.
#
# Exit codes:
#   0 = harness behaved correctly (caught the silent-drop mutation)
#   1 = harness FAILED to catch the mutation, OR the cargo run
#       failed for a non-divergence reason (build error, missing
#       toolchain, oracle crash). Either case is a real signal —
#       the meta-test is gating quality of the harness itself, so
#       we refuse to issue a PASS on noise.
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
# the default 256 cases the first sequence with a `Put` diverges,
# so the run fails within seconds. We expect FAILURE here — `set +e`
# so the script keeps running, then validate the exit code AND the
# captured output below. A bare exit-code check (any non-zero = PASS)
# is too lax: a build error, missing toolchain, or oracle crash also
# exits non-zero, and would silently issue a PASS verdict on noise.
LOG="$WORK/cargo-test.log"
echo "verify_harness_catches: running differential sweep against mutated oracle (output → $LOG)..."
set +e
(
    cd "$REPO_ROOT"
    MANGO_BBOLT_ORACLE="$ORACLE_PATH" \
        cargo test \
            -p mango-storage \
            --test differential_vs_bbolt \
            proptest_256_cases_no_divergence \
            -- --nocapture
) > "$LOG" 2>&1
HARNESS_EXIT=$?
set -e

# Tail the log either way so a failure in this script is locally
# debuggable. Capped to the last 60 lines — divergence dumps are
# verbose and the panic line is near the end.
echo "verify_harness_catches: --- last 60 lines of cargo output ---"
tail -n 60 "$LOG" || true
echo "verify_harness_catches: --- end of output ---"

# Rust's libtest exits 101 on panic — proptest emits the divergence
# via `panic!("proptest divergence: …")` (see
# `crates/mango-storage/tests/differential_vs_bbolt.rs:2215`), which
# is what we want. Build errors exit 101 too (cargo test propagates
# the rustc exit code), which is why we cross-check the panic
# marker below.
if [ "$HARNESS_EXIT" -eq 0 ]; then
    echo ""
    echo "verify_harness_catches: FAIL — harness returned 0 against a broken oracle." >&2
    echo "verify_harness_catches: the differential test did NOT detect silent-drop puts." >&2
    exit 1
fi

if [ "$HARNESS_EXIT" -ne 101 ]; then
    echo ""
    echo "verify_harness_catches: FAIL — cargo test exited $HARNESS_EXIT (expected 101)." >&2
    echo "verify_harness_catches: this is build/toolchain noise, not a harness signal." >&2
    exit 1
fi

if ! grep -qF "proptest divergence:" "$LOG"; then
    echo ""
    echo "verify_harness_catches: FAIL — no 'proptest divergence:' marker in cargo output." >&2
    echo "verify_harness_catches: exit 101 was caused by something other than a harness divergence." >&2
    exit 1
fi

echo ""
echo "verify_harness_catches: PASS — harness correctly failed (exit=$HARNESS_EXIT, divergence marker present) against mutated oracle."
exit 0
