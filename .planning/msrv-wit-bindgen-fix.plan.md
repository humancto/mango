# MSRV fix — target-scoped `cargo fetch` in the msrv job (plan v2)

**Issue**: [#23](https://github.com/humancto/mango/issues/23)
**Classification**: Plumbing (Reviewer's Contract item #1). No
north-star axis moves. This PR unbreaks the `msrv` CI job that is
red on every push.

## Problem (reproduced)

`cargo fetch --locked` in the `msrv (cargo check @ 1.80)` CI job
fails because `Cargo.lock` references `wit-bindgen 0.57.1`, whose
`Cargo.toml` declares `edition = "2024"`. Parsing that manifest
requires Rust 1.85+; our MSRV toolchain is 1.80.

**Local reproduction** on the current main (`41a63f8` + follow-up
checkbox flips):

```console
$ rustup run 1.80 cargo fetch --locked
...
  Downloaded wit-bindgen v0.57.1
error: failed to parse manifest at `/Users/archithrapaka/.cargo/registry/src/index.crates.io-6f17d22bba15001f/wit-bindgen-0.57.1/Cargo.toml`

Caused by:
  feature `edition2024` is required
  (cargo 1.80.1 does not have edition2024 stabilized)
EXIT=101
```

**The chain** (from `Cargo.lock`):

```
tonic-build 0.12.3 (build-dep of mango-proto)
  └── tempfile 3.27.0
        └── getrandom 0.3.4
              └── wasip2 1.0.3    (cfg(target_os = "wasi") only)
                    └── wit-bindgen 0.57.1   <-- requires edition2024
```

`wit-bindgen 0.57.1` is **never actually compiled** on any target
mango CI uses — `wasip2` is `#[cfg(target_os = "wasi")]` gated.
But `cargo fetch --locked` without a `--target` filter parses the
manifest of every entry in `Cargo.lock` regardless of platform.
That parsing blows up on `edition2024`.

## Options surveyed (updated after rust-expert v1 REVISE)

1. ~~`[patch.crates-io]` to pin `wit-bindgen` to an older version.~~
   **Rejected**: cargo's `[patch]` table does not bypass the
   resolver's semver constraints. `wasip2 1.0.3` requires
   `wit-bindgen ^0.57.1`; any patch to `=0.49.0` (the last
   edition2021 version, not 0.41.0 as plan v1 incorrectly claimed)
   or earlier falls outside `^0.57.1` and is silently dropped.
   Additionally, `0.49.0` itself requires `rust_version = "1.82.0"`,
   so no patch target exists that both satisfies the semver
   requirement AND parses on 1.80.
2. ~~Pin `tempfile` to `~3.13` or `getrandom` to `^0.2`.~~
   **Rejected**: wide cascade across unrelated deps; opts out of
   upstream fixes; indirect and fragile.
3. **`cargo fetch --target x86_64-unknown-linux-gnu` in the MSRV
   CI job** — the fix. `--target` restricts manifest parsing to
   the resolver subgraph that applies to that target, skipping
   wasi-only transitives entirely. **Validated locally**: `rustup
   run 1.80 cargo fetch --locked --target x86_64-unknown-linux-gnu`
   exits 0; bare `cargo fetch --locked` exits 101 with the
   edition2024 error above.
4. ~~Bump MSRV to 1.85.~~ **Deferred, not rejected**: the roadmap
   (item 0.9) says "start at 1.80, bump deliberately." A
   wasi-only transitive isn't a deliberate reason. **But**: if a
   future dep forces us past 1.80 through a non-wasi path, bumping
   to 1.82 is the honest next step. Documented in `docs/msrv.md`
   as the fallback.

**Choice: Option 3.** Single-line CI change, zero dep graph change,
works immediately, and the fix is self-documenting at the call
site.

## Files

Modified:

- `.github/workflows/ci.yml` — the `msrv` job's `cargo fetch
  --locked` step gets `--target x86_64-unknown-linux-gnu`. Same
  change applied to `cargo check` for consistency. A comment
  above the step links Issue #23 so future maintainers don't
  remove the `--target` flag without understanding the
  consequence.

New:

- `docs/msrv.md` — documents the MSRV policy: current floor
  (1.80), the `--target` workaround and why it's there, what
  triggers a deliberate bump, and the bump procedure. Roadmap
  item 0.9 said the MSRV *gate* ships; this is the *policy*.

Modified (one-line):

- `CONTRIBUTING.md` §7 MSRV row — gains a "See `docs/msrv.md` for
  the full policy" link.
- `scripts/verify-contributing-refs.sh` — nothing needed; Check 1
  will naturally flag `docs/msrv.md` once the CONTRIBUTING link
  lands (reciprocity). Check 2 validates the new relative link.

## Verification strategy

Per CONTRIBUTING.md §4 plumbing-PR case:

1. **Reproduction (captured above)**: `rustup run 1.80 cargo fetch
   --locked` on current main returns EXIT=101 with the
   edition2024 error.
2. **Post-fix, local**:
   ```bash
   rustup run 1.80 cargo fetch --locked --target x86_64-unknown-linux-gnu
   rustup run 1.80 cargo check --workspace --all-targets --locked --target x86_64-unknown-linux-gnu
   ```
   Both exit 0. Documented in the commit message.
3. **Post-fix, CI**: merge-gate is the `msrv (cargo check @ 1.80)`
   job going green on the PR.
4. **Nothing else broke**:
   - `bash scripts/verify-contributing-refs.sh` — green (new
     `docs/msrv.md` link resolves via Check 2).
   - `cargo fmt --all -- --check` — green.
   - `cargo clippy --workspace --all-targets --locked -- -D warnings`
     — green.
   - `cargo test --workspace --all-targets --locked` — green.
   - `cargo deny check`, `cargo audit` — green (no dep graph
     change, so these can't regress from this PR).

## Why not add a scripted regression test?

rust-expert v1 suggested: "a 10-line script — `cargo metadata
--format-version=1 | jq` that greps for `edition: 2024` in
`resolve.nodes`." Considered and rejected: `cargo metadata` on
1.80 fails on the same edition2024 manifest parse, so the script
can't run on the MSRV toolchain. It would have to run on stable,
which means it's testing "does the lockfile contain any
edition2024 crate at all" — a broader assertion than what we
actually need (we need "the target-scoped subgraph for our
supported targets has no edition2024 crate under MSRV").

The existing CI `msrv` job with `--target` is the right
regression signal: if a future dep bump pulls an edition2024 crate
into the x86_64-linux subgraph, the MSRV job goes red. That's
precisely when we want to notice.

If stronger enforcement becomes necessary later (e.g., multi-target
MSRV matrix), a `scripts/test-msrv-dep-graph.sh` can be added then.

## Test plan

This is docs/plumbing (CI config + a new policy doc). Per
CONTRIBUTING §4: "trust CI" is not a test plan. The reproducible
verification commands are the test.

Per the user's standing rule — "HOPE YOU'RE ALL ADDING TESTS" —
the regression test here is the pre-existing `msrv` CI job, which
this PR repairs and which exists specifically to catch this bug
class going forward. No new test file is added because the CI
job IS the test; adding an identical script locally would be
redundant with the job we're fixing.

## Risks

1. **Does `--target` change what `cargo check` actually
   typechecks?** Yes — it restricts type-checking to the
   target-specific subgraph. For our workspace this is harmless
   because we don't support wasi targets; the check we want is
   "does the Linux build typecheck on 1.80." But it does mean
   the MSRV job no longer guarantees "the whole lockfile
   typechecks." That guarantee was already broken by Issue #23;
   the PR makes the narrower guarantee explicit.
2. **If we ever add wasi support**, the `--target` flag will need
   updating (a matrix, or dropped entirely). Comment in ci.yml
   notes this.
3. **`cargo deny` and `cargo audit` scan the full lockfile**,
   including wasi-only entries. They don't need `--target`; they
   operate on the manifest metadata without parsing source.
   Confirmed in plan review — no change needed there.
4. **Future dep bump** pulls an edition2024 crate through a
   non-wasi path. The MSRV job goes red; someone bumps MSRV to
   1.82 or 1.85 per the deliberate-bump process in
   `docs/msrv.md`. Acceptable — this is the correct failure mode.

## Rollback

The plain rollback is to revert this PR; that restores the red
MSRV job (known pre-existing state documented in Issue #23).

The **proper rollback** (if `--target` turns out to be wrong) is
to bump MSRV to 1.82, which is the ecosystem floor that the
current dep graph already aligns with (`wasip2` requires 1.87 to
compile but only on wasi targets; `getrandom 0.3` requires 1.63
to compile; `wit-bindgen 0.49` requires 1.82 to compile). 1.82 was
released 2024-10-17. A deliberate bump to 1.82 is a valid fallback
per roadmap item 0.9.

## Refs

- [Issue #23](https://github.com/humancto/mango/issues/23)
- `ROADMAP.md:757` (item 0.9 — cargo-msrv job, already shipped)
- rust-expert parting nit from PR #22 (surfaced the issue)
- rust-expert final review on PR #24 (confirmed the issue is
  pre-existing and not introduced by 0.15)
- rust-expert plan v1 REVISE (rejected the `[patch.crates-io]`
  approach with the semver-incompatibility proof)

---

## Revisions applied from rust-expert v1 REVISE

- **Showstopper 1**: `[patch.crates-io]` is semver-incompatible
  with the `^0.57.1` constraint from `wasip2`. Mechanism
  abandoned; replaced with `--target x86_64-unknown-linux-gnu`
  in the msrv CI job (Option 3 — validated locally: EXIT=0 with
  `--target`, EXIT=101 without).
- **Showstopper 2**: wrong pinned version (claimed 0.41.0, correct
  last-edition2021 is 0.49.0; 0.49.0 itself needs 1.82). Moot
  since the patch mechanism is abandoned, but noted in the
  "Options surveyed" section so the factual error is recorded.
- **Bug 3**: MSRV 1.82 is the ecosystem floor. Incorporated into
  `docs/msrv.md` and the Rollback section as the deliberate-bump
  fallback.
- **Risk 4**: `wasip2` / `getrandom` / `tempfile` cascade
  correctly rejected in plan v1; preserved.
- **Missing 5**: reproduction commands now in the plan (verbatim
  output with EXIT codes).
- **Missing 6**: regression test question answered in its own
  section; conclusion is "the `msrv` CI job we're fixing IS the
  regression test," with justification for not adding a local
  script (can't run on 1.80 itself).
- **Nit 7**: `cargo deny check` unaffected (operates on manifest
  metadata, not parsed manifests); noted in Risks.
- **Nit 8**: `docs/msrv.md` is the right artifact (confirmed by
  rust-expert); added explicit CONTRIBUTING §7 link.
- **Nit 9**: Rollback section rewritten — "revert is not a
  rollback, it re-breaks" is now explicit, and the proper
  fallback (bump to 1.82) is named.
