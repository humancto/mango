# ADR 0001 — Bench oracle harness scaffold

Status: Accepted (ROADMAP item 0.10)
Date: 2026-04-23

## Context

mango's roadmap promises comparisons against etcd ("beats etcd by Nx"
claims in Phases 2–14 and the Phase 12 release gate). Comparisons
require:

- A specific etcd version, reproducibly downloadable.
- A declared hardware class so two people can interpret the same
  number the same way.
- A provenance record on every result file so numbers that get
  copy-pasted into ROADMAP, release notes, or papers carry their
  origin with them.

Without any of these, the first throughput claim arrives and we
improvise the scaffold under deadline pressure, which is how projects
end up with numbers that don't reproduce.

## Decision

Land a scaffold-only PR with:

1. **Pinned etcd oracle** in `benches/oracles/etcd/` — `VERSIONS` with
   per-platform sha256 plus the sha of the release's `SHA256SUMS`
   file, and `fetch.sh` that verifies all three paths (tarball sha,
   SHA256SUMS sha, SHA256SUMS↔VERSIONS cross-check).
2. **Two hardware tiers** in `benches/runner/HARDWARE.md` — Tier 1
   single-node (Phases 2–13), Tier 2 multi-node (Phase 14.5 chaos +
   Phase 12 release gate).
3. **A canonical hardware signature** (`BENCH_HW v1:`) emitted by
   `benches/runner/hardware-signature.sh` and prepended to every bench
   run by `benches/runner/run.sh`.

No benchmarks run from this PR. Phase 2+ writes them and consumes the
scaffold.

## Load-bearing decisions

### 1. Shell, not Rust

The harness is plumbing around shell-native concerns (`curl`,
`sha256sum`/`shasum`, `sysctl`, `/proc/cpuinfo`, `lsblk`, `diskutil`).
Rewriting those in Rust is ceremony without payoff at this scale
(~300 lines total). Reading shell-pipeline output _from_ Rust would
still require most of the same platform branches, just with more
surface area.

**Trigger to port to Rust:** when `hardware-signature.sh` exceeds
~150 lines OR the first NUMA / hugepages / cgroup-aware field is
requested. At that point the shell version becomes a reference
implementation during the transition, and the Rust version lives at
`benches/runner/hwsig`.

### 2. TOFU (trust-on-first-use) for the etcd pin, defended by two hashes

**Problem:** local-pinning the tarball sha defends against GitHub
release compromise _after_ the pin was taken, but not before. If the
adversary compromised the release simultaneously with or before our
pin, we trust the attacker.

**Decision:** pin the tarball sha AND the SHA256SUMS file's own sha.
An attacker would need to substitute content that hashes correctly
against _both_ pinned values, which is cryptographically infeasible.
This narrows — but does not eliminate — the TOFU window.

**Not done:** cosign / sigstore signatures, PGP verification, CI-bot
counter-signing. Phase 12+ release-attestation work will subsume this
scheme.

### 3. `BENCH_TIER` as an env var, hard-fail on bench argv

**Problem:** silent defaults lead to unlabeled numbers in result files.
A flag you can forget to set is a flag that gets forgotten.

**Decision:** `BENCH_TIER` is a required env var for bench invocations,
enforced by `run.sh`:

- `BENCH_TIER=1` or `BENCH_TIER=2` → signature records it.
- `BENCH_TIER=<anything else>` → hard error.
- Unset + argv looks like a bench (`cargo bench`, `--bench`, `bench`) →
  hard error (exit 2).
- Unset + argv is anything else (diagnostics, `echo hello`) → soft
  warning, signature records `tier=unknown`.

The asymmetry is the point. Smoke tests should work without setup;
real numbers should not.

### 4. Canonical signature format, v1

**Problem:** "join fields with spaces and hash" as specified would
give different hashes on different hosts for the same inputs — field
order varies, value padding varies, missing-field handling varies.

**Decision:** canonical form is

- Fields sorted **lexically by key** (deterministic across shells).
- Values trimmed of leading/trailing whitespace (defends against
  `/proc/cpuinfo` tab padding).
- Values shell-escaped (`\ ` for space, `\\` for backslash) — no
  quotation marks. A line parsed by `read -a` recovers key=value
  pairs.
- Single-space separator, no trailing space, no trailing newline in
  the hashed input.
- `sha=<digest>` appears last in the printed line but is **not** part
  of its own hashed input (circular).

Version tag `BENCH_HW v1: ` is the display header, not in the hash.
Field additions bump to `v2`.

### 5. Signature to stderr, not stdout

**Problem:** prepending a signature line to stdout breaks every
JSON-producing bench tool (criterion `--output-format json`,
hyperfine `--export-json`, fio `--output-format json`).

**Decision:** `run.sh` emits the signature to **stderr** unconditionally
and, if `BENCH_OUT_DIR` is set, also writes it to
`$BENCH_OUT_DIR/signature.txt`. Stdout stays clean; downstream tooling
reads the sidecar.

### 6. Two hardware tiers, named now

**Problem:** the first multi-node bench will want to define "what
counts as comparable hardware" under deadline pressure. Better to
fight about it now than in the Phase 14.5 PR review.

**Decision:** Tier 1 = single-node, documented in HARDWARE.md with
explicit core/ram/NVMe minimums plus Linux-only turbo/TSC/isolcpus/
scheduler requirements and macOS best-effort equivalents. Tier 2 =
multi-node, Linux-only, with clock-sync and bandwidth-measurement
requirements. Tier 3 (cross-AZ, public cloud) is explicitly
out-of-scope.

### 7. Five shell tests, wired as a new `bench-harness` CI job

The tests are:

1. `verify_sha` function (pass/fail with locally-generated fixtures,
   no network).
2. Signature line format + determinism (same host, same shell, within
   a few seconds).
3. `run.sh` separates stdout/stderr cleanly, propagates exit code,
   writes sidecar on `BENCH_OUT_DIR`.
4. VERSIONS ↔ HARDWARE.md platform coverage (each supported platform
   has a pinned sha; no extra shas for unsupported platforms).
5. Canonicalization round-trip (extract `sha=` from emitted line,
   recompute it from the rest, assert match).

CI job is Ubuntu-only; macOS coverage is a local-developer
responsibility documented in `benches/README.md`. macOS CI matrix is
Phase 0.5+ territory.

## Consequences

**Positive:**

- First Phase 2 bench PR cannot ship unsigned numbers.
- Pin bumps produce a visible, reviewable diff (the VERSIONS file) and
  a paired bench-harness CI run on the new pin.
- Hardware differences show up in the signature, not as "why does
  CI disagree with my laptop" debugging.

**Negative:**

- Adds five shell scripts to the repo; shell tests can rot faster
  than Rust tests. Mitigation: the portable helpers live in one file
  (`hwsig-lib.sh`) so regressions have one place to hit.
- Becomes a one-way door at the first Phase 2 bench PR — every
  committed result file embeds a signature, and rolling the scaffold
  back would invalidate them.
- macOS + Linux parity is a manual discipline. The signature's field
  set is a promise we make to Apple-Silicon contributors that some
  fields will be `unknown` by design.

## Alternatives considered

- **etcd as a git submodule** — creates `git clone --recursive`
  friction for every contributor; the pin churn belongs in a sha file,
  not in submodule history.
- **Cargo bench crate from day one** — dead code. Phase 2+ adds it
  when there's something to bench.
- **Rust binary for the signature** — premature; <150 lines of shell
  is more auditable than its Rust equivalent. Porting trigger named
  above.
- **No tiers, one big "recommended hardware" doc** — guarantees the
  first multi-node PR litigates the spec under deadline pressure.
