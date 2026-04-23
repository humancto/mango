# Phase 0 item 0.13 — `mango-proto` skeleton

**Roadmap:** `ROADMAP.md:761`.

**Goal.** Create `crates/mango-proto` with `tonic-build` and a
hello-world `.proto` file that compiles on CI. This is Phase 0
plumbing: it verifies the protobuf → Rust codegen pipeline works
under the workspace's lint / MSRV / clippy regime **before** the
first real `.proto` (the `KV`, `Watch`, `Lease` surfaces in
Phase 6+) tries to land on top of a broken toolchain.

No wire-compatible API yet; no KV / Watch / Lease surfaces. Just:

1. A new workspace member `crates/mango-proto`.
2. A `build.rs` that runs `tonic-build` against a single `.proto`.
3. A tiny `hello.proto` that compiles cleanly.
4. A library target that re-exports the generated code.
5. Tests that exercise the generated types under the workspace
   lint regime.
6. CI runs the build under the existing `ci.yml` jobs, with a
   `protoc` install step added to the jobs that compile.

## Revisions applied (post rust-expert review of plan v1)

Verdict was `REVISE`. Two showstoppers, two bugs, several risks
/ missing items. All applied in v2.

- **S1 — protoc is NOT vendored** by `tonic-build 0.12` /
  `prost-build 0.13`. `ubuntu-24.04` has no `protoc` by default;
  the build fails with "Could not find `protoc`". **Fix in
  this PR**: add `apt-get install -y protobuf-compiler` to the
  CI jobs that actually compile (`clippy`, `test`, `msrv`).
  Not `fmt` (rustfmt only), not `deny` (cargo-deny binary,
  no compile), not `bench-harness` (pure shell). The
  `protobuf-compiler` apt package is on the GitHub runner image
  cache, so warm runs are fast.
- **S2 — workspace lint quarantine is insufficient.** The
  v1 `#![allow(clippy::all, clippy::pedantic, clippy::nursery,
unreachable_pub, missing_docs)]` does **not** suppress
  `arithmetic_side_effects`, `cast_possible_truncation`,
  `cast_sign_loss`, `indexing_slicing`, `disallowed_types`,
  `incompatible_msrv`, `unwrap_used`, `expect_used`, or `panic`,
  because the workspace table sets those at `level = "deny",
priority = 1` and priority-1 deny outranks priority-0 allow.
  **Fix**: enumerate every deny explicitly in the generated
  module's `#![allow(...)]`. Even if prost's emitted code for
  the hello-world does not trip them today, a future prost point
  release or new clippy lint is one change away from breaking CI.
- **B1 — dead reference to `tonic::include_proto!`.** v1 had a
  half-drafted `tonic::include_proto!` snippet that the final
  `include!(concat!(env!("OUT_DIR"), "/..."))` replaces. v2
  drops the draft so the implementing PR does not cargo-cult the
  wrong one.
- **B2 — `default-features = false, features = ["prost"]`
  rationale was wrong.** `tonic-build 0.12`'s `transport` feature
  is `transport = []` — marker, zero deps. Dropping it changes
  nothing in the dep graph. **Correct rationale**: future-
  proofing; if a future `tonic-build` point release adds runtime
  deps to `transport`, we do not pick them up silently. The
  feature spec is kept; the comment is corrected.
- **R1 — `syn` duplicate-version flare.** `tonic-build` pulls
  `syn 2`. If any future dep lands `syn 1`, `cargo-deny`
  `multiple-versions = "deny"` fires. Not this PR's problem; do
  not add a speculative skip. Documented as a known-wait-for-it.
- **R2 — `hello.proto` delete-plan in the file**. Plan's TODO was
  only in the doc-comment. v2 adds a `// DELETE:` comment at the
  top of `hello.proto` itself so it is grep-visible by name.
- **M1 — compile-time trait-bound assertion**. v1 only had a
  prost-roundtrip test. v2 adds a zero-runtime-cost const
  assertion that the generated types implement
  `Clone + Default + Debug + PartialEq + prost::Message`.
  Catches a future prost-build config change that strips a
  derive.
- **M2 — `publish = false` guard.** `mango-proto` is not
  publishable yet. Add `publish = false` to `[package]` so a
  future absent-minded `cargo publish` is a no-op.
- **M3 — `Cargo.toml` description**. v1 ended the description
  with `"..."`. v2 uses a real sentence.
- **Nit — MSRV on `tonic-build` / `prost`**. Both target Rust ≥
  1.70 per tokio-rs policy. Our MSRV 1.80 is above that. No
  change needed; noted for provenance.

## North-star axis moved

**Time-to-first-protobuf**. Every later gRPC PR assumes the
`tonic-build` + `.proto` → Rust pipeline works. Landing the
skeleton now means the first `mango.kv.v1.KV` PR is a diff
against a working crate, not a standup of the whole
build-script / feature-gating / lint-escape story.

## Out of scope

- **Any real wire surface** (`KV`, `Watch`, `Lease`, `Cluster`,
  `Auth`, `Maintenance`). Phase 6+.
- **etcd wire compatibility**. Phase 6+ wire-compat gate.
- **`tonic` runtime wiring** (service impls, servers, clients).
  Phase 6+. This crate is _code-generation only_ for now.
- **Custom codegen plugins**, `prost-build` attribute tuning
  beyond the `tonic-build` defaults, `serde` derive wiring. First
  real PR picks what it needs.
- **`buf` linting / breaking-change detection.** Phase 6+ when
  the surface is wire-compat-load-bearing.
- **Feature-flagging client-only vs. server-only builds.** First
  real gRPC PR picks the split.

## Non-goals

- No `Cargo.lock` explosion — `tonic-build` pulls a lot of
  transitive deps, but all of them are build-time only (not
  runtime) and `cargo-deny` policy already excludes the things
  that matter (`openssl-sys`). Audit post-land.
- No `tonic` / `prost` workspace dep hoisting yet. If a second
  crate needs them, hoist then. Premature hoisting is churn.
- No runtime dep on `tonic` itself. `prost` (for the generated
  message structs) is the only runtime dep; `tonic-build` and
  `prost-build` are build-deps.

## Files

- `Cargo.toml` (workspace) — EDIT. Add `crates/mango-proto` to
  `[workspace] members`.
- `crates/mango-proto/Cargo.toml` — NEW:

  ```toml
  [package]
  name = "mango-proto"
  version.workspace = true
  edition.workspace = true
  rust-version.workspace = true
  license.workspace = true
  repository.workspace = true
  authors.workspace = true
  description = "Mango generated protobuf bindings (Phase 0 skeleton; real wire surfaces land in Phase 6)."
  publish = false

  [dependencies]
  prost = "0.13"

  [build-dependencies]
  # `default-features = false, features = ["prost"]` is future-proofing.
  # `transport` today is `transport = []` (a marker with zero deps);
  # dropping it does not change the current dep graph. If a future
  # tonic-build point release adds runtime deps under `transport`, we
  # do not pick them up silently — we opt in deliberately.
  tonic-build = { version = "0.12", default-features = false, features = ["prost"] }

  [lints]
  workspace = true
  ```

- `crates/mango-proto/build.rs` — NEW:

  ```rust
  fn main() -> Result<(), Box<dyn std::error::Error>> {
      // Types-only — no service trait generation. The hello proto has
      // no `service` block, but even if someone adds one by mistake
      // the build stays types-only (no runtime tonic dep) until Phase 6.
      tonic_build::configure()
          .build_server(false)
          .build_client(false)
          .compile_protos(&["proto/hello.proto"], &["proto"])?;
      Ok(())
  }
  ```

- `crates/mango-proto/proto/hello.proto` — NEW:

  ```proto
  // DELETE: removed when the first real `mango.*.v1` proto lands (Phase 6).
  // This is Phase 0 skeleton only — it exists to exercise the tonic-build
  // pipeline under the workspace lint and MSRV regime. Not wire-stable.
  syntax = "proto3";
  package mango.hello.v0;

  message HelloRequest { string name = 1; }
  message HelloReply   { string message = 1; }
  ```

  Package namespace `mango.hello.v0` — `v0` not `v1` because the
  message set is pre-stable and will be deleted when the first
  real proto lands. Stable wire namespaces begin at `v1`.

- `crates/mango-proto/src/lib.rs` — NEW:

  ```rust
  //! Mango — generated protobuf bindings.
  //!
  //! Phase 0 skeleton (ROADMAP.md:761). Real wire surfaces land in
  //! Phase 6+. The `hello::v0` module ships with a single
  //! request/reply pair solely to exercise the `tonic-build`
  //! pipeline end-to-end under the workspace lint and MSRV regime.
  //! It is **not** wire-stable and will be deleted when the first
  //! real `mango.*.v1` proto lands.

  pub mod hello {
      pub mod v0 {
          // Generated code is foreign — we do not own the style. We
          // explicitly allow every workspace-denied lint that prost's
          // expansion could plausibly trip (now or in a future point
          // release). Each `clippy::all` / `clippy::pedantic` /
          // `clippy::nursery` group allow is priority-0; the workspace
          // table sets the individual denies at priority-1. Priority-1
          // deny wins over priority-0 allow, so the individual denies
          // MUST be enumerated explicitly here — the group allows are
          // kept only for future-proofing against new lints.
          #![allow(
              clippy::all,
              clippy::pedantic,
              clippy::nursery,
              clippy::arithmetic_side_effects,
              clippy::cast_possible_truncation,
              clippy::cast_sign_loss,
              clippy::indexing_slicing,
              clippy::disallowed_types,
              clippy::incompatible_msrv,
              clippy::unwrap_used,
              clippy::expect_used,
              clippy::panic,
              clippy::dbg_macro,
              clippy::print_stdout,
              clippy::print_stderr,
              clippy::await_holding_lock,
              clippy::await_holding_refcell_ref,
              unreachable_pub,
              missing_docs
          )]
          include!(concat!(env!("OUT_DIR"), "/mango.hello.v0.rs"));
      }
  }

  #[cfg(test)]
  mod tests {
      #![allow(
          clippy::unwrap_used,
          clippy::expect_used,
          clippy::panic,
          clippy::indexing_slicing,
          clippy::unnecessary_literal_unwrap,
          clippy::arithmetic_side_effects
      )]

      use super::hello::v0::{HelloReply, HelloRequest};

      // Compile-time assertion that prost-derive emits the expected
      // trait bounds. If a future prost-build config change strips a
      // derive (e.g., omits `Default`), this const fails to typecheck
      // and the build breaks loudly at PR time — cheaper than
      // discovering the regression from a field using the method.
      const _: fn() = || {
          fn assert_bounds<T>()
          where
              T: Clone + Default + std::fmt::Debug + PartialEq + prost::Message,
          {
          }
          assert_bounds::<HelloRequest>();
          assert_bounds::<HelloReply>();
      };

      #[test]
      fn hello_types_roundtrip_via_prost() {
          // Exercise the generated types: construct, serialize,
          // round-trip. Proves (a) codegen ran, (b) the types
          // implement prost::Message, (c) encoding/decoding are
          // wired correctly.
          use prost::Message;

          let req = HelloRequest { name: "mango".to_string() };
          let mut buf = Vec::new();
          req.encode(&mut buf).expect("encode HelloRequest");

          let decoded = HelloRequest::decode(buf.as_slice())
              .expect("decode HelloRequest");
          assert_eq!(decoded.name, "mango");

          let reply = HelloReply { message: "hello, mango".to_string() };
          assert_eq!(reply.message, "hello, mango");
      }
  }
  ```

- `.github/workflows/ci.yml` — EDIT. Add a `protoc` install step
  to `clippy`, `test`, `msrv`. Not `fmt` (rustfmt only), not
  `deny` (cargo-deny binary), not `bench-harness` (shell only).
  The step shape:

  ```yaml
  - name: install protoc
    # tonic-build 0.12 + prost-build 0.13 do not vendor protoc;
    # the protobuf-compiler apt package is on the GitHub runner
    # image cache, so this is warm after the first run.
    run: sudo apt-get update && sudo apt-get install -y protobuf-compiler
  ```

  Placed before `cargo fetch` in each of the three jobs.

- `deny.toml` — CHECK only. Verify new transitive build-deps
  from `tonic-build` / `prost-build` do not trip license or
  advisory rules. Known-safe upstream (tokio-rs). If a flare
  lands, narrow skip — prefer an upstream fix over a skip.
- `.gitignore` — no edit needed (already ignores `target/`).

## Test strategy

Three gates, all in existing CI jobs (after the `protoc` install
step lands):

1. **`cargo build`** via `cargo clippy --workspace --all-targets`
   and `cargo test --workspace --all-targets` in `ci.yml`. Fails
   if `build.rs` cannot find `protoc` — the apt install step is
   the fix (S1).
2. **`cargo test` — `hello_types_roundtrip_via_prost` +
   compile-time `assert_bounds` (M1).** Proves codegen ran, types
   implement `prost::Message` + the expected auto-derives.
3. **`cargo clippy --workspace --all-targets -- -D warnings`.**
   Proves the explicit deny-list in the generated module's
   `#![allow(...)]` (S2) is tight enough that no workspace-denied
   lint escapes into the user crate's compile.

**MSRV job (cargo check @ 1.80).** `tonic-build` 0.12 and
`prost` 0.13 both target Rust ≥ 1.70 per tokio-rs policy. Our
MSRV 1.80 is above that. `cargo check --workspace --all-targets
--locked` on rustc 1.80 runs locally before push.

**`cargo-deny` job.** `tonic-build` pulls `syn 2`, `quote 1`,
`proc-macro2 1`, `prettyplease 0.2`, `prost-build 0.13`,
`prost-types 0.13`. All tokio-rs upstream; licenses Apache-2.0
/ MIT. No `openssl-sys`. Expect clean.

**No bespoke `scripts/test-*.sh`.** The workspace's existing
jobs are sufficient; adding a proto-specific shell script is
over-fitting at skeleton size.

## Risks

- **`protoc` on macOS contributor machines.** `brew install
protobuf` handles it. A `CONTRIBUTING.md` note is a Phase 0.14
  follow-up (next roadmap item), not this PR.
- **Workspace lint regime fighting generated code.** Mitigated
  by the explicit full-enumeration `#![allow(...)]` (S2). If a
  future clippy release adds a new lint in the deny table, the
  quarantine may need one new line — grep-obvious failure mode.
- **MSRV drift.** `tonic-build` 0.12 → 0.13 could raise MSRV.
  Mitigated by caret range `"0.12"` (minor-flex) + CI MSRV job
  catching regressions at PR time.
- **`tonic-build` pulling `openssl-sys`.** With
  `default-features = false, features = ["prost"]`, it does not.
  Verified via `cargo tree -d`. Double-check at PR time.
- **`cargo-deny multiple-versions = "deny"`.** `tonic-build`
  brings `syn 2`; any future dep landing `syn 1` flares. Not
  this PR; do not pre-skip.
- **Namespace collision with future `KV` proto.** `mango.hello.v0`
  is one-shot; when the first real proto lands under
  `mango.kv.v1`, the `hello.v0` module is deleted in the same
  PR. `// DELETE:` comment at the top of `hello.proto` makes
  this grep-visible (R2).
- **Skeleton crate being treated as importable.** No other crate
  should depend on `mango-proto` yet. If a future PR adds
  `mango-proto = { path = "..." }` purely to exercise hello
  types, push back — that is test-fixture coupling across
  crates.

## Plan of work

1. Branch: `feat/mango-proto-skeleton`.
2. Create `crates/mango-proto/` with the four files above
   (`Cargo.toml`, `build.rs`, `src/lib.rs`, `proto/hello.proto`).
3. Edit workspace `Cargo.toml` to add the member.
4. Edit `.github/workflows/ci.yml` to add the `protoc` install
   step to `clippy`, `test`, `msrv`.
5. Run locally:
   - `cargo build --workspace --locked`
   - `cargo test --workspace --all-targets --locked`
   - `cargo clippy --workspace --all-targets --locked -- -D warnings`
   - `rustup run 1.80 cargo check --workspace --all-targets --locked`
   - `cargo tree -d` — eyeball for unexpected dupes
6. Push branch. Open PR with the roadmap reference and a
   one-liner "proto skeleton, hello world round-trips, no wire
   surfaces yet" summary.
7. `rust-expert` adversarial review on the diff.
8. Revise on review; re-request `APPROVE`.
9. Merge `--squash --delete-branch` on `APPROVE`.
10. Flip `ROADMAP.md:761` checkbox on main; commit + push.

## Rollback plan

Revert the merge commit. The crate is additive; no other crate
depends on it yet. The `ci.yml` `protoc` install step is
additive too; reverting costs a no-op apt step on future PRs
until the next proto crate lands, which is negligible.

## Acceptance

- `crates/mango-proto/` exists with `Cargo.toml` (with `publish
= false`), `build.rs`, `src/lib.rs`, `proto/hello.proto` (with
  `// DELETE:` header).
- Workspace `Cargo.toml` lists the new member.
- `.github/workflows/ci.yml` has a `protoc` install step in the
  `clippy`, `test`, and `msrv` jobs.
- `cargo build --workspace --locked` succeeds locally.
- `cargo test --workspace --all-targets --locked` succeeds
  including:
  - the compile-time `assert_bounds` const (M1),
  - the `hello_types_roundtrip_via_prost` test.
- `cargo clippy --workspace --all-targets --locked -- -D
warnings` succeeds with no generated-code lint escapes.
- `cargo check --workspace --all-targets --locked` on rustc
  1.80 succeeds (MSRV gate).
- `cargo-deny` job succeeds on the PR.
- `rust-expert` `APPROVE` on the final diff.
- `ROADMAP.md:761` checkbox flipped on merge.
