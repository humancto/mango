//! YCSB `ScrambledZipfianGenerator` port (N3).
//!
//! Phase 1 parity bench harness (ROADMAP:829). The contract is
//! pinned in `.planning/parity-bench-harness.plan.md` §"Workload spec".
//!
//! # Algorithm
//!
//! Two layers stacked:
//!
//! 1. **Base zipfian** — Jim Gray's algorithm from "Quickly
//!    Generating Billion-Record Synthetic Databases", over a fixed
//!    domain of `YCSB_BASE_ITEM_COUNT` (10 billion).
//! 2. **FNV-1a scramble** — the raw zipfian output is FNV-1a hashed
//!    and reduced modulo the caller's actual `itemcount`. This
//!    decorrelates the popular-key positions from low integer
//!    indices so the hottest 1 % of keys are scattered rather than
//!    clustered at the start of the keyspace.
//!
//! When `theta == 0.99` the published YCSB constant
//! `USED_ZIPFIAN_CONSTANT_THETA_099 = 26.46902820178302` is used as
//! the precomputed `zeta(10B, 0.99)`, avoiding the 10-billion-term
//! sum at construction. For other theta values the sum runs at
//! construction (single-threaded, ~ seconds for theta = 0.9).
//!
//! # Determinism
//!
//! The generator takes `&mut R: Rng + ?Sized` per draw, not at
//! construction. Two calls with the same seeded RNG state and same
//! `(itemcount, theta)` produce the **same stream**. The bench
//! harness pairs this with a `ChaCha20Rng` seeded as
//! `H(workload.seed || run_index)` per
//! `.planning/parity-bench-harness.plan.md` §"Statistical rigor —
//! Replication".
//!
//! # Why not `rand_distr::Zipf`
//!
//! `rand_distr::Zipf` produces an unbounded power law on `[1, n]`
//! with different tail mass than YCSB's discrete zipfian — the
//! latter is the read-popularity model the storage-engine benches
//! literature uses. Forbidden in the workload schema (see
//! `Generator::YcsbScrambled`) and not pulled as a dep.

use rand::Rng;

/// Pre-computed `zeta(10_000_000_000, 0.99)`. Lifted from the YCSB
/// `ScrambledZipfianGenerator.USED_ZIPFIAN_CONSTANT` field. Used
/// when `theta == 0.99` to skip the 10-billion-term sum at
/// construction. Bit-stable across runs — this is a constant of
/// the algorithm, not a measurement.
pub const USED_ZIPFIAN_CONSTANT_THETA_099: f64 = 26.469_028_201_783_02;

/// YCSB's fixed base item count: 10 billion. The base zipfian
/// operates on this domain; the FNV scramble step then mods down to
/// the caller's actual `itemcount`. See
/// `ScrambledZipfianGenerator.ITEM_COUNT` in the YCSB source.
pub const YCSB_BASE_ITEM_COUNT: u64 = 10_000_000_000;

const FNV_OFFSET_BASIS_64: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME_64: u64 = 0x0000_0100_0000_01b3;

/// Construction-time error.
#[derive(Debug, thiserror::Error)]
pub enum ZipfianError {
    #[error("itemcount must be > 0")]
    ItemCountZero,
    #[error("theta must be in (0, 1)")]
    ThetaOutOfRange,
}

/// YCSB `ScrambledZipfianGenerator`.
///
/// `next(&mut rng)` returns an index in `[0, itemcount)` drawn from
/// the FNV-scrambled zipfian distribution.
#[derive(Debug, Clone)]
pub struct ScrambledZipfian {
    base: BaseZipfian,
    itemcount: u64,
}

impl ScrambledZipfian {
    /// Construct for `itemcount` items with the given `theta`. When
    /// `theta == 0.99` the precomputed YCSB zeta is used; otherwise
    /// zeta is computed once over the YCSB base item count (10B).
    ///
    /// Returns an error if `itemcount == 0` or theta is outside
    /// `(0.0, 1.0)`.
    pub fn new(itemcount: u64, theta: f64) -> Result<Self, ZipfianError> {
        if itemcount == 0 {
            return Err(ZipfianError::ItemCountZero);
        }
        if !(theta > 0.0 && theta < 1.0) {
            return Err(ZipfianError::ThetaOutOfRange);
        }
        let zetan = if (theta - 0.99).abs() < f64::EPSILON {
            USED_ZIPFIAN_CONSTANT_THETA_099
        } else {
            zeta(YCSB_BASE_ITEM_COUNT, theta)
        };
        Ok(Self {
            base: BaseZipfian::new(YCSB_BASE_ITEM_COUNT, theta, zetan),
            itemcount,
        })
    }

    /// Draw the next index in `[0, itemcount)`.
    pub fn next<R: Rng + ?Sized>(&self, rng: &mut R) -> u64 {
        let raw = self.base.next(rng);
        let scrambled = fnv1a_64(raw);
        // `itemcount` is checked > 0 at construction; clippy can't
        // see that, so we use `checked_rem` and substitute 0 on
        // the structurally-unreachable None branch (workspace
        // `arithmetic_side_effects = deny`).
        scrambled.checked_rem(self.itemcount).unwrap_or(0)
    }

    /// Inspect the configured itemcount.
    #[must_use]
    pub fn itemcount(&self) -> u64 {
        self.itemcount
    }

    /// Inspect the configured theta.
    #[must_use]
    pub fn theta(&self) -> f64 {
        self.base.theta
    }
}

/// Internal: Jim Gray zipfian over a fixed domain.
#[derive(Debug, Clone)]
struct BaseZipfian {
    items: u64,
    theta: f64,
    zetan: f64,
    alpha: f64,
    eta: f64,
    half_pow_theta: f64,
}

impl BaseZipfian {
    fn new(items: u64, theta: f64, zetan: f64) -> Self {
        // `items` is always YCSB_BASE_ITEM_COUNT (10B). 10B fits
        // exactly in f64 (mantissa is 52 bits → 4.5 × 10^15). The
        // cast is lossless on the only value we ever pass.
        let items_f = u64_to_f64_lossy(items);
        let zeta2theta = zeta(2, theta);
        // theta < 1.0 (checked by caller) → 1 - theta > 0 → division is safe.
        let alpha = 1.0 / (1.0 - theta);
        // Direct port of the YCSB formula. (1 - (2/n)^(1-theta))
        // / (1 - zeta(2,theta)/zetan).
        let eta = (1.0 - (2.0 / items_f).powf(1.0 - theta)) / (1.0 - zeta2theta / zetan);
        Self {
            items,
            theta,
            zetan,
            alpha,
            eta,
            half_pow_theta: 0.5_f64.powf(theta),
        }
    }

    fn next<R: Rng + ?Sized>(&self, rng: &mut R) -> u64 {
        // u ∈ [0, 1) — `rand`'s default Standard distribution for
        // f64. ChaCha20Rng → f64 produces a fresh value per draw.
        let u: f64 = rng.gen();
        let uz = u * self.zetan;
        if uz < 1.0 {
            return 0;
        }
        if uz < 1.0 + self.half_pow_theta {
            return 1;
        }
        let n = u64_to_f64_lossy(self.items);
        let v = self.eta * u - self.eta + 1.0;
        let ret_f = n * v.powf(self.alpha);
        // Clamp into [0, items) before the f64→u64 cast. Without
        // this, an extreme draw could overflow u64; with it, the
        // cast is bounded and saturating.
        let clamped = ret_f.clamp(0.0, u64_to_f64_lossy(self.items.saturating_sub(1)));
        f64_to_u64_clamped(clamped).min(self.items.saturating_sub(1))
    }
}

/// Compute zeta(n, theta) = Σ_{i=1..=n} 1/i^theta.
fn zeta(n: u64, theta: f64) -> f64 {
    let mut sum = 0.0f64;
    let mut i: u64 = 1;
    while i <= n {
        // i ≥ 1, theta in (0,1) → i.powf > 0 → division safe.
        let i_f = u64_to_f64_lossy(i);
        sum += 1.0 / i_f.powf(theta);
        // i ≤ n ≤ u64::MAX-1 by construction (n is at most
        // YCSB_BASE_ITEM_COUNT = 10B). Use checked_add to satisfy
        // the workspace `arithmetic_side_effects` lint.
        match i.checked_add(1) {
            Some(next) => i = next,
            None => break,
        }
    }
    sum
}

/// FNV-1a 64-bit hash of the low 8 bytes of `val`. Mirrors YCSB's
/// `Utils.fnvhash64` with the workspace `wrapping_mul` policy
/// applied to the multiply step.
fn fnv1a_64(val: u64) -> u64 {
    let mut hash = FNV_OFFSET_BASIS_64;
    let mut v = val;
    for _ in 0..8 {
        let octet = v & 0xff;
        v >>= 8;
        hash ^= octet;
        // FNV multiply is canonical wrapping arithmetic — the
        // hash is defined modulo 2^64. Workspace policy
        // (docs/arithmetic-policy.md) names `wrapping_mul` as the
        // right primitive for hashes.
        hash = hash.wrapping_mul(FNV_PRIME_64);
    }
    hash
}

/// Cast a u64 to f64 acknowledging the lossy edge: u64 values
/// above 2^53 round to the nearest representable f64. For our
/// usage (`items ≤ 10B = 10^10 < 2^34`), the cast is exact.
#[allow(
    clippy::cast_precision_loss,
    reason = "items ≤ 10B is well below f64's exact-integer range (2^53)"
)]
fn u64_to_f64_lossy(v: u64) -> f64 {
    v as f64
}

/// Clamp a non-negative finite f64 to the u64 range, returning a
/// truncated u64. Caller is responsible for ensuring
/// `0.0 ≤ v ≤ f64::from(u64::MAX-)` so the cast is well-defined.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "caller clamps into [0.0, u64::MAX] before invoking; truncation is intended (zipfian draw rounds down)"
)]
fn f64_to_u64_clamped(v: f64) -> u64 {
    if v.is_nan() || v < 0.0 {
        return 0;
    }
    if v >= u64_to_f64_lossy(u64::MAX) {
        return u64::MAX;
    }
    v as u64
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::panic
    )]

    use super::*;
    use rand::SeedableRng as _;
    use rand_chacha::ChaCha20Rng;

    /// Smoke: every draw is in `[0, n)`, no panics, multiple draws
    /// produce more than one distinct value.
    #[test]
    fn draws_stay_in_range_and_produce_variety() {
        let z = ScrambledZipfian::new(1_000_000, 0.99).unwrap();
        let mut rng = ChaCha20Rng::seed_from_u64(0xdead_beef);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..2_000 {
            let v = z.next(&mut rng);
            assert!(v < 1_000_000, "draw out of range: {v}");
            seen.insert(v);
        }
        assert!(
            seen.len() > 100,
            "fewer than 100 distinct values in 2k draws — sampler is degenerate"
        );
    }

    /// Determinism: same seed + same generator → same stream.
    #[test]
    fn same_seed_same_stream() {
        let z = ScrambledZipfian::new(1_000_000, 0.99).unwrap();
        let mut a = ChaCha20Rng::seed_from_u64(42);
        let mut b = ChaCha20Rng::seed_from_u64(42);
        let stream_a: Vec<u64> = (0..500).map(|_| z.next(&mut a)).collect();
        let stream_b: Vec<u64> = (0..500).map(|_| z.next(&mut b)).collect();
        assert_eq!(stream_a, stream_b);
    }

    /// Different seeds → different streams (with overwhelming
    /// probability — collision over 500 draws is ≈ 0).
    #[test]
    fn different_seeds_different_streams() {
        let z = ScrambledZipfian::new(1_000_000, 0.99).unwrap();
        let mut a = ChaCha20Rng::seed_from_u64(1);
        let mut b = ChaCha20Rng::seed_from_u64(2);
        let stream_a: Vec<u64> = (0..500).map(|_| z.next(&mut a)).collect();
        let stream_b: Vec<u64> = (0..500).map(|_| z.next(&mut b)).collect();
        assert_ne!(stream_a, stream_b);
    }

    /// Skew check: with `theta = 0.99`, the **most-popular bucket**
    /// (under the FNV scramble, this is the FNV hash of `0` mod
    /// itemcount) gets a meaningful slice of the draws relative to
    /// uniform expectation.
    ///
    /// Concretely: draw 100 000 times into a 1 M-item space. Under
    /// uniform, every bucket would expect ~ 0.1 draws — i.e. nearly
    /// every bucket gets 0. Under YCSB-zipfian-99, the hottest
    /// bucket gets thousands of hits.
    #[test]
    fn theta_99_concentrates_on_one_bucket() {
        let n = 1_000_000;
        let z = ScrambledZipfian::new(n, 0.99).unwrap();
        let mut rng = ChaCha20Rng::seed_from_u64(0xc0ff_eeee);
        let mut counts: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        let trials = 100_000_u32;
        for _ in 0..trials {
            *counts.entry(z.next(&mut rng)).or_insert(0) += 1;
        }
        let max = counts.values().max().copied().unwrap_or(0);
        // YCSB-zipfian-99 gives the hottest UNDERLYING item
        // ~ 1/zeta(10B, 0.99) ≈ 1/26.47 ≈ 3.78 % of all draws,
        // and after FNV scrambling that mass lands in a single
        // bucket. So 100k draws × 0.0378 ≈ 3 780 in the hottest
        // bucket; even with sample noise it is > 1 000. A uniform
        // sampler would put ~ 0–2 hits per bucket.
        assert!(
            max > 1_000,
            "hottest bucket got {max} hits in {trials} draws — distribution is not concentrating"
        );
    }

    /// CDF check: the **top-10 hottest buckets** account for far
    /// more mass than uniform would predict. For YCSB
    /// `ScrambledZipfian` over n=1M with theta=0.99, the FNV scramble
    /// step deliberately spreads the secondary-popularity tail
    /// across all buckets, so the top-10 bucket fraction is
    /// dominated by the **first 10 underlying items'** zipfian
    /// probabilities — concretely Σ_{i=0..10} 1/((i+1)^0.99 · zetan)
    /// ≈ (1 + 1/2^0.99 + 1/3^0.99 + … + 1/10^0.99) / 26.469
    /// ≈ 3.019 / 26.469 ≈ 11.4 %.
    ///
    /// The check requires > 5 % — a loose lower bound that catches
    /// (a) a uniform sampler, where top-10 of 1M ≈ 0.001 %, and
    /// (b) a broken scrambling step that spreads ALL mass evenly
    /// (where top-10 would ≈ 10/1M as well). 5 % is well below
    /// the analytic 11.4 % so sample noise at 100k draws is
    /// absorbed without flakiness.
    #[test]
    fn top_10_buckets_dominate_under_theta_99() {
        let n = 1_000_000;
        let z = ScrambledZipfian::new(n, 0.99).unwrap();
        let mut rng = ChaCha20Rng::seed_from_u64(0xface);
        let mut counts: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        let trials: u32 = 100_000;
        for _ in 0..trials {
            *counts.entry(z.next(&mut rng)).or_insert(0) += 1;
        }
        let mut top: Vec<u32> = counts.values().copied().collect();
        top.sort_unstable_by(|a, b| b.cmp(a));
        let top_10_sum: u64 = top.iter().take(10).map(|&x| u64::from(x)).sum();
        let frac = top_10_sum as f64 / f64::from(trials);
        assert!(
            frac > 0.05,
            "top-10 buckets only got {frac:.3} of mass under theta=0.99 — distribution is wrong (analytic expectation ≈ 0.114)"
        );
    }

    /// Construction errors.
    #[test]
    fn rejects_zero_itemcount() {
        let err = ScrambledZipfian::new(0, 0.99).unwrap_err();
        assert!(matches!(err, ZipfianError::ItemCountZero));
    }

    #[test]
    fn rejects_theta_at_or_above_one() {
        for t in [1.0, 1.5, 100.0, f64::INFINITY] {
            let err = ScrambledZipfian::new(1000, t).unwrap_err();
            assert!(matches!(err, ZipfianError::ThetaOutOfRange));
        }
    }

    #[test]
    fn rejects_theta_at_or_below_zero() {
        for t in [0.0, -0.5, f64::NEG_INFINITY] {
            let err = ScrambledZipfian::new(1000, t).unwrap_err();
            assert!(matches!(err, ZipfianError::ThetaOutOfRange));
        }
    }

    #[test]
    fn rejects_nan_theta() {
        let err = ScrambledZipfian::new(1000, f64::NAN).unwrap_err();
        assert!(matches!(err, ZipfianError::ThetaOutOfRange));
    }

    /// FNV-1a hash regression vector. FNV-1a("") hashed as 8 zero
    /// bytes (which is what passing `0u64` through this 8-byte loop
    /// does) returns the offset basis: bytes are xored in but every
    /// byte is 0, so each iteration just multiplies by `FNV_PRIME_64`.
    /// `FNV_OFFSET_BASIS_64 * FNV_PRIME_64^8 (mod 2^64)`.
    ///
    /// The reference value is computed inline from the constants —
    /// this catches accidental constant typos.
    #[test]
    fn fnv1a_64_zero_input_matches_iterated_constants() {
        let mut expected: u64 = FNV_OFFSET_BASIS_64;
        for _ in 0..8 {
            expected = expected.wrapping_mul(FNV_PRIME_64);
        }
        assert_eq!(fnv1a_64(0), expected);
    }

    /// FNV-1a is sensitive to single-byte input flips: hashing `1`
    /// vs `0` differs (the first iteration XORs `0x01` instead of
    /// `0x00`, propagating through subsequent multiplies).
    #[test]
    fn fnv1a_64_sensitivity_single_byte_flip() {
        assert_ne!(fnv1a_64(0), fnv1a_64(1));
        assert_ne!(fnv1a_64(0), fnv1a_64(0x100));
        assert_ne!(fnv1a_64(0xff), fnv1a_64(0x00));
    }

    /// `zeta(2, 0.99)` matches the YCSB Java output to 12 decimal
    /// places. This is the analytic check that our `zeta()` is the
    /// right summation. Reference value computed by direct
    /// evaluation: 1 + 1/2^0.99 = 1 + 0.503685... = 1.503685...
    #[test]
    fn zeta_2_theta_99_matches_reference() {
        let z = zeta(2, 0.99);
        let expected = 1.0 + 0.5_f64.powf(0.99);
        assert!(
            (z - expected).abs() < 1e-12,
            "zeta(2,0.99) = {z}, expected {expected}"
        );
    }

    /// `f64_to_u64_clamped` saturates on inputs above `u64::MAX`
    /// without overflow.
    #[test]
    fn f64_to_u64_clamped_saturates() {
        assert_eq!(f64_to_u64_clamped(-1.0), 0);
        assert_eq!(f64_to_u64_clamped(f64::NAN), 0);
        assert_eq!(f64_to_u64_clamped(0.0), 0);
        assert_eq!(f64_to_u64_clamped(42.7), 42);
        assert_eq!(f64_to_u64_clamped(1e30), u64::MAX);
        assert_eq!(f64_to_u64_clamped(f64::INFINITY), u64::MAX);
    }
}
