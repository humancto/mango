# Documentation policy

Mango gates rustdoc output at PR time so broken references and
undocumented public symbols can never land on `main`. The gate is
[`doc` in `.github/workflows/ci.yml`](../.github/workflows/ci.yml);
this doc is the policy.

## The two layers

### Layer 1 — `RUSTDOCFLAGS="-D warnings"` on `cargo doc`

Turns every rustdoc lint from `warn`-by-default into a hard
error. Catches, among others:

- `rustdoc::broken_intra_doc_links` — stale `[Symbol]` refs
  after a rename.
- `rustdoc::invalid_html_tags` — hand-written HTML that
  rustdoc can't parse.
- `rustdoc::bare_urls` — URLs that weren't wrapped in `<...>`
  or a markdown link.
- `rustdoc::redundant_explicit_links` — when the text and the
  target would collapse.

**Why broad `-D warnings` and not an enumerated allowlist?**
New rustdoc lints land every few releases. An enumerated
allowlist rots silently — you would get coverage of existing
lints but miss the new ones. Broad deny plus per-module
`#![allow(...)]` for known-noisy generated code (today:
`crates/mango-proto/src/lib.rs` for prost output) is the right
shape — the `allow` is visible in the source, and any new
rustdoc lint that lands is opt-out rather than opt-in.

### Layer 2 — `#![deny(missing_docs)]` on every `crates/mango-*/src/lib.rs`

Forces every `pub` item on Mango's API surface to carry at
least one line of prose. The root attribute cascades into all
public items in the crate; private items are exempt.

**Scope**: applied to `crates/mango`, `crates/mango-proto`,
`crates/mango-loom-demo`. Explicitly NOT applied to
`crates/xtask-vet-ttl` — it is a `publish = false`
supply-chain helper, not a user-facing API. It carries an
explicit `#![allow(missing_docs)]` so that a future `rustc`
flipping the `missing_docs` default from `allow` to `warn`
cannot retroactively red the `doc` job.

**Cascade exception** — `crates/mango-proto/src/hello/v0`
carries an inner `#![allow(missing_docs, rustdoc::bare_urls,
rustdoc::broken_intra_doc_links)]` because the module body is
prost-generated code we don't own. The outer module boundary
is still gated.

## Local reproducer

Run the same command CI runs:

```
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items --workspace --locked
```

To verify the gate is actually tight (smoke-test that both
layers bite):

```
# Layer 1 — broken intra-doc link:
echo '//! [not_a_real_symbol]' >> crates/mango/src/lib.rs
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items --workspace --locked
# Expect exit != 0, diagnostic: "unresolved link to `not_a_real_symbol`"
git checkout -- crates/mango/src/lib.rs

# Layer 2 — missing doc:
# Add `pub fn foo() {}` to a mango-* lib.rs, without a doc comment above it.
# `cargo build -p <crate>` fails with "missing documentation for a function".
```

## Interaction with other jobs

- Doctest **execution** remains in the `test` job
  (`cargo test --doc --workspace --locked` in
  `.github/workflows/ci.yml`). The `doc` job is link- and
  warning-only — it compiles doc pages but does not run code
  blocks. This split keeps incremental doc rendering fast
  without losing executable-example coverage.
- **Cross-crate intra-doc links** ARE resolved by `cargo doc
--workspace --no-deps`. `--no-deps` skips documentation for
  third-party dep crates, but workspace members are still fully
  traversed — a `[mango::VERSION]` reference from
  `mango-proto` is validated against `mango`'s real surface. A
  rename in `mango` that leaves a stale ref in `mango-proto`
  will red CI. This is the desired behavior.
- **`#![deny(missing_docs)]` fires at compile time.** A
  contributor adding `pub fn foo()` without a `///` comment
  sees a `cargo build` / `cargo check` failure, not a
  `cargo doc`-only failure. This is intentional: catch the
  omission in the editor, not in CI.

## Adding a new mango-\* crate

Any new crate under `crates/mango-*` (i.e., matching the
glob the roadmap names) must:

1. Include `#![deny(missing_docs)]` at the top of its
   `src/lib.rs`.
2. Pass the local reproducer command on first commit.
3. Update `docs/documentation-policy.md` only if it introduces
   a new cascade exception (e.g., new codegen target).

rust-expert PR review will flag omissions.

## Related docs

- [`unsafe-policy.md`](unsafe-policy.md)
- [`sbom-policy.md`](sbom-policy.md)
- [`ct-comparison-policy.md`](ct-comparison-policy.md)
- [`supply-chain-policy.md`](supply-chain-policy.md)
