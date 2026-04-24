# API stability policy — `#[non_exhaustive]` on public enums

Mango commits to **semver-safe surface evolution** on every published
`crates/mango-*` crate. Today no crate is published; when Phase 6
opens (first stable public API), adding a variant to a non-
`#[non_exhaustive]` `pub enum` becomes a silent major-version break
that downstream exhaustive `match` callers hit at compile time.

This policy closes that failure mode before it can ship.

Cross-refs:

- [`docs/public-api-policy.md`](public-api-policy.md) — `cargo-public-api`
  diffs the full surface (symbol set).
- [`docs/semver-policy.md`](semver-policy.md) — `cargo-semver-checks`
  diagnoses individual lints (including `#[non_exhaustive]` **flips**,
  but not the _missing-on-creation_ case this policy covers).

The three policies are layered: this one prevents the regression at
PR time; `public-api` catches the surface change; `semver-checks`
classifies it.

## The rule

**Every `pub enum` in a publishable crate MUST carry
`#[non_exhaustive]`** unless a documented exception applies. The
exception inventory is this doc's §"Exceptions" section. Per-enum
exceptions live on the enum itself as a `// reason:` line-comment
followed by `#[allow(clippy::exhaustive_enums)]`; crate-wide
exceptions live at lib.rs root.

Error enums are the **canonical case _for_ `#[non_exhaustive]`**,
not an exception. Adding an error variant is the single most
common semver-silent break for downstream users.

## Scope — which crates the policy applies to

A crate is **publishable** iff its `Cargo.toml` does **not** set
`publish = false`. This matches the predicate used by
`scripts/public-api-scripts-test.sh` and the `public-api.yml`
workflow's jq filter — one oracle for three policies, one source of
drift to watch.

Today: `crates/mango` is the only publishable crate. The other four
(`mango-proto`, `mango-loom-demo`, `mango-madsim-demo`,
`xtask-vet-ttl`) set `publish = false` and carry a crate-level
`#![allow(clippy::exhaustive_enums)]` escape at lib.rs top — see the
precedent shape in [`crates/mango-proto/src/lib.rs`](../crates/mango-proto/src/lib.rs).

When Phase 1+ adds a publishable crate, the workspace lint is
inherited automatically. No per-crate deny needed — the default is
safe.

## Enforcement

Two layers, defense-in-depth:

1. **Primary gate — `clippy::exhaustive_enums`**. Enabled at
   `deny` level in `[workspace.lints.clippy]` at
   [`Cargo.toml`](../Cargo.toml). Clippy parses the AST; no regex
   false-positives. Fires at PR time via the existing workspace
   clippy step in `ci.yml` — no new CI wiring needed for the lint
   itself.

2. **Structural backstop — `scripts/non-exhaustive-check.sh`**.
   Runs in
   [`.github/workflows/non-exhaustive.yml`](../.github/workflows/non-exhaustive.yml).
   Asserts (a) the workspace lint entry is present in `Cargo.toml`,
   (b) each `pub enum` in each publishable crate either has
   `#[non_exhaustive]` or an `#[allow(clippy::exhaustive_enums)]`
   preceded by a `// reason:` comment. Catches the failure mode
   where someone removes the workspace lint entry — clippy would
   go silent; this backstop would not.

The workflow also runs a fixture-based clippy regression test
against
[`tests/fixtures/non-exhaustive/`](../tests/fixtures/non-exhaustive/),
locking in clippy's behavior on this lint against restriction-
category drift across clippy point releases.

## Exceptions

Four narrow cases:

### 1. Exhaustive-by-contract enums

Variant sets that are load-bearing contracts with downstream
exhaustive pattern-matching and where adding a variant is _always_
a major semver break by design — e.g. `Direction { Ingress,
Egress }`, `Parity { Even, Odd }`.

Be strict about what qualifies. Raft `Role { Leader, Follower,
Candidate }` is _not_ a good example — real Raft implementations
legitimately grow roles (`PreCandidate`, `Learner`, etc.) and the
right shape for such an enum is `#[non_exhaustive]`. Reserve the
exhaustive-by-contract escape for enums where the mathematical or
protocol-level closure is load-bearing.

```rust
// reason: exhaustive-by-contract — parity mod 2 is mathematically closed; a third variant would be ill-formed, not merely a break.
#[allow(clippy::exhaustive_enums)]
pub enum Parity {
    Even,
    Odd,
}
```

The `// reason:` comment is on a **single line**. The backstop
(`scripts/non-exhaustive-check.sh`) requires the line _immediately_
before `#[allow(clippy::exhaustive_enums)]` to be a `// reason:`
line-comment; a wrapped two-line reason fails the backstop because
the continuation line is not itself a `reason:` marker.

### 2. C-repr / FFI enums

Enums with a fixed `#[repr(u8/u16/i32/...)]` that round-trip across
a wire protocol, kernel ioctl, or FFI boundary. Adding a variant
would silently reinterpret existing data.

```rust
// reason: C-repr wire type — variants match kernel TCP_INFO states; adding one would misread a live socket.
#[allow(clippy::exhaustive_enums)]
#[repr(u8)]
pub enum TcpState {
    Established = 1,
    SynSent = 2,
    // ...
}
```

### 3. Enums from generated code

`prost` / `tonic` / `proto3` codegen does not emit
`#[non_exhaustive]`, and retrofitting post-codegen is brittle. The
escape is a **crate-level** `#![allow(clippy::exhaustive_enums)]` at
the generated crate's lib.rs root.

Today: `mango-proto` sets `publish = false` and already carries the
crate-level allow. When `mango-proto` becomes publishable (Phase 6+),
the allow stays — the "generated code" exception is the
justification.

### 4. Non-publishable crates

Crates that set `publish = false` are **out of scope** for this
policy. They carry a crate-level `#![allow(clippy::exhaustive_enums)]`
at lib.rs top with a one-line comment citing this section. Removing
`publish = false` MUST be paired with **replacing** the crate-level
allow with either `#[non_exhaustive]` on every public enum or per-enum
`// reason:` escapes — dropping the crate-level allow alone would
red-light clippy on every enum in the crate at once.

## How to add a per-enum exception at MSRV 1.80

`#[allow(foo, reason = "...")]` (the inline `reason` field) is
stable from rustc 1.81+. Mango MSRV is 1.80 and `clippy::incompatible_msrv`
is denied workspace-wide, so the inline `reason = "..."` form would
fail CI.

**Convention**: put a `// reason: ...` line-comment _immediately
preceding_ the `#[allow]` attribute.

```rust
// reason: <one-sentence justification citing which §1–§4 exception applies>
#[allow(clippy::exhaustive_enums)]
pub enum Foo {
    A,
    B,
}
```

- Line-comment only (not `///` doc-comment — those leak into
  rustdoc output).
- The backstop script asserts `^\s*// reason:` appears on the line
  immediately before each `#[allow(clippy::exhaustive_enums)]`.
- When mango's MSRV bumps to 1.81+, `docs/msrv.md`'s MSRV-bump
  checklist covers migrating `// reason:` comments to inline
  `reason = "..."` attribute fields.

## Struct-like enum variants

`#[non_exhaustive]` on the outer enum only covers the **variant set**.
If a variant has named or tuple fields expected to grow, the variant
itself also needs `#[non_exhaustive]`:

```rust
#[non_exhaustive]
pub enum Event {
    #[non_exhaustive]
    Message { body: String, timestamp: u64 },
    Disconnect,
}
```

Without the inner attribute, adding `priority: u8` to `Message` is a
semver break even though the outer enum permits adding a new variant.
Clippy does not catch this; reviewers do on the PR.

## Interaction with other workspace lints

### `unreachable_pub = "warn"`

If clippy fires both `unreachable_pub` and `exhaustive_enums` on the
same item, the **primary fix is reducing visibility**
(`pub(crate) enum ...`), not adding `#[non_exhaustive]`. Crate-
private enums are not subject to this policy — `#[non_exhaustive]`
only earns its keep when the type crosses the crate boundary.

### `disallowed_types` / other workspace lints

This policy is one of several orthogonal lint-table gates. See
[`Cargo.toml`](../Cargo.toml) `[workspace.lints.clippy]` for the full
set — `unwrap_used`, `expect_used`, `disallowed_types`,
`arithmetic_side_effects`, etc.

## Removing `#[non_exhaustive]` is a major-version break

Once `mango-*` is published, dropping `#[non_exhaustive]` from an
enum is a major-version break: downstream code that had fallback
arms in `match` can now be flagged as unreachable, and the API
contract has shifted from "may grow" to "is frozen."

`cargo-public-api` surfaces the change; `cargo-semver-checks`
classifies it. Document the break in the release notes.

## Reviewer checklist for a PR adding a `pub enum`

- [ ] Enum carries `#[non_exhaustive]`, OR
- [ ] Enum carries `// reason:` + `#[allow(clippy::exhaustive_enums)]`
      and the reason cites a documented exception (§1–§4).
- [ ] If the enum has variants with struct/tuple fields that may
      grow, each such variant also carries `#[non_exhaustive]`.
- [ ] `cargo clippy --workspace --all-targets` passes locally.
- [ ] The `non-exhaustive` CI job is green on the PR.

## See also

- [`ROADMAP.md:804`](../ROADMAP.md) — item this policy lands
- [`Cargo.toml`](../Cargo.toml) `[workspace.lints.clippy]` —
  `exhaustive_enums = "deny"`
- [`scripts/non-exhaustive-check.sh`](../scripts/non-exhaustive-check.sh)
  — structural backstop
- [`scripts/non-exhaustive-scripts-test.sh`](../scripts/non-exhaustive-scripts-test.sh)
  — self-test
- [`tests/fixtures/non-exhaustive/`](../tests/fixtures/non-exhaustive/)
  — clippy regression fixture
- [`.github/workflows/non-exhaustive.yml`](../.github/workflows/non-exhaustive.yml)
  — CI workflow
- [`docs/msrv.md`](msrv.md) — MSRV-bump checklist (migrates
  `// reason:` comments when MSRV reaches 1.81)
