# 0.10 — Bench oracle harness scaffold

## Roadmap item

> Bench oracle harness scaffold: `benches/oracles/etcd/` checks in a script that downloads etcd v3.5.x at a pinned version + sha256, plus `benches/runner/HARDWARE.md` documenting the canonical hardware spec we run comparisons on, plus `benches/runner/run.sh` that prints a hardware signature alongside every result. Without this, every later "beats etcd by Nx" claim has no oracle. `HARDWARE.md` MUST declare two hardware tiers: (a) Tier 1 / single-node bench rig — single host, NVMe SSD, ≥ 16 cores, ≥ 64 GB RAM, sufficient for Phases 2–13 single-node and 3-node benches; (b) Tier 2 / multi-node fleet — ≥ 10 hosts (5 voters + 5 learners), ≥ 25 GbE intra-cluster bandwidth, root access for `tc` / `iptables` (or a `toxiproxy` install) to drive the Phase 14.5 chaos tests.

## Why

This is **scaffold only**. No benchmarks run yet — Phase 2+ writes those. The point is to check in the oracle (etcd download + verify) and the provenance apparatus (HARDWARE.md tiers, hardware-signature printer) _before_ anyone publishes a single throughput number, so when comparisons start landing in Phases 2–14, every result has a verifiable oracle version and the tier/hardware is on the page.

If we let this slip, the first "beats etcd by Nx" PR arrives and we improvise an oracle under deadline pressure. That's how you end up with numbers that don't reproduce.

## Revisions from rust-expert review (REVISE → APPROVE gate)

This plan is the post-review revision. Showstoppers S1/S2/S3 and bugs B1–B4 are folded in; risk-level tier additions (R2) and signature-field additions (R1) are also folded. Summary of what changed vs. the initial draft:

- **S1**: cross-platform sha256 helper (`sha256_of()`) branching on `uname -s` — `sha256sum` on Linux, `shasum -a 256` on macOS. Never hardcode `sha256sum`.
- **S2**: signature format is now versioned (`BENCH_HW v1:`), fields are **sorted lexically by key**, values are shell-escaped, joined by single spaces with no trailing newline. `sha` is computed over the canonical form _excluding_ the `sha=` field. Field values are trimmed of leading/trailing whitespace before hashing (defends against `/proc/cpuinfo` `model name` tab-padding).
- **S3**: tests now exercise `VERSIONS` parsing and platform selection, including the `linux/aarch64` → `linux/arm64` normalization that `uname -m` produces.
- **B1**: CI `bench-harness` job asserts `git diff --exit-code benches/oracles/etcd/VERSIONS` after sourcing it (script must not mutate VERSIONS).
- **B2**: local-pin threat model documented as TOFU in `benches/oracles/etcd/README.md`. We also pin the `SHA256SUMS` file's own sha in `VERSIONS` (defense-in-depth, two independent hashes at pin time).
- **B3**: Test 2 determinism claim weakened to "same shell, same boot, within 5 seconds."
- **B4**: `run.sh` emits the signature to **stderr** (so stdout stays clean for JSON bench output) AND writes it to a sidecar file `$BENCH_OUT_DIR/signature.txt` when `BENCH_OUT_DIR` is set.
- **R1**: drop `nvme` field (misleading — SATA SSDs also have `rotational=0`). Replace with `storage=<device-model>` from `lsblk -d -o MODEL` / `diskutil info`. Leave TODO for `fsync_us_p50` (Phase 2+).
- **R2**: Tier 1 now requires documented turbo/frequency pinning, isolcpus, TSC invariance (`constant_tsc + nonstop_tsc`), queue scheduler, memory channel count. Tier 2 adds a clock-sync requirement (PTP preferred, chrony fallback). All captured in signature fields: `tsc=`, `turbo=`, `mem_channels=`, `scheduler=`, `cpu_mhz_max=`, `virt=`, `storage=`.
- **R4**: `BENCH_TIER` unset is a hard error when argv contains the token `bench`, soft warning otherwise.
- **R5**: concrete Rust-port trigger named: any of (third target OS, first structured field like NUMA/hugepages/cgroup/PMU, `hardware-signature.sh` passing ~300 non-comment lines). Earlier "~150 lines" draft trigger was raised to 300 after the PR review flagged self-contradiction against the ADR's "~300 lines total" line count. See ADR decision 1.
- **M2/M3**: new Tests 4 and 5 — VERSIONS ↔ HARDWARE.md platform coverage, and canonicalization round-trip.
- **M5**: ADR committed at `.planning/adr/0001-bench-oracle-harness.md`.

## Files

### New

```
benches/oracles/etcd/fetch.sh            # download + sha256-verify etcd v3.5.x tarball
benches/oracles/etcd/VERSIONS            # pinned etcd version + sha256 (per OS/arch, plus SHA256SUMS hash)
benches/oracles/etcd/.gitignore          # ignore downloaded tarballs and unpacked dirs
benches/oracles/etcd/README.md           # TOFU threat model + usage
benches/runner/HARDWARE.md               # Tier 1 + Tier 2 hardware spec
benches/runner/run.sh                    # emits signature (stderr + sidecar), execs argv
benches/runner/hardware-signature.sh     # library that emits the signature line
benches/runner/hwsig-lib.sh              # portable helpers: sha256_of, uname_arch_normalize, sysinfo
benches/README.md                        # one-page overview of oracle + runner layout
.planning/adr/0001-bench-oracle-harness.md  # decisions behind shell-over-Rust, TOFU, env-var tier, signature format
scripts/test-bench-oracle-fetch.sh       # Test 1 (+ Test 4 platform coverage)
scripts/test-hardware-signature.sh       # Test 2 (+ Test 5 canonicalization round-trip)
scripts/test-bench-run-wrapper.sh        # Test 3
```

### Modified

- `.github/workflows/ci.yml` — new `bench-harness` job.

No `Cargo.toml` changes (the harness is shell, not a Rust bench crate yet; cargo-bench integration lands in Phase 2 when there's actually something to measure).

## etcd version pin

- **Re-verify the patch version at commit time.** The roadmap says v3.5.x. Draft pin: v3.5.17 (latest as of plan draft); implementer picks whatever is the current latest v3.5.z on the day of commit and updates `VERSIONS` accordingly.
- Official release URL format:
  ```
  https://github.com/etcd-io/etcd/releases/download/<ETCD_VERSION>/etcd-<ETCD_VERSION>-<os>-<arch>.tar.gz
  ```
- We commit **two independent hashes** pinned at plan-commit-time:
  1. The per-platform tarball sha256 (the thing we actually run).
  2. The `SHA256SUMS` file's own sha256 (defense-in-depth: if someone later substitutes the release, both the tarball hash and the checksum-file hash would have to collide, which requires compromise _before_ our pin was taken).

`fetch.sh` downloads the tarball, verifies its sha against `VERSIONS`, then downloads `SHA256SUMS` and verifies _its_ sha against `VERSIONS`, then cross-checks that `SHA256SUMS` contains the same tarball hash we pinned. Any disagreement → exit 1.

`VERSIONS` format (KEY=VALUE, shell-sourceable):

```
ETCD_VERSION=v3.5.17
ETCD_SHA256_linux_amd64=<hex>
ETCD_SHA256_linux_arm64=<hex>
ETCD_SHA256_darwin_amd64=<hex>
ETCD_SHA256_darwin_arm64=<hex>
ETCD_SHA256SUMS_SHA256=<hex>   # sha256 of the SHA256SUMS file itself, pinned at commit time
```

`fetch.sh` resolves platform with a `uname_arch_normalize()` helper in `hwsig-lib.sh` that maps `uname -m` output (`x86_64|amd64 → amd64`, `aarch64|arm64 → arm64`) to the etcd release naming. If a platform isn't pinned, fail with a message naming the missing key and pointing at `VERSIONS`.

## Threat model (documented in `benches/oracles/etcd/README.md`)

Local pin is **TOFU (trust-on-first-use)** against post-publication compromise of the etcd GitHub release. It does NOT protect against an attacker who compromised the release _before_ we took the pin. The two-hash scheme (tarball hash + SHA256SUMS file hash) narrows the window: an attacker would have to substitute both artifacts with content that hashes to our pinned values, which is cryptographically infeasible.

"Self-authenticating signature line" (`sha` field) is **tamper-evident against accidental corruption** (copy-paste, Markdown munging, grep-replace across result files), not a security property. A malicious actor editing the signature would just recompute the hash. Real attestation (TPM quote, CI-bot signatures) is out of scope; it's Phase 12+ territory.

## `benches/runner/HARDWARE.md` — two tiers

### Tier 1 — single-node bench rig (Phases 2–13 single-node + 3-node)

Hardware:

- 1 host
- ≥ 16 physical cores (32 vCPU acceptable for SMT)
- ≥ 64 GB RAM
- NVMe SSD, ≥ 500 GB free
- ≥ 2 memory channels populated (DDR4 or DDR5), ≥ 4 preferred for Raft replay paths
- Linux kernel ≥ 5.15 **or** macOS 14+ on Apple Silicon (M1 Pro/Max/Ultra, M2, M3 acceptable)

Operator-configured (Linux-only where noted):

- No swap during benches (`swapoff -a`) — Linux
- CPU governor: `performance` — Linux
- Turbo/frequency pinning: `intel_pstate=disable` or `cpupower frequency-set -g performance -u <max>` (records max freq in signature) — Linux
- Isolated cores for tail-latency benches: `isolcpus=4-15 nohz_full=4-15 rcu_nocbs=4-15` — Linux, **required** for Phase 2+ latency benches, optional for throughput benches (but must be declared in the signature either way)
- Block device scheduler: `mq-deadline` (or `none` for NVMe where the device manages its own queue); `nr_requests=1024`
- TSC flags: `constant_tsc` AND `nonstop_tsc` present in `/proc/cpuinfo`. If either is missing, `Instant::now()` is unreliable and **this host is not Tier 1** — document and move on.

### Tier 2 — multi-node fleet (Phase 14.5 chaos, Phase 12 release gate)

- ≥ 10 hosts, each meeting Tier 1 spec
- ≥ 5 voter nodes + ≥ 5 learner/follower nodes
- ≥ 25 GbE intra-cluster bandwidth, measured with `iperf3`, recorded in the run log
- Root access for `tc qdisc` / `iptables` (or a `toxiproxy` install on each host)
- All hosts in one physical rack / one AZ (cross-AZ benches are a separate tier, out of scope for Phase 14.5)
- **Clock sync**: PTP via `ptp4l` + `phc2sys` (preferred) OR chronyd with `maxpoll 4` against a rack-local NTP server. Cross-host drift must be < 1ms during the bench window, asserted in pre-roll.

### Tier contamination rules

- Every host in a multi-node bench emits its **own** signature. Analysis tools MUST assert all signatures agree on `cpu`, `cores`, `ram_gb`, `storage`, `tier` before reporting aggregate numbers.
- Running Tier 1 benches on a host from a Tier 2 fleet is allowed; the operator sets `BENCH_TIER=1` and accepts responsibility for isolating the host.
- `BENCH_TIER` set to a value other than `1` or `2` → hard error.

Every run MUST record which tier it was on. A number without a tier annotation is not a number.

## Hardware signature

`hardware-signature.sh` emits one line per run, self-contained, grep-friendly:

```
BENCH_HW v1: arch=amd64 cores=64 cpu=AMD\ EPYC\ 7B13 cpu_mhz_max=3500 kernel=6.1.0 mem_channels=8 os=linux ram_gb=256 scheduler=mq-deadline storage=Samsung\ 980\ Pro tier=1 tsc=invariant turbo=disabled virt=bare-metal sha=<digest>
```

### Canonical form (S2 resolution)

- Fields sorted **lexically by key name** (e.g., `arch` before `cores` before `cpu`, alphabetical).
- Values shell-escaped: spaces → `\ `, quotes → `\"`. This avoids the ambiguity of `"quoted value"` vs `unquoted`.
- Single-space separator between fields. No trailing space, no trailing newline in the hashed input.
- Each value stripped of leading/trailing whitespace and tabs before escaping.
- Version prefix `BENCH_HW v1: ` is **not** part of the hashed input; it's a display header.
- `sha=<digest>` is the last field in the printed line but is **not** included in its own hash (circular).

Hash computation:

```
canonical=$(print all fields except sha=, sorted by key, escaped, space-joined)
sha=$(printf '%s' "$canonical" | <sha256_of> | cut -d' ' -f1)
printf 'BENCH_HW v1: %s sha=%s\n' "$canonical" "$sha"
```

### Fields

| Field          | Source (Linux)                                                                                                       | Source (macOS)                              | Notes                                         |
| -------------- | -------------------------------------------------------------------------------------------------------------------- | ------------------------------------------- | --------------------------------------------- | -------------------------------------------------- | -------------- | --- |
| `arch`         | `uname -m` → normalize                                                                                               | `uname -m` → normalize                      | `x86_64                                       | amd64 → amd64`; `aarch64                           | arm64 → arm64` |
| `cores`        | `nproc`                                                                                                              | `sysctl -n hw.physicalcpu`                  | Physical cores, not SMT                       |
| `cpu`          | `/proc/cpuinfo` `model name` (first occurrence, trimmed)                                                             | `sysctl -n machdep.cpu.brand_string`        | Strip trailing tabs/spaces                    |
| `cpu_mhz_max`  | `lscpu` `CPU max MHz` or `/proc/cpuinfo` `cpu MHz`                                                                   | `sysctl -n hw.cpufrequency_max` / 1_000_000 | `0` if unavailable                            |
| `kernel`       | `uname -r`                                                                                                           | `uname -r`                                  |                                               |
| `mem_channels` | `dmidecode -t memory` parse, or `0` if unavailable / not root                                                        | `0` (not reliably queryable)                | Documented as "best effort"                   |
| `os`           | `uname -s` lowercased                                                                                                | `uname -s` lowercased                       | `linux                                        | darwin`                                            |
| `ram_gb`       | `/proc/meminfo` `MemTotal` (kB) / (1024\*1024)                                                                       | `sysctl -n hw.memsize` (bytes) / (1024^3)   | Integer rounding                              |
| `scheduler`    | `cat /sys/block/<primary>/queue/scheduler`                                                                           | `unknown`                                   | Linux-only                                    |
| `storage`      | `lsblk -d -o MODEL -n <primary>`                                                                                     | `diskutil info /                            | grep "Device / Media Name"`                   | Replace spaces with `\ `                           |
| `tier`         | `$BENCH_TIER`                                                                                                        | `$BENCH_TIER`                               | `1` or `2`; unset → hard-or-soft error per R4 |
| `tsc`          | `grep -q constant_tsc /proc/cpuinfo && grep -q nonstop_tsc /proc/cpuinfo && echo invariant                           |                                             | echo variable`                                | `invariant` (Apple Silicon and recent Intel Macs)  |                |
| `turbo`        | `/sys/devices/system/cpu/intel_pstate/no_turbo` if present; `disabled` if `1`, `enabled` if `0`, `unknown` otherwise | `unknown`                                   |                                               |
| `virt`         | `systemd-detect-virt 2>/dev/null                                                                                     |                                             | echo bare-metal`                              | `sysctl -n kern.hv_vmm_present 2>/dev/null` → `vmm | bare-metal`    |     |

"Best effort" fields (`mem_channels`, `scheduler` on macOS, `cpu_mhz_max` fallback) are honestly reported as `unknown` or `0`. They do NOT derive a warning — the point is that the signature captures what's knowable, not that every field is always known.

## Runner wrapper (`run.sh`) — B4 resolution

`run.sh` does the following in order:

1. Compute the signature via `hardware-signature.sh`.
2. Emit the signature line to **stderr** (so stdout remains a clean channel for the wrapped command).
3. If `BENCH_OUT_DIR` is set, also write the signature to `$BENCH_OUT_DIR/signature.txt`. Phase 2+ tools read this sidecar to correlate results.
4. `exec "$@"` — forward all argv as-is. The wrapped command's exit code is the script's exit code.

### Tier-hard-fail rule (R4)

If `BENCH_TIER` is unset AND argv contains the token `bench` (as in `cargo bench`, `--bench name`), `run.sh` exits 2 with a message on stderr. Any other argv proceeds with `tier=unknown` and a warning. Rationale: smoke tests (`run.sh echo hello`) don't need a tier; real benches always do.

## Tests — MANDATORY per project rule

Five shell-only tests. All portable to Linux and macOS via `hwsig-lib.sh`. Wired into CI as `bench-harness`.

### Test 1: `fetch.sh` `verify_sha` function

`scripts/test-bench-oracle-fetch.sh`:

- Source `fetch.sh` to get the `verify_sha` function.
- Create a temp file with known content, compute its sha with the portable helper.
- Assert `verify_sha $file $correct_sha` exits 0.
- Assert `verify_sha $file $wrong_sha` exits non-zero with an error message that names the file path.
- Does NOT hit the network.

### Test 2: `hardware-signature.sh` emits a well-formed line

`scripts/test-hardware-signature.sh` (part A):

- Run `hardware-signature.sh` with `BENCH_TIER=1`.
- Assert the output matches the regex: `^BENCH_HW v1: ([a-z_]+=[^ ]+ )+sha=[0-9a-f]{64}$`.
- Assert fields are alphabetically sorted (split on space, check each key ≤ next key).
- Assert running again within 5 seconds produces an identical line (weakened determinism — B3).
- Run with `BENCH_TIER` unset and argv not containing `bench`: expect `tier=unknown` and a stderr warning.
- Run with `BENCH_TIER=3`: expect exit 1 with "invalid tier" message.

### Test 3: `run.sh` stderr signature + stdout cleanliness (B4)

`scripts/test-bench-run-wrapper.sh`:

- Run `BENCH_TIER=1 run.sh echo hello world 2>/tmp/sig 1>/tmp/out`.
- Assert `/tmp/sig` contains `BENCH_HW v1:` as the first line.
- Assert `/tmp/out` is exactly `hello world\n` (no signature prepended).
- Run with `BENCH_OUT_DIR=$(mktemp -d)` and assert the sidecar file exists and matches the stderr signature.
- Run `BENCH_TIER= run.sh cargo bench --bench foo` (won't actually exec if cargo/bench not wired) — expect exit 2 before exec, with "BENCH_TIER required when running benches" stderr.
- Run `run.sh false` — expect exit 1 (propagated), signature still printed to stderr.

### Test 4: `VERSIONS` ↔ `HARDWARE.md` platform coverage (M2)

`scripts/test-bench-oracle-fetch.sh` (part B):

- For each `os/arch` in `HARDWARE.md`'s supported list (parsed from a machine-readable block in the markdown), assert `VERSIONS` has a matching `ETCD_SHA256_<os>_<arch>=<non-empty-hex>` line.
- For each `ETCD_SHA256_<os>_<arch>` in `VERSIONS`, assert that platform appears in `HARDWARE.md`'s supported list.
- Catches drift: adding a platform to HARDWARE.md without pinning a sha, or vice versa.

### Test 5: Canonicalization round-trip (M3)

`scripts/test-hardware-signature.sh` (part B):

- Capture a signature line.
- Extract the `sha=` field; reconstruct the canonical form from the rest; feed to the portable hasher; compare.
- If the recomputed digest differs from the printed one, the canonicalization is inconsistent with the hashing — fail with a diff.
- This test catches future field additions that forget to update the canonicalization logic.

### CI integration

New `bench-harness` job in `.github/workflows/ci.yml`:

- `runs-on: ubuntu-24.04`, `timeout-minutes: 5`.
- `actions/checkout@<pinned sha>` only. No Rust toolchain, no cache action (no build).
- Step 1: `bash scripts/test-bench-oracle-fetch.sh`
- Step 2: `bash scripts/test-hardware-signature.sh`
- Step 3: `bash scripts/test-bench-run-wrapper.sh`
- Step 4 (B1): `git diff --exit-code benches/oracles/etcd/VERSIONS` — verifies no test mutated the committed VERSIONS file.

macOS coverage: the three scripts are portable, but CI only runs Linux. A manual `./scripts/test-*.sh` run on macOS is required as part of the Phase 2+ bench PRs and is documented in `benches/README.md`. (A macOS CI matrix is Phase 0.5+ territory — too heavy for this scaffold.)

## Non-goals for this PR

- No actual benchmarks. Phase 2+ adds them.
- No cargo-bench integration. Same — Phase 2+.
- No automated tier-1 vs tier-2 hardware detection. `BENCH_TIER` is a human-set env var.
- No Windows support. Roadmap scope is Linux + macOS. `fetch.sh` fails loudly on other platforms.
- No etcd _running_. We check the binary unpacks and the version matches. Actual cluster wiring is the Phase 2 bench's problem.
- No cosign / release signing. Future hardening pass.
- No NUMA / hugepages fields in the signature. Adding these is the trigger for the Rust port per R5.
- No macOS CI matrix. Phase 0.5+ when loom / madsim justify the extra expense.
- No ADR for every platform variant. The one ADR at `.planning/adr/0001-bench-oracle-harness.md` captures the load-bearing decisions; per-platform notes live in `benches/README.md`.

## Rollback plan

### Mechanical rollback

`git rm -rf benches/ scripts/test-bench-*.sh scripts/test-hardware-signature.sh .planning/adr/0001-bench-oracle-harness.md` and remove the `bench-harness` CI job. No runtime code, no dependencies — mechanically clean.

### Dependency graph (important)

Once Phase 2 ships a comparison bench, rolling this back also requires:

- Reverting every `benches/results/` file that includes a `BENCH_HW v1:` signature.
- Reverting every ROADMAP.md cross-reference to `benches/runner/HARDWARE.md`.
- Reverting Phase 12's release gate (which consumes tier declarations).

In practice, **this scaffold becomes a one-way door at the first Phase 2 bench PR.** Name it here so future-us doesn't accidentally try.

## Follow-ups (documented, not shipped in this PR)

- **Phase 2 first-bench PR MUST include** an end-to-end smoke test that runs `fetch.sh`, unpacks, executes `etcd --version`, asserts version match with `ETCD_VERSION`. Track in M6 note in `benches/README.md`.
- **Phase 0.14 CONTRIBUTING.md MUST reference** `benches/runner/HARDWARE.md` when describing how to run benches locally.
- **R4 enforcement** — `bench-harness` is currently an advisory CI check because `main` branch protection isn't enabled yet (tracked from PR #16 review, R4 there). When branch protection lands, add `bench-harness` to the required-checks list.
- **Signature format v2** — NUMA / hugepages / cgroup-awareness fields will trigger the shell-to-Rust migration per R5. Signature version bumps to `BENCH_HW v2:` at that point.
- **`--no-download` / cache mode** for `fetch.sh` — CI environments that pre-populate a mirror. Added when the first CI matrix that needs it lands (Phase 2+).

## Expected rust-expert feedback (anticipated, post-revision)

The REVISE round caught the real issues. Remaining likely pushback:

1. **"Why pin the SHA256SUMS hash separately instead of signing the tarball?"** — signing requires a trusted key; we don't have one. Two-hash TOFU is the honest floor. Cosign + sigstore when the Phase 12 release-attestation work happens.
2. **"Tier 1 isolcpus requirement is going to turn away contributors on laptops."** — laptops are fine for _throughput_ benches; tail-latency benches demand isolcpus. HARDWARE.md calls this out explicitly so contributors don't run latency benches on contended cores and ship a bad number.
3. **"Signature field list will grow."** — versioned (`v1` → `v2`), and R5 names the concrete trigger for the shell→Rust port. Growth is planned.
4. **"Why stderr and not stdout for the signature?"** — stdout must stay clean for JSON-producing bench tools (criterion `--output-format json`, hyperfine `--export-json`). This is the B4 fix. Sidecar file for downstream tooling that wants to correlate.
