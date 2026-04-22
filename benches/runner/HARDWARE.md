# Bench hardware tiers

Every bench result mango publishes — in ROADMAP.md, in `benches/results/`,
in release notes, or in a paper — MUST carry a declared hardware tier
and the matching hardware signature line (see
`benches/runner/hardware-signature.sh`). A number without a tier
annotation is not a number.

This document defines two tiers. They are the envelope: a rig meeting
Tier 1 is allowed to produce single-node and 3-node numbers; a fleet
meeting Tier 2 is allowed to produce multi-node / chaos / release-gate
numbers. Running a Tier 2 acceptance bench on a Tier 1 rig produces an
invalid result.

## Supported platforms

The bench harness supports Linux and macOS on amd64 and arm64. Keep
this table in sync with `benches/oracles/etcd/VERSIONS` — the drift
test in `scripts/test-bench-oracle-fetch.sh` enforces that every
supported platform has a pinned etcd hash.

| OS     | Arch  |
| ------ | ----- |
| linux  | amd64 |
| linux  | arm64 |
| darwin | amd64 |
| darwin | arm64 |

## Tier 1 — single-node bench rig

Used for Phases 2–13 single-node and 3-node bench results.

### Hardware requirements

- 1 host.
- ≥ 16 physical cores (32 vCPU with SMT acceptable).
- ≥ 64 GB RAM.
- NVMe SSD, ≥ 500 GB free.
- ≥ 2 memory channels populated (DDR4 or DDR5). ≥ 4 strongly preferred
  for the Raft log-replay path, which is memory-bandwidth-bound.
- Linux kernel ≥ 5.15, **or** macOS 14+ on Apple Silicon
  (M1 Pro/Max/Ultra, M2, M3, M4 — recorded in the signature as
  `cpu=Apple M*`).

### Operator-configured (Linux, mandatory)

- No swap during benches: `swapoff -a`.
- CPU governor `performance`: `cpupower frequency-set -g performance`.
- Turbo/frequency pinning: `intel_pstate=disable` in the kernel
  cmdline or `cpupower frequency-set -u <max>`. The signature records
  `turbo=` and `cpu_mhz_max=` so results can be filtered by state.
- TSC flags: `/proc/cpuinfo` MUST report both `constant_tsc` and
  `nonstop_tsc`. If either is missing, `Instant::now()` is unreliable
  on this host and **this rig is not Tier 1** — do not use it.
  The signature records `tsc=invariant|variable`.

### Operator-configured (Linux, bench-class dependent)

- **Isolated cores (required for tail-latency benches):**
  `isolcpus=4-15 nohz_full=4-15 rcu_nocbs=4-15` (adjust range to the
  rig's core count). Throughput benches MAY skip this but the
  signature will still report the scheduler state.
- **Block device scheduler:** `mq-deadline` (or `none` for NVMe where
  the device manages its own queue). `nr_requests=1024` on the root
  block device. Signature field: `scheduler=`.

### Operator-configured (macOS, best-effort)

macOS does not expose CPU-governor, turbo, or scheduler knobs the
same way Linux does. The signature reports `turbo=unknown`,
`scheduler=unknown`, `mem_channels=0` on Darwin by design; the
signature is honest about what is knowable. macOS on Apple Silicon
has invariant TSC (reported as `tsc=invariant`) and that is the
load-bearing property for `Instant` math.

macOS Tier 1 is acceptable for single-node throughput and
single-latency benches. It is NOT acceptable for Tier 2 or for the
Phase 12 release gate.

## Tier 2 — multi-node fleet

Used for Phase 14.5 chaos tests and the Phase 12 release gate.
Available only on Linux.

### Hardware requirements

- ≥ 10 hosts, each meeting Tier 1 spec.
- ≥ 5 voter nodes + ≥ 5 learner/follower nodes.
- ≥ 25 GbE intra-cluster bandwidth, measured with `iperf3` at the
  start of every run and recorded in the run log.
- Root access on every host for `tc qdisc` and `iptables` (or a
  `toxiproxy` install on each host — pick one fault-injection
  mechanism per run).
- All hosts in one physical rack / one AZ. Cross-AZ benches are a
  separate tier, out of scope for this document.

### Clock synchronization (required)

- PTP via `ptp4l` + `phc2sys` preferred; chronyd with `maxpoll 4`
  against a rack-local NTP server is an acceptable fallback.
- Cross-host drift MUST be `< 1ms` during the bench window.
- Pre-roll: every host samples `chronyc tracking` or
  `pmc -u -b 0 'GET CURRENT_DATA_SET'`; max offset is logged.

### Bandwidth measurement

Every run records the result of `iperf3 -c <neighbor> -t 10` between
each voter pair (or a representative mesh). Numbers below 20 Gbit/s
on a 25 GbE fabric are a sign of link-layer degradation and should
invalidate the run.

## Tier contamination rules

- **Every host in a multi-node bench emits its own signature.**
  Analysis tools MUST assert all host signatures agree on `cpu`,
  `cores`, `ram_gb`, `storage`, and `tier` before aggregating. A
  mixed-tier cluster silently averages into a nonsense number.
- **Single-node benches on a Tier 2 host are allowed**, with
  `BENCH_TIER=1` and operator-asserted isolation (no other bench
  workload on neighbors during the run). Document the isolation
  assertion in the result file.
- **`BENCH_TIER=3` or any value outside `{1, 2}` is a hard error**
  enforced by `hardware-signature.sh`.
- **`BENCH_TIER` unset when running an actual bench is a hard error**
  enforced by `run.sh`. It is a soft warning (signature shows
  `tier=unknown`) when wrapping non-bench commands for diagnostics.

## Declaring a run

1. Set `BENCH_TIER=1` or `BENCH_TIER=2`.
2. Optionally set `BENCH_OUT_DIR=path/to/results` — the runner will
   write a `signature.txt` there.
3. Invoke the bench via `benches/runner/run.sh <command...>`.
4. The signature goes to stderr (stdout stays clean for
   criterion/hyperfine JSON output); the sidecar file carries the
   same content for downstream correlation.

Example:

```bash
BENCH_TIER=1 BENCH_OUT_DIR=out/throughput \
    benches/runner/run.sh cargo bench --bench write_throughput -- --baseline main
```

## Changing this document

This document's requirements feed into the Phase 12 release gate.
Additions (new signature fields, new tier requirements) MUST bump the
signature version (`BENCH_HW v1:` → `BENCH_HW v2:`) so old results
remain interpretable. Removing requirements is effectively never
compatible — open an RFC before relaxing a tier.
