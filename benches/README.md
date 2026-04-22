# mango bench harness

Scaffold for bench comparisons against etcd. No benchmarks run from
this directory yet — Phase 2+ writes those. The point is to ship the
provenance apparatus (oracle pin, hardware tiers, hardware signature)
_before_ any "beats etcd by Nx" claim appears in the codebase so
every claim has a verifiable floor.

## Layout

```
benches/
├── README.md                    # this file
├── oracles/
│   └── etcd/
│       ├── fetch.sh             # download + sha256-verify etcd tarball
│       ├── VERSIONS             # pinned etcd version + sha256 hashes
│       ├── .gitignore           # keep downloaded artifacts out of git
│       └── README.md            # etcd-specific usage + threat model
└── runner/
    ├── HARDWARE.md              # Tier 1 + Tier 2 requirements
    ├── hardware-signature.sh    # emits the BENCH_HW v1: ... line
    ├── hwsig-lib.sh             # portable helpers (sha256, arch norm)
    └── run.sh                   # wraps a command, emits signature
```

## Two things this harness gives you

1. **An oracle you can reproduce.** `benches/oracles/etcd/fetch.sh`
   downloads a pinned etcd release and verifies two independent
   sha256 hashes against a locally committed value. If the download
   doesn't match, the script exits non-zero — the bench does not
   proceed on an uncertain oracle.

2. **A hardware provenance record.** `benches/runner/run.sh` wraps
   any bench command, emits a canonical single-line hardware
   signature to stderr (and a sidecar file if `BENCH_OUT_DIR` is
   set), and forwards the command. Every result that mango
   publishes carries this line so readers can tell what box it
   was run on.

## Running the harness

```bash
# Download + verify the etcd oracle for the current platform.
benches/oracles/etcd/fetch.sh
# → writes benches/oracles/etcd/cache/etcd-vX.Y.Z-<os>-<arch>.{tar.gz,zip}
#   and prints the path on success.

# Print a bare hardware signature.
BENCH_TIER=1 benches/runner/hardware-signature.sh

# Wrap a bench command.
BENCH_TIER=1 benches/runner/run.sh cargo bench --bench my_bench
# stdout: whatever cargo bench prints
# stderr: BENCH_HW v1: arch=... sha=<digest>

# Capture a sidecar signature file for downstream tooling.
BENCH_TIER=1 BENCH_OUT_DIR=out/run-1 benches/runner/run.sh <cmd>
# → out/run-1/signature.txt contains the signature line
```

## `BENCH_TIER` enforcement

- Running a bench (any argv containing `bench` or `cargo bench` or
  `--bench`) with `BENCH_TIER` unset is a hard error (exit 2) from
  `run.sh`. This is the point — unsigned numbers are not numbers.
- Running any other command with `BENCH_TIER` unset is a soft
  warning; the signature reports `tier=unknown`.
- `BENCH_TIER` set to anything other than `1` or `2` is a hard error.

## The signature line

```
BENCH_HW v1: arch=amd64 cores=64 cpu=AMD\ EPYC\ 7B13 cpu_mhz_max=3500 kernel=6.1.0 mem_channels=8 os=linux ram_gb=256 scheduler=mq-deadline storage=Samsung\ 980\ Pro tier=1 tsc=invariant turbo=disabled virt=bare-metal sha=<64-hex>
```

**Canonical form (v1):**

- Fields sorted lexically by key.
- Values shell-escaped (`\ ` for spaces, `\\` for backslashes).
- Single space between fields, no trailing space, no trailing newline
  in the hashed bytes.
- `sha=` is the last printed field but is NOT in its own input.

**What the `sha` buys you:** tamper-evident detection of accidental
corruption (copy-paste, markdown munging, sed scripts across result
files). It is NOT a security property — an adversary editing the line
would just recompute the hash. See
`benches/oracles/etcd/README.md` for the (different) threat model
around the etcd pin.

**Version discipline:** adding a field (NUMA, hugepages, cgroup mode)
requires bumping `v1` to `v2` so old signatures remain interpretable.
Dropping a field is effectively never compatible.

## Follow-ups (tracked, not shipped here)

- **Phase 2 first-bench PR MUST include** a smoke test that invokes
  `fetch.sh`, unpacks the artifact, runs `etcd --version`, and
  asserts the version matches `ETCD_VERSION`. This is the
  end-to-end gate we do not exercise from the scaffold.
- **CONTRIBUTING.md (Phase 0 item 0.14)** must reference this
  directory when describing how to run local benches.
- **Branch protection:** `bench-harness` CI job is advisory until
  `main` is protected and required-checks include it (tracked from
  previous PR review).
- **Signature v2 trigger:** when `hardware-signature.sh` exceeds
  150 lines OR the first NUMA / hugepages / cgroup-aware field is
  requested, port the signature emitter to a Rust binary
  (`benches/runner/hwsig`) and keep the shell version as a
  reference implementation during the transition.

## macOS coverage

CI only runs the harness on Ubuntu 24.04. The scripts are portable
to macOS (via `shasum -a 256` instead of `sha256sum`, `sysctl`
instead of `/proc/*`) and anyone bumping the harness must run
`scripts/test-*.sh` on both macOS and Linux locally before
proposing the change. A macOS CI matrix is Phase 0.5+ territory —
too heavy an ask for the scaffold.
