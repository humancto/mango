# etcd oracle

Every "mango beats etcd by Nx" claim needs an oracle: a specific etcd
release, pinned by content hash, runnable on the bench rig. This
directory holds the pin and the fetcher; the actual bench suites that
run the oracle live in Phase 2+.

## Files

- `VERSIONS` — pinned `ETCD_VERSION` and per-platform sha256 hashes.
  Shell-sourceable. Also includes `ETCD_SHA256SUMS_SHA256` for
  defense-in-depth (see Threat model below).
- `fetch.sh` — downloads and verifies the pinned artifact. Prints the
  resolved path to stdout on success.

## Usage

```bash
# Downloads etcd for the current platform into ./cache/ and verifies
# its hash. Prints the artifact path on success.
benches/oracles/etcd/fetch.sh
```

The `cache/` directory is `.gitignore`d. Phase 2+ bench runners unpack
the artifact; this scaffold only verifies that the downloaded bytes
match the pin.

## Supported platforms

| OS     | Arch  | Extension |
| ------ | ----- | --------- |
| linux  | amd64 | `.tar.gz` |
| linux  | arm64 | `.tar.gz` |
| darwin | amd64 | `.zip`    |
| darwin | arm64 | `.zip`    |

Anything else (Windows, other BSDs, ppc64le, s390x) fails loudly.
`uname -m`'s `x86_64`/`aarch64` names are normalized to etcd's
`amd64`/`arm64` convention by `hwsig-lib.sh`'s `uname_arch_normalize`.

## Bumping the pin

1. Pick a new v3.5.z release from https://github.com/etcd-io/etcd/releases.
2. Download the release's `SHA256SUMS` file and record its own sha256.
3. Extract the per-platform hashes from `SHA256SUMS`.
4. Update `VERSIONS` with the new values.
5. Run `fetch.sh` on each supported platform (at minimum locally and in
   CI) to confirm the pin is live.
6. Commit as a standalone PR; the expert-review gate applies here too
   because a bad pin would mask Phase 12+ comparison drift.

## Threat model

Local pinning of the tarball sha is **TOFU (trust-on-first-use)**
against post-publication compromise of the etcd GitHub release page.
It does NOT protect against an attacker who had compromised the
release _before_ the pin was taken.

To narrow the window, we pin **two independent hashes** in `VERSIONS`:

- `ETCD_SHA256_<os>_<arch>`: the per-platform tarball/zip sha.
- `ETCD_SHA256SUMS_SHA256`: the sha of the `SHA256SUMS` file itself.

An attacker substituting the tarball would fail the first hash.
Substituting both tarball and `SHA256SUMS` would fail the second.
Finding a tarball plus a `SHA256SUMS` that collide on both pinned
hashes simultaneously is cryptographically infeasible.

Real attestation (TPM quote, sigstore/cosign signatures of the
release, CI-bot counter-signing) is out of scope for this scaffold.
It is Phase 12+ territory once mango itself needs a release-attestation
story; the architecture there will subsume this one.

## Non-goals

- No signing key. No PGP. No cosign.
- No mirror or failover. If GitHub releases are down, benches block.
- No Windows / BSD / big-endian support. Documented and failing-loud.
- No "always latest" mode. The pin is the point.
