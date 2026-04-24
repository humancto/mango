# Constant-time comparison policy

Mango treats equality on secret byte sequences as a timing side
channel, not a neutral operation. The machinery that enforces this is
a scoped text gate — `scripts/ct-comparison-check.sh` — that runs on
every PR and flags the specific source patterns that produce timing
oracles in authentication, crypto, token, and hash-chain code.

This doc is the policy. The CI enforcement is
[`.github/workflows/ct-comparison.yml`](../.github/workflows/ct-comparison.yml);
the check script is
[`scripts/ct-comparison-check.sh`](../scripts/ct-comparison-check.sh);
the ignore list is
[`.ct-comparison-ignore`](../.ct-comparison-ignore).

## Why this exists

Comparing two byte sequences with `==` on most hardware short-circuits
at the first differing byte. A remote attacker who can time the
comparison learns one byte of a secret per failed probe — this is the
classic HMAC-equals timing oracle (lucky-thirteen, HMAC tag
verification CVEs, and the whole family they belong to). The fix is
constant-time comparison: always walk the full length, always XOR and
OR-accumulate.

[`subtle::ConstantTimeEq`](https://docs.rs/subtle) is the canonical
Rust crate for this. Its `ct_eq` returns a `subtle::Choice`, a
wrapper around `u8` whose `From<Choice> for bool` conversion is
always the final, branching step — by design, you have to explicitly
decide when to leak.

## Scope

The gate scans files whose path matches any of:

- `crates/*/src/**/auth*.rs`
- `crates/*/src/**/crypto*.rs`
- `crates/*/src/**/token*.rs`
- `crates/*/src/**/hash_chain*.rs`
- the directory variant: files under `crates/*/src/**/{auth,crypto,token,hash_chain}/**/*.rs`

Exclusions (unconditional, applied before scope matching):

- Any path containing `/tests/`.
- Any file ending in `_test.rs` or `_tests.rs`.

Rationale: test code routinely `assert_eq!` on byte slices. The gate
is for production paths; test fixtures and `#[test]` modules in
separate files stay out of scope by convention. Unit tests in the
same file (`#[cfg(test)] mod tests`) are in scope and need the
`// ct-allow: test` annotation — see Escape hatch below.

### Naming collisions

The `**/token*.rs` glob also matches `token_bucket.rs`,
`token_stream.rs`, etc. — files that have nothing to do with auth
tokens. The mitigation is [`.ct-comparison-ignore`](../.ct-comparison-ignore)
at the repo root: one path per line, `#` comments allowed. Every
entry must name a file that exists on disk; stale entries fail the
gate with exit code 3. Entries carry a one-line justification as a
trailing `#`-comment.

## What is banned

In a scoped file, the gate flags these five source-text patterns.
All five require a `// ct-allow: <reason>` trailing-comment annotation
to pass (see Escape hatch).

- **P1 — byte-literal compare:** `== b"…"` or `!= b"…"` on either
  operand. No ambiguity; the left or right side is literally a byte
  string.

- **P2 — secret-named receiver `.eq(` / `.ne(`:** `<ident>.eq(…)` or
  `<ident>.ne(…)` where `<ident>` ends in `_hash`, `_hmac`, `_mac`,
  `_tag`, `_token`, `_digest`, `_signature`, `_nonce`, `_secret`, or
  `_key`. Also matches `.as_bytes().eq(` / `.as_bytes().ne(` and
  `.eq(b"…")` / `.eq(&b"…")` forms regardless of receiver name.
  _Not_ flagged: `role.eq(&Role::Admin)`, `path.eq(expected_path)`,
  `opt.eq(&other)` — these are non-secret `.eq(` calls and must not
  turn the gate into noise.

- **P3 — `.as_bytes()` compare:** `.as_bytes() ==`, `== <expr>.as_bytes()`,
  and the `!=` variants. Any `.as_bytes()` chain on either side of an
  equality operator is flagged.

- **P4 — secret-named type derives `PartialEq` or `Eq`:** a
  `#[derive(...)]` attribute containing `PartialEq` or `Eq` followed
  by a `struct` or `enum` declaration whose name matches
  `Token | Hmac | Mac | Tag | Secret | Password | Key | Credential | Nonce | Digest | Signature`
  (case-insensitive, whole-word). The detection spans multi-line
  derives (`#[derive(\n    Debug,\n    PartialEq,\n)]` is flagged).
  Types that must never derive `PartialEq`: anything that holds a
  secret. The correct trait is `subtle::ConstantTimeEq`. Flagged only
  in scoped files.

- **P5 — secret-named identifier in `==` / `!=`:** `==` or `!=` where
  either operand references an identifier whose name ends in one of
  the P5 suffix words (same list as P2). Example: `session.token == received`,
  `computed_hmac != stored`, `tag == expected_tag`. Both sides of the
  operator are scanned.

Patterns P1-P5 are deliberately narrow. Integer compares, enum
matches, `Duration::ZERO` comparisons, `Ordering` compares — none
match any pattern. The gate is not a blanket ban on `==`; it is a
ban on five specific source-text shapes that reliably appear in
secret-compare bugs.

## What is required

Use
[`subtle::ConstantTimeEq::ct_eq`](https://docs.rs/subtle/latest/subtle/trait.ConstantTimeEq.html):

```rust
use subtle::ConstantTimeEq;

// Compare two MAC tags. Both sides must be equal length.
let choice = computed_hmac.ct_eq(&expected_hmac);
if bool::from(choice) {
    // equal — process the authenticated message
} else {
    // not equal — reject
}
```

The final `bool::from(choice)` is the leak point. Everything before
it is constant-time; the `if` diverges, which is the moment the
secret becomes observable in timing. Put `bool::from` as close to the
divergence as possible. Alternative equivalent form:

```rust
if choice.unwrap_u8() != 0 { … }
```

**Length caveat:** `ct_eq` requires equal-length inputs. If the
inputs differ in length, `ct_eq` returns false immediately — which
itself is a length oracle. If the length could be secret, pad to a
fixed buffer before calling `ct_eq`, or use a length-aware wrapper.
Most protocols treat tag length as public (HMAC-SHA256 tags are
always 32 bytes); in those cases, the caller is responsible for
enforcing the length contract.

For fixed-size arrays (`[u8; 32]` for SHA-256 tags), `subtle` 2.6's
`const-generics` feature gives you `ct_eq` on `[T; N]` directly.
Mango's workspace declaration (`Cargo.toml`,
`[workspace.dependencies]`) enables this feature.

## Escape hatch

A trailing-comment annotation on the offending line:

```rust
let equal_len = received.len() == expected.len(); // ct-allow: length comparison, not secret
```

The reason is free-form but must be specific. Reviewers look at every
`// ct-allow:` and ask "is this genuinely out-of-scope for timing, or
is the author punting?"

**Label gate.** Every PR that adds a new `// ct-allow:` line needs
the `ct-allow-approved` GitHub label. The gate detects new
annotations via a set-difference between the base-ref tree and the
HEAD tree, keyed on `(file-path, normalized-reason-text)` — so
rustfmt reflows, line-number drift, and whitespace normalization
don't trigger a false label-required prompt. Only genuinely new
annotations count.

Missing label on a PR that adds a new annotation: **exit code 2**.
This mirrors the `unsafe-growth-approved` label flow documented in
[`docs/unsafe-policy.md`](unsafe-policy.md).

## Failure modes

| exit | meaning       | trigger                                                                                                |
| ---- | ------------- | ------------------------------------------------------------------------------------------------------ |
| 0    | PASS          | no violations, or every violation is annotated, or new annotations carry the `ct-allow-approved` label |
| 1    | violations    | unannotated P1-P5 match in a scoped file                                                               |
| 2    | missing label | PR adds new `// ct-allow:` annotations without the `ct-allow-approved` label                           |
| 3    | stale ignore  | `.ct-comparison-ignore` names a file that does not exist on disk                                       |

`--list-scope` (local-debug flag) separately returns **0** if the
scope set is non-empty, **2** if empty — CI does not care, this is
only for contributors sanity-checking the glob.

## Sanity-break recipe

Verifies the gate would actually catch a regression. Mirrors the
recipe in [`docs/unsafe-policy.md`](unsafe-policy.md).

1. Create a temporary file at `crates/mango/src/auth_demo.rs` with:
   ```rust
   pub fn check(received: &[u8], expected: &[u8]) -> bool {
       received == expected
   }
   ```
2. Run `./scripts/ct-comparison-check.sh`. Expected: **exit 1**,
   violation reported at `auth_demo.rs:2`.
3. Rename the file to `crates/mango/src/unrelated.rs`. Re-run.
   Expected: **exit 0** (file no longer matches the scope glob).
4. Delete `crates/mango/src/unrelated.rs` before committing.

If step 2 returns exit 0, the gate is broken or the scope glob is
not matching the file — investigate before shipping.

## Complementary layer — `clippy::disallowed_methods`

`clippy.toml` at the repo root declares byte-slice `PartialEq::eq`
as disallowed workspace-wide, as an **advisory** signal:

```toml
disallowed-methods = [
  { path = "<[u8] as core::cmp::PartialEq>::eq", reason = "…" },
  { path = "<alloc::vec::Vec<u8> as core::cmp::PartialEq>::eq", reason = "…" },
  { path = "<bytes::Bytes as core::cmp::PartialEq>::eq", reason = "…" },
]
```

Advisory because the scoped grep, not clippy, is the CI gate. Clippy
fires warnings on `==` between byte slices _anywhere_ in the
workspace — this catches the cross-file indirection the scoped grep
misses (e.g., a helper `fn equal(a: &[u8], b: &[u8]) -> bool { a == b }`
in an unscoped file, called from a scoped file). `bytes::Bytes`
specifically is not catchable by P1-P5 alone because a `Bytes == Bytes`
comparison has no byte-literal, no `.as_bytes()`, no P5 suffix
identifier, and no derive. Clippy's `disallowed_methods` is the only
layer that sees this shape.

Clippy's path-resolution for trait-method paths has historically been
flaky. If the paths above silently fail to resolve (no warnings fire
where they should), the layer is documented as "attempted, not
load-bearing" and the scoped grep remains the sole enforcement.

## Known limitations

- **No type resolution.** The gate is source-text only; it cannot see
  that `let x: &[u8] = …; x == y` is a byte compare unless `x` has a
  secret-suffix name. `subtle` adoption by hand and the
  `disallowed_methods` layer are the backstops.

- **Multi-line operators.** `foo\n    == bar` (one operand on a
  separate line) is not flagged. Rustfmt's `binop_separator` default
  keeps comparisons on a single line; banked on.

- **Macro expansion.** `assert_eq!(token, expected)` expands to `==`
  but the gate never sees the expansion. In test paths this is
  exempted by the `/tests/` / `_test.rs` path rule; in scoped
  non-test files, `assert_eq!` / `assert_ne!` / `debug_assert_eq!` /
  `debug_assert_ne!` are a manual-review backstop. The policy is:
  don't assert on secrets in production code.

- **Cross-file indirection.** A byte compare hidden behind a helper
  function call in an unscoped file is invisible to the scoped grep.
  `clippy::disallowed_methods` is the workspace-wide signal; manual
  review is the backstop.

- **`bytes::Bytes == Bytes`.** Not caught by P1-P5. Caught by
  `clippy::disallowed_methods` if clippy resolves the path.

- **`token*` naming collision.** `**/token*.rs` matches
  `token_bucket.rs` and friends. Mitigated by
  [`.ct-comparison-ignore`](../.ct-comparison-ignore).

- **MSRV.** `subtle` 2.6 supports Rust ≥1.41; workspace MSRV is 1.89
  — no interaction.

## When to upgrade to a dylint

The roadmap item (ROADMAP.md:797) names `dylint` as the target
mechanism; this grep gate is the interim enforcement the roadmap
itself sanctions ("Until the dylint lands, the enforcement is a CI
grep + manual review"). Upgrading makes sense when:

- Real `auth/` / `crypto/` / `token/` / `hash_chain/` code has landed
  in the workspace, giving the lint a real calibration target.
- The grep gate's false-positive OR false-negative rate warrants
  the heavier infrastructure (pinned nightly driver, HIR-level type
  resolution).

At dylint time, each `// ct-allow:` site translates to an
`#[allow(mango::ct_comparison)]` attribute on the enclosing
expression, statement, or item. Expression-level `#[allow]` requires
Rust ≥1.81 (`stmt_expr_attributes` stabilization) and may inform
dylint timing.

## See also

- [`docs/unsafe-policy.md`](unsafe-policy.md) — sibling gate, same
  structure: monotonic growth + label-gated escape hatch.
- [`docs/miri.md`](miri.md) — runtime UB gate on the curated subset.
- [`docs/arithmetic-policy.md`](arithmetic-policy.md) — the fourth
  security-relevant policy doc.
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) §7 (Other policies) and
  §8 (Running the checks locally).
