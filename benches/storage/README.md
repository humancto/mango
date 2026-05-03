# mango-bench-storage

Phase 1 parity bench harness (ROADMAP:829). Drives a fixed workload
against the mango `RedbBackend` and the bbolt oracle subprocess,
captures latency / throughput / on-disk-size, emits a single-engine
result JSON, and gates two of those JSONs into a pass/fail verdict.

The full design rationale lives in
`.planning/parity-bench-harness.plan.md`. This README is the
crate-level operator manual: what the modules do, how to run the
harness, where each protocol invariant is enforced.

> **The `run` binary at `src/bin/run.rs` is still a scaffold stub.**
> Phase 1 numbers will be produced by wiring the public module API
> (`mango_runner` / `bbolt_runner` / `measure`) into `run.rs` in a
> follow-up commit on Tier-1 Linux hardware. The lib API and the
> `gate` binary are complete.

## Layout

```
benches/storage/
├── Cargo.toml                 # publish = false; bench-only crate
├── src/
│   ├── lib.rs                 # public module roots (one section per file below)
│   ├── workload.rs            # toml parser + sha256 of the canonical bytes
│   ├── measure.rs             # LatencyHistogram + ResultFile JSON shape
│   ├── stats.rs               # paired bootstrap 95 % CI + Verdict
│   ├── zipfian.rs             # YCSB ScrambledZipfianGenerator port (N3)
│   ├── dropcache.rs           # cold-cache primitive (S2 / N6)
│   ├── mango_runner.rs        # in-process driver against RedbBackend
│   ├── bbolt_runner.rs        # subprocess driver against the bbolt oracle
│   ├── gate.rs                # verdict logic — the wall for L829
│   └── bin/
│       ├── run.rs             # workload driver CLI (currently stub)
│       └── gate.rs            # verdict CLI — exit 0 / 1 / 2
└── tests/                     # integration-test placeholder
```

## Module guide

| Module         | Responsibility                                                                                                                                                                                                                              | Key invariants                                                                                                                                                                                    |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `workload`     | Parses `benches/workloads/storage.toml`, computes sha256 of the verbatim bytes. The hash is recorded in every result JSON; the gate compares two files byte-for-byte.                                                                       | The on-disk toml is `include_bytes!`-ed into a regression test (`canonical_phase_1_storage_toml_loads_and_pins_critical_fields`) so any edit invalidates the test binary.                         |
| `measure`      | `LatencyHistogram` (HdrHistogram pinned to 1 µs floor / 60 s ceiling / 3 sig figs) and the `ResultFile` JSON shape. V2-deflate base64 is the on-wire format shared with the Go oracle.                                                      | `format_version == 1`. Saturating record on overflow — a single >60 s outlier cannot abort a run.                                                                                                 |
| `stats`        | Paired bootstrap 95 % CI on the mango/bbolt ratio. `Verdict::{Win, Loss, Tie, Incomplete}`. `HigherIs::{Better, Worse}` flips the ratio so latency/size are graded "lower is better".                                                       | `WIN_FLOOR = 1.05`, `LOSS_CEILING = 0.95`, `DEFAULT_RESAMPLES = 10_000`. RNG is `ChaCha20Rng`, seeded per-metric (see `gate::mix_seed`).                                                          |
| `zipfian`      | YCSB `ScrambledZipfianGenerator` semantics. `theta = 0.99`, `USED_ZIPFIAN_CONSTANT_THETA_099`, `YCSB_BASE_ITEM_COUNT`.                                                                                                                      | `rand_distr::Zipf` is **forbidden** (different tail mass; not the YCSB distribution — N3).                                                                                                        |
| `dropcache`    | Two-stage cold-cache primitive: `posix_fadvise(POSIX_FADV_DONTNEED)` (per-file, no root) then best-effort `drop_caches` (root-only). On macOS the verdict is `Incomplete` — both stages are unavailable, and the gate fails the bench (S2). | Symmetric: both engines pay the same primitive on the same file set.                                                                                                                              |
| `mango_runner` | In-process driver for `RedbBackend`. Op set: `open` / `load` / `get_seq` / `get_zipfian` / `range` / `size` / `close`.                                                                                                                      | Async commit path goes through a private `tokio` current-thread runtime — same pattern as the differential test in `crates/mango-storage/tests/differential_vs_bbolt.rs`.                         |
| `bbolt_runner` | JSON-line subprocess protocol against `benches/oracles/bbolt/bbolt-oracle --mode=bench`. Same op set. Latency histograms travel base64 V2-deflate over the pipe.                                                                            | Single in-flight: caller MUST read each response before sending the next request.                                                                                                                 |
| `gate`         | Reads two result JSONs, applies S1 / S2 / S3 / N9 / N10. Pass = ≥ 1 Win, 0 Loss, no Incomplete, signatures parse as Linux Tier-1.                                                                                                           | `infer_kind` is a closed lookup. An unknown metric name is `GateError::UnknownMetric` (exit 2), not a silent `Throughput` default. New metrics require a code change AND a `format_version` bump. |

## Op set (`mango_runner` ⇋ `bbolt_runner`)

The two runners ship a shape-identical surface so the orchestrator's
paired-comparison loop calls into either engine through the same
function names and outcome types.

| Op                              | What it measures                                                                                                                                                                                                     |
| ------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `open`                          | Process / file open cost. Untimed for the verdict; bookkeeping only.                                                                                                                                                 |
| `load`                          | Initial bulk-load. Counts as `write_throughput_load`.                                                                                                                                                                |
| `unbatched_put` / `batched_put` | Two write paths (1 op/commit and 64 ops/commit) — `write_throughput_unbatched` / `write_throughput_batched`. Stresses per-commit fsync overhead vs amortized fsync.                                                  |
| `get_seq` (hot)                 | `read_throughput_hot` + `read_latency_p99_hot`. Cache is warm; measures steady-state.                                                                                                                                |
| `get_seq` (cold)                | `read_throughput_cold` + `read_latency_p99_cold`. Cache dropped via `dropcache` first. Verdict is `Incomplete` on platforms that can't drop.                                                                         |
| `get_zipfian`                   | `read_throughput_zipfian` + `read_latency_p99_zipfian`. YCSB-skew distribution at θ = 0.99.                                                                                                                          |
| `range`                         | `range_throughput` over `sizes = [100, 10_000, 100_000]`. Marked `fairness = "symmetric_copy"`: both engines force-copy each row + xor-fold checksum so bbolt's mmap reads cannot be elided (S3 fairness invariant). |
| `size`                          | On-disk size after compaction (`db.Compact()` on bbolt, redb compaction on mango). Steady-state size, not "size with whatever fragmentation the load order produced".                                                |
| `close`                         | Graceful shutdown signal. Untimed.                                                                                                                                                                                   |

## Histogram parameters (pinned)

```rust
pub const LOWEST_DISCERNIBLE_NS: u64 = 1_000;          // 1 µs floor
pub const HIGHEST_TRACKABLE_NS: u64  = 60_000_000_000; // 60 s ceiling
pub const SIGNIFICANT_FIGURES: u8    = 3;              // ~0.1 % buckets
```

These constants are part of the bench protocol. The Rust harness and
the Go bbolt oracle (`benches/oracles/bbolt/bench.go`) MUST agree
byte-for-byte. A change is a wire-format break and requires bumping
`format_version`.

## Result JSON shape

`ResultFile` (`format_version = 1`):

- `format_version`, `engine`, `runs`, `seed`, `created_at_utc`
- `workload_sha256`, `workload_version` (gated for byte-equality)
- `signature_path` (relative to the JSON's directory; gate
  canonicalizes and refuses paths outside that directory — N9 §3)
- `runs[]` — per-run histograms (hot / cold / zipfian) base64
  V2-deflate, `cold_cache_verdict`, `run_order`
- `metrics[]` — per-metric `MetricRecord` with `engine_runs`, mean,
  median, stddev, optional `fairness` flag

Older payloads without `fairness` deserialize successfully — the
field is `skip_serializing_if = "Option::is_none"`.

## Running the harness

The `run` binary is currently a stub; once it is wired up:

```bash
# Build the bbolt oracle once.
benches/oracles/bbolt/build.sh
export MANGO_BBOLT_ORACLE="$PWD/benches/oracles/bbolt/bbolt-oracle"

# Run mango.
TS=$(date -u +%Y%m%dT%H%M%SZ)
SHA=$(git rev-parse --short HEAD)

BENCH_TIER=1 BENCH_OUT_DIR=benches/results/phase-1/${TS}-${SHA}-mango \
    benches/runner/run.sh \
    cargo run -p mango-bench-storage --release --bin run -- \
        --workload benches/workloads/storage.toml \
        --engine mango \
        --runs 20 \
        --out benches/results/phase-1/${TS}-${SHA}-mango

# Run bbolt.
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

`benches/runner/run.sh` is the wrapper that emits the
`signature.txt` co-resident with the result JSON (Tier 1 / Tier 2;
see `benches/runner/HARDWARE.md`).

## Gate exit codes

| Exit | Meaning                                                                                                                                                                                                                 |
| ---- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0    | **Pass** — ≥ 1 Win, 0 Loss, no `Incomplete`, both signatures parse as `os=linux tier=1`. ROADMAP:829 satisfied.                                                                                                         |
| 1    | **Fail** — Mango lost on a non-asymmetric metric, OR no metric reached Win, OR any `Incomplete`.                                                                                                                        |
| 2    | **Structural error** — Schema mismatch, missing or non-co-resident signature, non-Tier-1 signature, unknown metric, format_version mismatch, workload-hash mismatch. The bench did not run cleanly enough to be graded. |

The full B4 loss-magnitude decision tree (which exit door a failed
run takes) is in `benches/results/phase-1/README.md`.

## Tests

The crate ships **102 lib tests**:

- 16 in `workload` (toml parsing, schema validation, the canonical
  on-disk toml regression test)
- 14 in `measure` (histogram pin, V2-deflate round-trip, JSON shape)
- 15 in `stats` (bootstrap CI, paired-difference invariants,
  `HigherIs` polarity)
- 13 in `zipfian` (distribution shape, scrambled mapping, theta pin)
- 7 in `dropcache` (capability probe, verdict mapping)
- 9 in `mango_runner` (op set, error propagation)
- 6 in `bbolt_runner` (JSON-line protocol, error propagation,
  process lifecycle)
- 22 in `gate` (verdict tree, signature parsing, fairness exclusion,
  `Incomplete` failure path, structural errors)

Run them with:

```bash
cargo test -p mango-bench-storage --lib
```

The crate is excluded from `default-members` in the workspace
`Cargo.toml`; `cargo build` / `cargo test` from the workspace root
skips it. Build it explicitly with `-p mango-bench-storage`.

## See also

- `.planning/parity-bench-harness.plan.md` — full design rationale,
  S1 / S2 / S3 / N9 / N10 numbering used in the module-level docs.
- `benches/results/phase-1/README.md` — operator manual for
  result-pair JSONs and the loss-magnitude decision tree.
- `benches/oracles/bbolt/BBOLT_QUIRKS.md` — divergences from default
  bbolt usage in the oracle (S3 force-copy, fsync fairness, …).
- `benches/oracles/bbolt/README.md` — oracle build + protocol.
- `benches/runner/HARDWARE.md` — Tier 1 / Tier 2 acceptance classes
  and the signature schema.
- `benches/README.md` — broader bench infrastructure (etcd oracle,
  hardware-signature wrapper, supply-chain provenance).
