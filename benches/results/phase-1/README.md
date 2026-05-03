# Phase 1 parity bench results

This directory holds the result-pair JSONs and signatures from
Phase 1 of the parity bench harness (ROADMAP:829). The harness
itself lives at `benches/storage/`; the per-commit plan is in
`.planning/parity-bench-harness.plan.md`.

> **No numbers are committed here yet.** Phase 1 numbers land in a
> separate commit (the plan's commit 13), and only after a Linux
> Tier-1 run satisfies the gate's "≥ 1 win, 0 losses, no
> incomplete" rule. See **Loss-magnitude decision tree** below for
> the rules on which exit door a failed run takes.

## Layout (when populated)

```
benches/results/phase-1/
├── README.md                      # this file
├── <utc>-<sha>-mango/             # one mango run-pair output dir
│   ├── mango.json                 # result file consumed by gate
│   └── signature.txt              # BENCH_HW v1: ... line, co-resident
├── <utc>-<sha>-bbolt/
│   ├── bbolt.json
│   └── signature.txt
└── <utc>-<sha>-failed/            # any non-passing run lands here,
    └── ...                        # so the gap is provenance-tracked
```

`<utc>` is the start time, `<sha>` is the source git SHA. The two
sibling directories are the mango/bbolt halves of one paired
session.

## Running a Phase 1 bench

```bash
# Build the bbolt oracle once.
benches/oracles/bbolt/build.sh
export MANGO_BBOLT_ORACLE="$PWD/benches/oracles/bbolt/bbolt-oracle"

# Run the bench (Linux Tier-1 only — see HARDWARE.md).
TS=$(date -u +%Y%m%dT%H%M%SZ)
SHA=$(git rev-parse --short HEAD)

BENCH_TIER=1 BENCH_OUT_DIR=benches/results/phase-1/${TS}-${SHA}-mango \
    benches/runner/run.sh \
    cargo run -p mango-bench-storage --release --bin run -- \
        --workload benches/workloads/storage.toml \
        --engine mango \
        --runs 20 \
        --out benches/results/phase-1/${TS}-${SHA}-mango

BENCH_TIER=1 BENCH_OUT_DIR=benches/results/phase-1/${TS}-${SHA}-bbolt \
    benches/runner/run.sh \
    cargo run -p mango-bench-storage --release --bin run -- \
        --workload benches/workloads/storage.toml \
        --engine bbolt \
        --runs 20 \
        --out benches/results/phase-1/${TS}-${SHA}-bbolt

# Apply the gate.
cargo run -p mango-bench-storage --release --bin gate -- \
    benches/results/phase-1/${TS}-${SHA}-mango/mango.json \
    benches/results/phase-1/${TS}-${SHA}-bbolt/bbolt.json
echo "gate exit: $?"
```

The gate's exit code is the only signal that matters for L829:

| Exit | Meaning                                                                                                                                      |
| ---- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| 0    | **Pass.** ≥ 1 Win, 0 Loss, no `incomplete`. ROADMAP:829 is satisfied.                                                                        |
| 1    | **Fail.** Mango lost on a non-asymmetric metric, OR no metric reached Win, OR any `incomplete`.                                              |
| 2    | **Structural error.** Schema mismatch, missing/non-Tier-1 signature, unknown metric, etc. The bench did not run cleanly enough to be graded. |

The full verdict logic is in `benches/storage/src/gate.rs`; the
N9 signature gate is in `benches/storage/src/gate.rs::read_signature`.

## What the gate checks (the contract for L829)

A result-pair satisfies ROADMAP:829 iff **all** of the following
hold (otherwise the gate's `Pass` is unreachable and exit code is
1 or 2):

1. **Both runs were Linux Tier-1.** The `signature.txt` co-resident
   with each JSON parses as `os=linux tier=1` (N9 §2). macOS or
   Tier-2 hardware can produce useful regression data but cannot
   satisfy L829.
2. **Both result JSONs carry `format_version == 1`.** The schema is
   versioned; an old result file cannot be silently re-graded
   under new gate logic (N10).
3. **Both runs use the same workload toml.** Compared via
   `workload_sha256` byte-for-byte. A run on a non-canonical
   workload is interesting but does not satisfy this gate.
4. **No metric is `incomplete`.** S2 cold-cache verdict
   `incomplete` (e.g., a macOS run where `posix_fadvise` /
   `drop_caches` were unavailable) fails the gate even if every
   other metric passes — the contract requires both hot- and
   cold-cache pass.
5. **At least one Win, zero Losses on non-asymmetric metrics.**
   Bootstrap-95% CI of the mango/bbolt ratio: lower bound ≥ 1.05
   = Win, upper bound ≤ 0.95 = Loss, anything else = Tie. A
   metric whose `fairness == "asymmetric"` is excluded from the
   aggregate (S3 §B4).

## Loss-magnitude decision tree (B4)

If a paired run does not pass the gate, the response depends on
the magnitude of the loss:

| Outcome                                        | Response                                                                                                                                                                                                                                                                     |
| ---------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Mango wins ≥ 1, loses 0 (gate exit 0)          | Land harness PR with the result-pair under `benches/results/phase-1/${TS}-${SHA}-{mango,bbolt}/`, then flip ROADMAP:829 in a separate commit on `main` per the standard loop.                                                                                                |
| Mango ties everywhere                          | Land harness PR **without** flipping ROADMAP:829. The gate requires ≥ 1 Win. File a perf-fix follow-up issue. The cache-knob lever (`BackendConfig::with_cache_size`) is the obvious place to look first.                                                                    |
| Mango loses by 5 – 25 %                        | Land harness PR **without** flipping ROADMAP:829. Real perf gap. File a perf-fix follow-up issue. The follow-up's PR addresses the gap, reruns the bench, and only on its merge does ROADMAP:829 flip. Failed numbers commit under `${TS}-${SHA}-failed/`.                   |
| Mango loses by > 25 %                          | Almost certainly a bench bug (cache misconfigured, RNG seed accidentally constant, value compression on by accident, redb opened in a degenerate path). Land harness PR **without** flipping ROADMAP:829, file a bench-bug issue, fix the bench. **Do NOT publish numbers.** |
| Mango loses on a metric S3 marked `asymmetric` | Doesn't count toward the gate. Confirm the metric carries `"fairness": "asymmetric"` in both JSONs and is in `report.skipped`. If it isn't, that is a B1 / S3 bug; fix the harness.                                                                                          |
| Gate exits 2 (structural error)                | The bench did not run cleanly enough to be graded. Read the stderr message: most likely a missing or non-Tier-1 signature, a `format_version` mismatch, or an unknown metric name. **Fix the structural problem; do not commit numbers.**                                    |

## What lives next to a result JSON

The harness's `run.sh` wraps the bench invocation and writes a
`signature.txt` next to the JSON. The JSON's `signature_path`
field is a path **relative to the JSON's directory**; the gate
canonicalizes both and refuses any signature outside the JSON's
directory (N9 §3). This prevents copying a Linux signature into a
macOS result directory and re-using it.

The signature line looks like:

```
BENCH_HW v1: arch=amd64 cores=64 cpu=AMD\ EPYC\ 7B13 cpu_mhz_max=3500 kernel=6.1.0 mem_channels=8 os=linux ram_gb=256 scheduler=mq-deadline storage=Samsung\ 980\ Pro tier=1 tsc=invariant turbo=disabled virt=bare-metal sha=<64-hex>
```

The gate only branches on `os=` and `tier=`; the other fields
are kept verbatim in `SignatureInfo.raw` for the human-readable
report.

## Re-running an existing pair

`gate` is idempotent and reads no state outside the two JSON
files and their `signature.txt` siblings. To re-grade an
existing result-pair:

```bash
cargo run -p mango-bench-storage --release --bin gate -- \
    benches/results/phase-1/<dir-mango>/mango.json \
    benches/results/phase-1/<dir-bbolt>/bbolt.json
```

The `--rng-seed` flag overrides the bootstrap RNG seed. Default
is the constant `gate::DEFAULT_GATE_RNG_SEED`; supply
`--rng-seed <u64>` for run-to-run reproducibility against a
specific operator seed.

## See also

- `.planning/parity-bench-harness.plan.md` — the full Phase 1
  plan, including S1 / S2 / S3 / N9 / N10 numbering used in the
  module-level docs.
- `benches/storage/README.md` — harness internals, op set,
  histogram parameters, oracle protocol.
- `benches/runner/HARDWARE.md` — Tier 1 / Tier 2 acceptance
  classes and the signature schema.
- `benches/oracles/bbolt/BBOLT_QUIRKS.md` — divergences from
  default bbolt usage in the oracle (S3 force-copy, fsync
  fairness, …).
