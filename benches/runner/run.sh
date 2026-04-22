#!/usr/bin/env bash
# benches/runner/run.sh
#
# Wraps a bench command. Emits the hardware signature to stderr (so
# stdout stays clean for JSON-producing bench tools like criterion
# --output-format=json or hyperfine --export-json) and, if
# BENCH_OUT_DIR is set, also writes the signature to
# $BENCH_OUT_DIR/signature.txt for downstream correlation.
#
# Environment:
#   BENCH_TIER     required when argv looks like a bench; see
#                  hardware-signature.sh for the rule.
#   BENCH_OUT_DIR  optional; if set, signature is ALSO written to
#                  $BENCH_OUT_DIR/signature.txt.
#
# Exit code: whatever the wrapped command returns. If no argv is
# provided, exits 0 after emitting the signature (so `run.sh` can be
# used to print a bare signature for result-file headers).

set -euo pipefail

_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=./hardware-signature.sh
. "$_here/hardware-signature.sh"

# Validate tier at the top level (not inside the `$(...)` subshell
# below) so a hard-fail exit propagates to our caller. Gates
# `run.sh cargo bench` with BENCH_TIER unset from ever starting the
# wrapped command.
validate_tier_argv "$@"

sig=$(emit_signature)

# Signature always goes to stderr so stdout stays clean.
printf '%s\n' "$sig" >&2

# Sidecar file for downstream tooling that correlates results.
if [ -n "${BENCH_OUT_DIR:-}" ]; then
    mkdir -p "$BENCH_OUT_DIR"
    printf '%s\n' "$sig" > "$BENCH_OUT_DIR/signature.txt"
fi

# No argv → just print the signature and return.
if [ "$#" -eq 0 ]; then
    exit 0
fi

# Forward the command as-is; its exit code is ours.
exec "$@"
