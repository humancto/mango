//! Bootstrap 95 % CI + win/loss verdict logic (S1).
//!
//! Phase 1 parity bench harness (ROADMAP:829). The contract is
//! pinned in `.planning/parity-bench-harness.plan.md` §"Statistical
//! rigor — S1".
//!
//! # Verdict definition
//!
//! For each metric, we compute the **bootstrap 95 % CI of the
//! mango/bbolt ratio**:
//!
//! - throughput-style metrics: `ratio = mango_run / bbolt_run`,
//!   paired by run index — higher ratio is better for mango.
//! - latency-style metrics: `ratio = bbolt_run / mango_run` — same
//!   convention (higher means mango is faster).
//! - on-disk-size: `ratio = bbolt_size / mango_size` — higher means
//!   mango is smaller.
//!
//! Effect-size floor: `Win` iff the lower bound of the 95 % CI is
//! ≥ 1.05 (mango at least 5 % better with 95 % confidence). `Loss`
//! iff the upper bound is ≤ 0.95. Otherwise `Tie`.
//!
//! `Incomplete` is reserved for the cold-cache metric on platforms
//! where cache drop was not possible (S2 §"Verdict states"); the
//! verdict logic in this file does not mint `Incomplete` itself —
//! the metric collector emits it directly when cache drop is
//! skipped.
//!
//! # Bootstrap details
//!
//! 10 000 resamples of **paired** per-run ratios. Pairing is by
//! run index: `ratio[i] = mango[i] op bbolt[i]`. The bootstrap
//! samples the ratios with replacement (n samples drawn n times
//! over), takes the mean of each resample, and the 2.5 %/97.5 %
//! percentiles of the resample-mean distribution are the CI
//! bounds.
//!
//! # Determinism
//!
//! The bootstrap RNG is seeded by the caller. The harness uses
//! `H(workload.seed || metric_name)` so the bootstrap is
//! reproducible run-to-run. This file does not pick the seed; it
//! takes one and uses it.

use rand::Rng;
use rand::SeedableRng as _;
use rand_chacha::ChaCha20Rng;
use serde::{Deserialize, Serialize};

/// Per-metric verdict.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Lower CI bound ≥ 1.05.
    Win,
    /// Upper CI bound ≤ 0.95.
    Loss,
    /// CI brackets 1.0 with > 5 % uncertainty.
    Tie,
    /// Cold-cache metric on a platform that could not drop the
    /// page cache. Always emitted upstream, never minted by
    /// [`verdict_from_ci`].
    Incomplete,
}

/// Whether higher numbers are better for the metric.
///
/// Drives the ratio direction inside [`bootstrap_ci`] so the
/// caller does not have to flip per-call. For latency, `mango`
/// passes its values and `bbolt` passes its values; the bootstrap
/// inverts internally.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HigherIs {
    Better,
    Worse,
}

/// Effect-size lower bound: ≥ 1.05 → win.
pub const WIN_FLOOR: f64 = 1.05;
/// Effect-size upper bound: ≤ 0.95 → loss.
pub const LOSS_CEILING: f64 = 0.95;
/// Default bootstrap resample count. 10k is a defensible floor
/// for paired-mean CIs at n = 20.
pub const DEFAULT_RESAMPLES: u32 = 10_000;

/// Bootstrap-CI errors. All point at preconditions the caller is
/// expected to enforce upstream (matched run counts, non-empty
/// vectors, finite numbers); the harness validates at the stage
/// boundary, not inside the percentile calculator.
#[derive(Debug, thiserror::Error)]
pub enum StatsError {
    #[error("paired arrays differ in length: mango={mango}, bbolt={bbolt}")]
    LengthMismatch { mango: usize, bbolt: usize },
    #[error("paired arrays must have at least 2 entries to form a CI (got {0})")]
    TooFewSamples(usize),
    #[error("non-finite or non-positive value in input arrays at index {0}")]
    InvalidValue(usize),
    #[error("resamples must be > 0")]
    ZeroResamples,
}

/// Output of a paired bootstrap.
///
/// All numbers are `f64`; the harness round-trips this into the
/// per-metric record in the result JSON.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct BootstrapCi {
    pub ratio_mean: f64,
    pub ratio_lower_95: f64,
    pub ratio_upper_95: f64,
    pub verdict: Verdict,
}

/// Compute the verdict from already-known CI bounds.
///
/// Caller-supplied `lower` and `upper` should satisfy
/// `lower ≤ upper`; the function does not enforce that and will
/// return whatever the inequalities decide. `verdict_from_ci` is
/// pure and side-effect free; use it from tests, the gate binary,
/// or to re-classify pre-computed CIs.
#[must_use]
pub fn verdict_from_ci(lower: f64, upper: f64) -> Verdict {
    if lower >= WIN_FLOOR {
        Verdict::Win
    } else if upper <= LOSS_CEILING {
        Verdict::Loss
    } else {
        Verdict::Tie
    }
}

/// Paired bootstrap CI of `mango[i] / bbolt[i]` (when
/// `higher_is == Better`) or `bbolt[i] / mango[i]` (when
/// `higher_is == Worse`). The convention is that the returned
/// `ratio_*` numbers are always "higher means mango wins" so the
/// `verdict_from_ci` floors apply uniformly across throughput,
/// latency, and size metrics.
pub fn bootstrap_ci(
    mango: &[f64],
    bbolt: &[f64],
    higher_is: HigherIs,
    resamples: u32,
    rng_seed: u64,
) -> Result<BootstrapCi, StatsError> {
    if mango.len() != bbolt.len() {
        return Err(StatsError::LengthMismatch {
            mango: mango.len(),
            bbolt: bbolt.len(),
        });
    }
    if mango.len() < 2 {
        return Err(StatsError::TooFewSamples(mango.len()));
    }
    if resamples == 0 {
        return Err(StatsError::ZeroResamples);
    }

    // Build the per-run ratios up front. We invert here once so
    // the bootstrap loop is direction-agnostic: `ratios[i] > 1`
    // always means "mango is winning on run i".
    let n = mango.len();
    let mut ratios = Vec::with_capacity(n);
    for i in 0..n {
        let m = mango.get(i).copied().unwrap_or(f64::NAN);
        let b = bbolt.get(i).copied().unwrap_or(f64::NAN);
        if !m.is_finite() || !b.is_finite() || m <= 0.0 || b <= 0.0 {
            return Err(StatsError::InvalidValue(i));
        }
        let r = match higher_is {
            // throughput: mango / bbolt
            HigherIs::Better => m / b,
            // latency / size: bbolt / mango (smaller mango is better)
            HigherIs::Worse => b / m,
        };
        ratios.push(r);
    }

    let mut rng = ChaCha20Rng::seed_from_u64(rng_seed);
    let mut sample_means: Vec<f64> = Vec::with_capacity(usize_from_u32(resamples));

    for _ in 0..resamples {
        // Resample n values with replacement and take the mean.
        let mut sum = 0.0_f64;
        for _ in 0..n {
            // gen_range over `0..n`: n ≥ 2 by construction so
            // the range is non-empty.
            let idx = rng.gen_range(0..n);
            sum += ratios.get(idx).copied().unwrap_or(0.0);
        }
        // n ≥ 2 → division safe; cast lossless for n ≤ 2^53.
        sample_means.push(sum / u_to_f64(n));
    }

    sample_means.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let lower_idx = percentile_index(resamples, 0.025);
    let upper_idx = percentile_index(resamples, 0.975);
    let ratio_lower_95 = sample_means.get(lower_idx).copied().unwrap_or(f64::NAN);
    let ratio_upper_95 = sample_means.get(upper_idx).copied().unwrap_or(f64::NAN);

    let mut sum = 0.0_f64;
    for r in &ratios {
        sum += *r;
    }
    let ratio_mean = sum / u_to_f64(n);

    Ok(BootstrapCi {
        ratio_mean,
        ratio_lower_95,
        ratio_upper_95,
        verdict: verdict_from_ci(ratio_lower_95, ratio_upper_95),
    })
}

/// Pick the integer index in `[0, resamples)` corresponding to the
/// given percentile. `resamples` is u32, so the cast is safe for
/// any `q ∈ [0, 1]`.
fn percentile_index(resamples: u32, q: f64) -> usize {
    // `q.clamp(0, 1) * (resamples - 1)` rounded — index of the
    // sorted resample-mean distribution. We use `floor` to match
    // the convention of `Vec::sort_unstable_by` + index lookup;
    // the off-by-one is negligible at resamples = 10k.
    let q = q.clamp(0.0, 1.0);
    let last = u32_to_f64(resamples.saturating_sub(1));
    let idx_f = (q * last).max(0.0).min(last);
    f64_to_usize_floor(idx_f)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "resamples ≤ 1M in practice, well below f64 exact-integer range (2^53)"
)]
fn u32_to_f64(v: u32) -> f64 {
    f64::from(v)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "n ≤ 100 in practice (paired-run count); well below 2^53"
)]
fn u_to_f64(v: usize) -> f64 {
    v as f64
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "caller clamps to [0, resamples-1]; fits in usize on every supported target"
)]
fn f64_to_usize_floor(v: f64) -> usize {
    if v < 0.0 || v.is_nan() {
        return 0;
    }
    v.floor() as usize
}

fn usize_from_u32(v: u32) -> usize {
    // Always fits: u32::MAX < usize::MAX on every platform we
    // build for (32- and 64-bit). `try_from` is the policy-clean
    // form even when the conversion is infallible in practice.
    usize::try_from(v).unwrap_or(usize::MAX)
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
    use rand_chacha::ChaCha20Rng;

    /// Box-Muller transform: turn two uniform `[0, 1)` draws into a
    /// standard-normal sample. Using `u1.max(1e-12)` avoids
    /// `ln(0)` on the unlikely-but-possible 0.0 draw. The two
    /// `f64::consts::PI` mul + cos arms produce one normal sample
    /// per call (we discard the second, which is fine in tests).
    fn standard_normal<R: Rng + ?Sized>(rng: &mut R) -> f64 {
        let u1: f64 = rng.r#gen::<f64>().max(1e-12);
        let u2: f64 = rng.r#gen::<f64>();
        let r = (-2.0 * u1.ln()).sqrt();
        let theta = 2.0 * std::f64::consts::PI * u2;
        r * theta.cos()
    }

    fn paired_normal(
        mu_a: f64,
        sigma_a: f64,
        mu_b: f64,
        sigma_b: f64,
        n: usize,
        seed: u64,
    ) -> (Vec<f64>, Vec<f64>) {
        let mut rng = ChaCha20Rng::seed_from_u64(seed);
        let mut a = Vec::with_capacity(n);
        let mut b = Vec::with_capacity(n);
        for _ in 0..n {
            let za = standard_normal(&mut rng);
            let zb = standard_normal(&mut rng);
            a.push((mu_a + sigma_a * za).max(1e-9));
            b.push((mu_b + sigma_b * zb).max(1e-9));
        }
        (a, b)
    }

    #[test]
    fn verdict_from_ci_classifies_correctly() {
        assert_eq!(verdict_from_ci(1.05, 1.20), Verdict::Win);
        assert_eq!(verdict_from_ci(1.10, 1.50), Verdict::Win);
        assert_eq!(verdict_from_ci(0.80, 0.95), Verdict::Loss);
        assert_eq!(verdict_from_ci(0.50, 0.90), Verdict::Loss);
        assert_eq!(verdict_from_ci(0.96, 1.04), Verdict::Tie);
        assert_eq!(verdict_from_ci(0.90, 1.10), Verdict::Tie);
        // Boundary cases: exactly at the floor / ceiling.
        assert_eq!(verdict_from_ci(1.05, 1.10), Verdict::Win);
        assert_eq!(verdict_from_ci(0.85, 0.95), Verdict::Loss);
        // Just-inside-tie at the floor.
        assert_eq!(verdict_from_ci(1.0499, 1.10), Verdict::Tie);
    }

    /// Synthetic win: mango ~10 % better with σ=5; CI lower bound
    /// > 1.05. n = 20 is the harness's run count.
    #[test]
    fn bootstrap_classifies_win() {
        let (mango, bbolt) = paired_normal(110.0, 5.0, 100.0, 5.0, 20, 1);
        let ci = bootstrap_ci(&mango, &bbolt, HigherIs::Better, DEFAULT_RESAMPLES, 1).unwrap();
        assert_eq!(ci.verdict, Verdict::Win, "ci = {ci:?}");
        assert!(ci.ratio_lower_95 > 1.05, "lower={}", ci.ratio_lower_95);
        assert!(ci.ratio_mean > 1.05);
    }

    /// Same nominal effect but high σ — CI brackets 1.0, verdict
    /// = Tie. This is the case the original 5 %-on-medians plan
    /// would have wrongly called a win.
    #[test]
    fn bootstrap_classifies_tie_under_high_variance() {
        let (mango, bbolt) = paired_normal(110.0, 30.0, 100.0, 30.0, 20, 7);
        let ci = bootstrap_ci(&mango, &bbolt, HigherIs::Better, DEFAULT_RESAMPLES, 7).unwrap();
        assert_eq!(ci.verdict, Verdict::Tie, "ci = {ci:?}");
    }

    /// Identical distributions → Tie. Ratio ≈ 1.0; CI brackets it.
    #[test]
    fn bootstrap_classifies_tie_for_identical() {
        let (mango, bbolt) = paired_normal(100.0, 5.0, 100.0, 5.0, 20, 13);
        let ci = bootstrap_ci(&mango, &bbolt, HigherIs::Better, DEFAULT_RESAMPLES, 13).unwrap();
        assert_eq!(ci.verdict, Verdict::Tie, "ci = {ci:?}");
    }

    /// Synthetic loss: mango is 10 % WORSE on a higher-is-better
    /// metric. Ratio < 1; upper bound below 0.95.
    #[test]
    fn bootstrap_classifies_loss_for_throughput() {
        let (mango, bbolt) = paired_normal(100.0, 5.0, 110.0, 5.0, 20, 19);
        let ci = bootstrap_ci(&mango, &bbolt, HigherIs::Better, DEFAULT_RESAMPLES, 19).unwrap();
        assert_eq!(ci.verdict, Verdict::Loss, "ci = {ci:?}");
        assert!(ci.ratio_upper_95 < 0.95);
    }

    /// Latency: mango records 100 µs, bbolt 110 µs (mango faster).
    /// With `HigherIs::Worse`, the bootstrap inverts so
    /// ratio = bbolt / mango ≈ 1.10, verdict = Win.
    #[test]
    fn bootstrap_inverts_for_lower_is_better() {
        let (mango_lat, bbolt_lat) = paired_normal(100.0, 5.0, 110.0, 5.0, 20, 23);
        let ci = bootstrap_ci(
            &mango_lat,
            &bbolt_lat,
            HigherIs::Worse,
            DEFAULT_RESAMPLES,
            23,
        )
        .unwrap();
        assert_eq!(ci.verdict, Verdict::Win, "ci = {ci:?}");
        assert!(ci.ratio_mean > 1.05);
    }

    /// Latency where mango is slower: ratio < 1, verdict = Loss.
    /// The effect size is set well above the 5 % floor (mango at
    /// 120 µs vs bbolt at 100 µs) so the bootstrap upper bound
    /// lands cleanly below 0.95 across seed jitter.
    #[test]
    fn bootstrap_lower_is_better_classifies_loss_when_mango_slower() {
        let (mango_lat, bbolt_lat) = paired_normal(120.0, 5.0, 100.0, 5.0, 20, 29);
        let ci = bootstrap_ci(
            &mango_lat,
            &bbolt_lat,
            HigherIs::Worse,
            DEFAULT_RESAMPLES,
            29,
        )
        .unwrap();
        assert_eq!(ci.verdict, Verdict::Loss, "ci = {ci:?}");
    }

    /// Determinism: same seed → same CI numbers exactly.
    #[test]
    fn bootstrap_is_deterministic_for_a_seed() {
        let (mango, bbolt) = paired_normal(110.0, 5.0, 100.0, 5.0, 20, 31);
        let a = bootstrap_ci(&mango, &bbolt, HigherIs::Better, 5_000, 31).unwrap();
        let b = bootstrap_ci(&mango, &bbolt, HigherIs::Better, 5_000, 31).unwrap();
        assert_eq!(a, b);
    }

    /// Different seeds → different CI numbers (almost surely).
    /// The bootstrap is a randomised resampling procedure; two
    /// different RNG seeds over `5_000` resamples should produce CI
    /// bounds that differ by at least one ULP-class jitter. We use
    /// a generous absolute-diff floor (1e-9) rather than a strict
    /// `assert_ne!` to satisfy `clippy::float_cmp` while still
    /// catching the regression where the seed is silently ignored.
    #[test]
    fn bootstrap_seed_matters() {
        let (mango, bbolt) = paired_normal(110.0, 5.0, 100.0, 5.0, 20, 31);
        let a = bootstrap_ci(&mango, &bbolt, HigherIs::Better, 5_000, 31).unwrap();
        let b = bootstrap_ci(&mango, &bbolt, HigherIs::Better, 5_000, 32).unwrap();
        let diff = (a.ratio_lower_95 - b.ratio_lower_95).abs();
        assert!(
            diff > 1e-9,
            "two seeds collapsed to identical lower bound: a={a:?} b={b:?}"
        );
    }

    /// Length mismatch is a structural error, not a tie.
    #[test]
    fn rejects_length_mismatch() {
        let mango = vec![1.0; 20];
        let bbolt = vec![1.0; 19];
        let err = bootstrap_ci(&mango, &bbolt, HigherIs::Better, 1_000, 0).unwrap_err();
        assert!(matches!(
            err,
            StatsError::LengthMismatch {
                mango: 20,
                bbolt: 19
            }
        ));
    }

    #[test]
    fn rejects_too_few_samples() {
        let m = vec![1.0];
        let b = vec![1.0];
        let err = bootstrap_ci(&m, &b, HigherIs::Better, 1_000, 0).unwrap_err();
        assert!(matches!(err, StatsError::TooFewSamples(1)));
    }

    #[test]
    fn rejects_zero_resamples() {
        let m = vec![1.0; 20];
        let b = vec![1.0; 20];
        let err = bootstrap_ci(&m, &b, HigherIs::Better, 0, 0).unwrap_err();
        assert!(matches!(err, StatsError::ZeroResamples));
    }

    #[test]
    fn rejects_zero_or_negative_values() {
        let m = vec![1.0; 20];
        let mut b = vec![1.0; 20];
        b[7] = 0.0;
        let err = bootstrap_ci(&m, &b, HigherIs::Better, 1_000, 0).unwrap_err();
        assert!(matches!(err, StatsError::InvalidValue(7)));

        let mut b2 = vec![1.0; 20];
        b2[3] = -1.0;
        let err = bootstrap_ci(&m, &b2, HigherIs::Better, 1_000, 0).unwrap_err();
        assert!(matches!(err, StatsError::InvalidValue(3)));
    }

    #[test]
    fn rejects_nan_or_inf() {
        let m = vec![1.0; 20];
        let mut b = vec![1.0; 20];
        b[0] = f64::NAN;
        let err = bootstrap_ci(&m, &b, HigherIs::Better, 1_000, 0).unwrap_err();
        assert!(matches!(err, StatsError::InvalidValue(0)));

        let mut b2 = vec![1.0; 20];
        b2[5] = f64::INFINITY;
        let err = bootstrap_ci(&m, &b2, HigherIs::Better, 1_000, 0).unwrap_err();
        assert!(matches!(err, StatsError::InvalidValue(5)));
    }

    /// `percentile_index` boundary behaviour — the 0 % and 100 %
    /// percentiles map to the first and last index of the sorted
    /// distribution.
    #[test]
    fn percentile_index_boundaries() {
        assert_eq!(percentile_index(10_000, 0.0), 0);
        assert_eq!(percentile_index(10_000, 1.0), 9_999);
        assert_eq!(percentile_index(10_000, 0.5), 4_999);
        // Saturating: q outside [0, 1] is clamped.
        assert_eq!(percentile_index(10_000, -0.5), 0);
        assert_eq!(percentile_index(10_000, 1.5), 9_999);
    }
}
