#!/usr/bin/env bash
# Reproducible build of the bbolt oracle binary.
#
# Usage (from repo root or from this directory):
#   ./benches/oracles/bbolt/build.sh
#
# Emits `./bbolt-oracle` (or `bbolt-oracle.exe` on Windows, though
# the harness does not target Windows — see README.md).
#
# `-trimpath` strips local file paths from the binary so builds are
# byte-identical across contributors' machines. `-ldflags="-s -w"`
# strips the symbol table and DWARF info — not required for
# correctness but keeps the artifact small and makes a diff-mode
# run easier to inspect in `ps`.
#
# CGO is disabled because bbolt is pure Go. A stray cgo-enabled
# build would introduce a C toolchain dependency the CI containers
# don't necessarily have.
set -euo pipefail

cd "$(dirname "$0")"

# Verify go.mod is in sync with go.sum before we build. `go mod
# verify` checksums every module's zip against the corresponding
# h1: hash in go.sum. Out-of-sync state fails the build loud rather
# than producing a binary no one else can reproduce.
go mod verify

CGO_ENABLED=0 go build \
    -trimpath \
    -ldflags="-s -w" \
    -o bbolt-oracle \
    .

echo "built: $(pwd)/bbolt-oracle"
