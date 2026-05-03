#!/usr/bin/env bash
# Materialize the seed corpus for the L853 fuzz targets.
#
# cargo-fuzz reads from `corpus/<target>/` on startup. The corpus
# directory itself is gitignored (corpora grow in CI runs); this
# script reproduces the curated seed set on demand.
#
# Idempotent — safe to re-run. Uses `printf` (not `echo -e`) for
# cross-shell portability on macOS bash 3.x.
#
# Usage: from the repo's `fuzz/` directory, run `./seed_corpus.sh`.
# Phase 15's CI must invoke this before any fuzz job; otherwise
# libfuzzer starts from a single empty input and the early-cycle
# coverage signal is wasted.
set -euo pipefail

cd "$(dirname "$0")"

mkdir -p corpus/round_trip corpus/decode_arbitrary

# decode_arbitrary corpus — the byte streams that exercise edge
# cases in the decoder. Mirrors the hand-written rejection tests
# at crates/mango-mvcc/src/encoding.rs#tests.
#
# - empty input
# - 16 bytes (bad length, short)
# - 17 bytes of zeros (decodes as (rev=(0,0), Put))
# - 18 bytes of zeros (separator OK, mark byte 0x00 != 0x74 -> bad mark)
# - 19 bytes (bad length, long)
# - well-formed Put for Revision(1, 2)
# - well-formed Tombstone for Revision(1, 2)
# - 17 bytes with bad separator (offset 8 = 0x00, not 0x5f)

CORPUS_DA=corpus/decode_arbitrary
printf '' > "${CORPUS_DA}/empty"
printf '%0.s\x00' $(seq 1 16) > "${CORPUS_DA}/zero_16"
printf '\x00\x00\x00\x00\x00\x00\x00\x00\x5f\x00\x00\x00\x00\x00\x00\x00\x00' \
    > "${CORPUS_DA}/zero_17_put_at_origin"
printf '\x00\x00\x00\x00\x00\x00\x00\x00\x5f\x00\x00\x00\x00\x00\x00\x00\x00\x00' \
    > "${CORPUS_DA}/zero_18_bad_mark"
printf '%0.s\x00' $(seq 1 19) > "${CORPUS_DA}/zero_19"
# Revision(1, 2) Put: main=1 BE, sep=_, sub=2 BE.
printf '\x00\x00\x00\x00\x00\x00\x00\x01\x5f\x00\x00\x00\x00\x00\x00\x00\x02' \
    > "${CORPUS_DA}/rev_1_2_put"
# Revision(1, 2) Tombstone: same as above + 0x74.
printf '\x00\x00\x00\x00\x00\x00\x00\x01\x5f\x00\x00\x00\x00\x00\x00\x00\x02\x74' \
    > "${CORPUS_DA}/rev_1_2_tombstone"
# 17 bytes, separator at offset 8 is 0x00 not 0x5f.
printf '\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x02' \
    > "${CORPUS_DA}/rev_1_2_bad_separator"

# round_trip corpus — typed Arbitrary inputs.
# arbitrary's u32 sourcing reads 4 bytes per field (unstructured),
# plus 1 byte for the bool. Layout: main (4) + sub (4) + is_tomb (1) = 9 bytes.
# We seed a few interesting (main, sub, kind) shapes.
CORPUS_RT=corpus/round_trip
# (main=0, sub=0, Put)
printf '\x00\x00\x00\x00\x00\x00\x00\x00\x00' > "${CORPUS_RT}/zero_put"
# (main=0, sub=0, Tombstone)
printf '\x00\x00\x00\x00\x00\x00\x00\x00\x01' > "${CORPUS_RT}/zero_tombstone"
# (main=u32::MAX, sub=u32::MAX, Put)
printf '\xff\xff\xff\xff\xff\xff\xff\xff\x00' > "${CORPUS_RT}/max_put"
# (main=u32::MAX, sub=u32::MAX, Tombstone)
printf '\xff\xff\xff\xff\xff\xff\xff\xff\x01' > "${CORPUS_RT}/max_tombstone"
# (main=1, sub=2, Put) — `arbitrary` consumes u32s in little-endian.
printf '\x01\x00\x00\x00\x02\x00\x00\x00\x00' > "${CORPUS_RT}/rev_1_2_put"

echo "Seeded $(ls "${CORPUS_DA}" | wc -l | tr -d ' ') decode_arbitrary cases"
echo "Seeded $(ls "${CORPUS_RT}" | wc -l | tr -d ' ') round_trip cases"
