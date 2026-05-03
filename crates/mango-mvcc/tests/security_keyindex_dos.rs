#![cfg(feature = "test-seed")]
// Belt-and-suspenders compile-gating. The Cargo.toml `[[test]]`
// block also declares `required-features = ["test-seed"]`, so the
// target is omitted from default builds. The file-level cfg here
// ensures a future maintainer who copy-pastes this file into a
// crate without the `[[test]]` block doesn't silently compile it
// (and trip on the test-seed-gated symbols). Same belt-and-
// suspenders posture as `tests/loom_sharded_index.rs:#![cfg(loom)]`,
// adapted for a Cargo-feature gate instead of a `--cfg` flag.
//
// Match the unit-test allow block at the top of `mod tests` in
// `src/sharded_key_index.rs` and the L841 loom integration test
// allow block. Workspace lints (unwrap_used, expect_used, panic,
// indexing_slicing, arithmetic_side_effects) are deny-by-default;
// integration tests get a local allow because they assert against
// panics and use unwrap to keep the test bodies tight.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
//! L842 — hostile-key `DoS` test for the sharded `KeyIndex`.
//!
//! ROADMAP:842 (verbatim):
//!
//! > Hostile-key DoS test in
//! > `crates/mango-mvcc/tests/security/keyindex_dos.rs`: with the
//! > `ahash` seed fixed to a known value (test-only API), confirm
//! > that an attacker who _knows_ the seed can construct N keys
//! > colliding into one shard and bring per-shard read latency to
//! > its single-`RwLock` ceiling. Then re-seed with a fresh
//! > CSPRNG-derived value and confirm the same key set distributes
//! > across shards within statistical bounds (no shard holds > 2x
//! > the mean key count). Validates that production seeding defeats
//! > the attack.
//!
//! Path note: ROADMAP says `tests/security/keyindex_dos.rs`; this
//! file is `tests/security_keyindex_dos.rs` (flat, matching the
//! `tests/loom_sharded_index.rs` convention). A subdirectory
//! umbrella would add linker work for one file. When a second
//! security test lands, consolidate to `tests/security/main.rs`
//! then.
//!
//! Local invocation:
//!
//! ```bash
//! cargo nextest run -p mango-mvcc \
//!     --features mango-mvcc/test-seed \
//!     --test security_keyindex_dos
//! ```
//!
//! Two complementary properties:
//!
//! 1. **Attack reproducibility** — under a known seed, an attacker
//!    who can pick keys can pile arbitrarily many of them into a
//!    single shard. Tests 0, 1, 4 cover this.
//! 2. **Production defense** — under fresh-per-process CSPRNG
//!    seeding (`ShardedKeyIndex::new()`), the *same* attacker key
//!    set redistributes within `max <= 2 * mean`. Tests 2, 3, 5, 6
//!    cover this.
//!
//! Together these prove the production `RandomState::new()`
//! seeding posture is the load-bearing defense, not the data
//! structure shape.

use mango_mvcc::sharded_key_index::{LOOM_SEED, SHARD_COUNT_FOR_TEST};
use mango_mvcc::{Revision, ShardedKeyIndex};

/// Number of attacker-chosen keys used by every distribution-
/// sensitive test in this file. Common N kills the "why is this
/// number different here" cognitive overhead. With
/// `SHARD_COUNT_FOR_TEST = 64`, mean per shard = 156, threshold for
/// `max <= 2 * mean` = 312. See the Chernoff comment block at
/// `production_seeding_redistributes_attacker_keys_within_2x_mean`
/// for the bound.
const N: usize = 10_000;

/// Search bound for the colliding-key derivation. With
/// `SHARD_COUNT_FOR_TEST = 64` and uniform routing, the expected
/// search factor to find N collisions on one shard is
/// approximately `64 * N`. For `N = 10_000` that is `~640_000`
/// candidate keys; `10_000_000` is generous headroom. If a future
/// `ahash` distributes so uniformly (or so adversarially) that
/// `10M` candidate keys yield fewer than N collisions on shard 0,
/// the search panics with a maintainer-facing message.
const SEARCH_BOUND: u32 = 10_000_000;

/// Target shard for the attack. Any shard would do; 0 is
/// deterministic and matches the L840 module-level reasoning.
const TARGET_SHARD: usize = 0;

/// Derive N attacker keys that all route to `TARGET_SHARD` under
/// `LOOM_SEED`. Returns the keys as `Vec<Vec<u8>>` in iteration
/// order. Panics if the search bound is hit before N keys are
/// found — the panic message names this plan and the workspace
/// `ahash` pin location so a maintainer knows where to bump.
fn derive_attacker_keys() -> Vec<Vec<u8>> {
    let seeded = ShardedKeyIndex::with_seed(LOOM_SEED);
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(N);
    for i in 0..SEARCH_BOUND {
        let candidate = format!("dos-{i}").into_bytes();
        if seeded.shard_for_test(&candidate) == TARGET_SHARD {
            out.push(candidate);
            if out.len() >= N {
                return out;
            }
        }
    }
    panic!(
        "L842 search exhausted: only {} of N={N} keys collided on shard {TARGET_SHARD} \
         within {SEARCH_BOUND} candidates. ahash distribution may have changed; \
         see .planning/l842-keyindex-dos-test.plan.md and the workspace ahash pin \
         in /Cargo.toml [workspace.dependencies]. Either bump SEARCH_BOUND or \
         pick a different TARGET_SHARD.",
        out.len()
    );
}

// === Test 0 — pin the SHARD_COUNT_FOR_TEST contract ===

/// Pins that `SHARD_COUNT_FOR_TEST == 64`. The lib unit test
/// `shard_count_for_test_pins_64` already does this; this
/// integration-side mirror catches a mis-export of the constant
/// across the test-seed feature boundary (e.g., a refactor that
/// inlines the value on one side of the gate but not the other).
#[test]
fn shard_count_for_test_is_64() {
    assert_eq!(SHARD_COUNT_FOR_TEST, 64);
}

// === Test 1 — attack reproducibility under fixed seed ===

/// Under `LOOM_SEED`, an attacker constructs N keys that all
/// route to a single shard. Operations on those keys
/// (`put`, `tombstone`, `since`) all go to one shard's
/// `RwLock` — the structural `DoS` the ROADMAP describes.
#[test]
fn attacker_can_pile_keys_onto_one_shard_under_fixed_seed() {
    let attacker_keys = derive_attacker_keys();
    let idx = ShardedKeyIndex::with_seed(LOOM_SEED);

    // Assertion 1: every collected key routes to TARGET_SHARD.
    for k in &attacker_keys {
        assert_eq!(
            idx.shard_for_test(k),
            TARGET_SHARD,
            "attacker key {k:?} did not route to TARGET_SHARD={TARGET_SHARD}"
        );
    }

    // Assertion 2: all puts succeed; len equals collection size.
    // Note: the assertion compares against `attacker_keys.len()`,
    // not `N`, so a future change to the search-bound logic does
    // not silently desync this check.
    for (i, k) in attacker_keys.iter().enumerate() {
        let rev = i64::try_from(i).unwrap() + 1;
        idx.put(k, Revision::new(rev, 0))
            .expect("put on freshly-derived attacker key must succeed");
    }
    assert_eq!(
        idx.len(),
        attacker_keys.len(),
        "every attacker key must land in the index exactly once"
    );

    // Assertion 3: tombstone + since round-trip works on the
    // colliding shard. Defense-in-depth — proves that operations
    // on the worst-case shard aren't silently rejected.
    let tombstone_rev = i64::try_from(N).unwrap() + 1_000;
    let half = attacker_keys.len() / 2;
    for k in &attacker_keys[..half] {
        idx.tombstone(k, Revision::new(tombstone_rev, 0))
            .expect("tombstone on existing key must succeed");
    }
    let mut revs: Vec<Revision> = Vec::new();
    idx.since(&attacker_keys[0], 0, &mut revs);
    assert!(
        revs.len() >= 2,
        "tombstoned key must have at least its put and tombstone in since(0): got {revs:?}"
    );
}

// === Test 2 — CSPRNG seeding redistributes attacker keys ===

/// Under fresh `ShardedKeyIndex::new()` (CSPRNG seed), the same
/// attacker key set distributes across shards with
/// `max_per_shard <= 2 * mean`. ROADMAP-mandated `2x mean`
/// threshold.
///
/// Multiplicative Chernoff: P[X >= (1+δ)μ] ≤ exp(-δ²μ/(2+δ)).
/// δ=1, μ=156: exp(-1²·156/3) = exp(-52) ≈ 2.6e-23 per shard.
/// Union-bound over 64 shards: 64 · 2.6e-23 ≈ 1.7e-21 ≈ 2^-69.
/// The `2 * mean` threshold is non-flaky at `N = 10_000`. Do not
/// tighten without re-running the math; a tighter δ would drag
/// the bound back into flake territory. Sibling test
/// `production_new_distributes_keys` at
/// `src/sharded_key_index.rs` uses `3x mean` for N=1000 with the
/// same justification template.
#[test]
fn production_seeding_redistributes_attacker_keys_within_2x_mean() {
    let attacker_keys = derive_attacker_keys();
    let production = ShardedKeyIndex::new();
    let counts = per_shard_counts(&production, &attacker_keys);

    let total: usize = counts.iter().sum();
    assert_eq!(total, attacker_keys.len(), "no key lost or double-counted");

    let mean = attacker_keys.len() / SHARD_COUNT_FOR_TEST;
    let max = counts.iter().copied().max().unwrap();
    let threshold = mean.saturating_mul(2);
    assert!(
        max <= threshold,
        "production CSPRNG seeding failed to redistribute attacker keys: \
         max={max} > threshold=2*mean={threshold} (mean={mean}). \
         Per-shard counts: {counts:?}"
    );
}

// === Test 3 — different fixed seed also redistributes ===

/// Same property as Test 2 but with a fixed non-LOOM seed.
/// Guards against the "Test 2 happened to work because the OS
/// RNG produced a friendly seed" failure mode: this test pins
/// the redistribution under a deterministic seed distinct from
/// `LOOM_SEED`. If Test 3 fails, ahash's per-seed distribution
/// has degraded for this specific N — the failure points at
/// ahash, not at the test's RNG.
#[test]
fn colliding_keys_redistribute_under_a_different_fixed_seed() {
    let attacker_keys = derive_attacker_keys();
    let other_seed = [0xAA_u8; 32];
    let other = ShardedKeyIndex::with_seed(other_seed);
    let counts = per_shard_counts(&other, &attacker_keys);

    let total: usize = counts.iter().sum();
    assert_eq!(total, attacker_keys.len(), "no key lost or double-counted");

    let mean = attacker_keys.len() / SHARD_COUNT_FOR_TEST;
    let max = counts.iter().copied().max().unwrap();
    let threshold = mean.saturating_mul(2);
    assert!(
        max <= threshold,
        "fixed [0xAA; 32] seed failed to redistribute attacker keys: \
         max={max} > threshold=2*mean={threshold} (mean={mean}). \
         Per-shard counts: {counts:?}"
    );
}

// === Test 4 — round-trip ops on the colliding shard ===

/// Every `put → tombstone → put` sequence on the worst-case
/// colliding shard must complete with correct visibility. No
/// concurrency — pure correctness on the structurally worst
/// routing. The L841 loom tests pin two-key, two-shard
/// interleavings; this pins many-key, one-shard sequential ops.
/// A regression catch for any future change that might (e.g.)
/// deadlock on same-shard re-entrant access.
#[test]
fn attack_round_trips_through_index_operations() {
    let attacker_keys = derive_attacker_keys();
    let idx = ShardedKeyIndex::with_seed(LOOM_SEED);
    let n_i64 = i64::try_from(attacker_keys.len()).unwrap();

    // Phase 1: put every key at rev (i + 1, 0).
    for (i, k) in attacker_keys.iter().enumerate() {
        let i64_i = i64::try_from(i).unwrap();
        idx.put(k, Revision::new(i64_i + 1, 0))
            .expect("phase-1 put must succeed");
    }

    // Phase 2: tombstone every other key at rev (N + i + 1, 0).
    for (i, k) in attacker_keys.iter().enumerate() {
        if i % 2 != 0 {
            continue;
        }
        let i64_i = i64::try_from(i).unwrap();
        idx.tombstone(k, Revision::new(n_i64 + i64_i + 1, 0))
            .expect("phase-2 tombstone must succeed");
    }

    // Phase 3: re-put the tombstoned half at rev (2N + i + 1, 0).
    for (i, k) in attacker_keys.iter().enumerate() {
        if i % 2 != 0 {
            continue;
        }
        let i64_i = i64::try_from(i).unwrap();
        idx.put(k, Revision::new((2 * n_i64) + i64_i + 1, 0))
            .expect("phase-3 re-put must succeed");
    }

    // Verify the latest-rev visibility for both halves.
    let final_rev = (3 * n_i64) + 1;
    for (i, k) in attacker_keys.iter().enumerate() {
        let at = idx
            .get(k, final_rev)
            .expect("get at final rev must find key");
        let i64_i = i64::try_from(i).unwrap();
        if i % 2 == 0 {
            // Re-put half: latest rev is (2N + i + 1, 0).
            assert_eq!(
                at.modified,
                Revision::new((2 * n_i64) + i64_i + 1, 0),
                "re-put key {k:?} (i={i}) wrong modified rev"
            );
        } else {
            // Untouched-by-tombstone half: latest rev is (i + 1, 0).
            assert_eq!(
                at.modified,
                Revision::new(i64_i + 1, 0),
                "untouched key {k:?} (i={i}) wrong modified rev"
            );
        }
    }
}

// === Test 5 — two CSPRNG seedings produce different routings ===

/// The production-defense claim is "every process gets a
/// different seed." Two independent `ShardedKeyIndex::new()`
/// instances must route at least one of the N attacker keys to
/// different shards. If a future refactor accidentally makes
/// `RandomState::new()` deterministic (e.g., re-uses a process-
/// global), this test fails fast.
///
/// Probability of false-positive (two random `RandomState`s
/// producing identical routings on N keys) is bounded by
/// `(1/SHARD_COUNT)^N = (1/64)^10000`, i.e., zero.
#[test]
fn two_csprng_seedings_route_at_least_one_key_differently() {
    let attacker_keys = derive_attacker_keys();
    let idx_a = ShardedKeyIndex::new();
    let idx_b = ShardedKeyIndex::new();

    let differs = attacker_keys
        .iter()
        .any(|k| idx_a.shard_for_test(k) != idx_b.shard_for_test(k));

    assert!(
        differs,
        "two ShardedKeyIndex::new() instances produced identical routings on \
         all N={N} attacker keys — RandomState::new() may have lost its \
         CSPRNG path"
    );
}

// === Test 6 — CSPRNG seeding differs from fixed LOOM_SEED ===

/// Regression catch for a refactor that makes `new()` and
/// `with_seed(LOOM_SEED)` accidentally identical (e.g., `new()`
/// silently losing its CSPRNG path and falling back to a
/// constant). At least one of the N attacker keys must route
/// differently.
#[test]
fn csprng_seeding_differs_from_fixed_loom_seed() {
    let attacker_keys = derive_attacker_keys();
    let production = ShardedKeyIndex::new();
    let fixed = ShardedKeyIndex::with_seed(LOOM_SEED);

    let differs = attacker_keys
        .iter()
        .any(|k| production.shard_for_test(k) != fixed.shard_for_test(k));

    assert!(
        differs,
        "ShardedKeyIndex::new() routed all N={N} attacker keys identically \
         to ShardedKeyIndex::with_seed(LOOM_SEED) — new() may have lost its \
         CSPRNG path"
    );
}

// === helpers ===

/// Compute per-shard key counts for a given index and key set.
/// Returns a `[usize; SHARD_COUNT_FOR_TEST]`-shaped `Vec`. (The
/// constant is opaque at type-level outside the lib crate, so a
/// `Vec` rather than a fixed-size array — the assertion sites
/// above use `.iter().max()` etc. and don't care about the
/// fixed-array shape.)
fn per_shard_counts(idx: &ShardedKeyIndex, keys: &[Vec<u8>]) -> Vec<usize> {
    let mut counts = vec![0_usize; SHARD_COUNT_FOR_TEST];
    for k in keys {
        let s = idx.shard_for_test(k);
        counts[s] = counts[s].saturating_add(1);
    }
    counts
}
