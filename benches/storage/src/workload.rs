//! Workload spec (`benches/workloads/storage.toml`) loader.
//!
//! Phase 1 parity bench harness (ROADMAP:829). The shape is pinned in
//! `.planning/parity-bench-harness.plan.md` §"Workload spec".
//!
//! Two contracts the rest of the harness leans on:
//!
//! 1. **Frozen schema, version-gated.** A workload toml MUST carry
//!    `version = 1`. Any other value is rejected at load time, not
//!    silently coerced. Downstream metric writers may stamp the
//!    version into the result JSON without re-validating.
//! 2. **Verbatim hash.** The sha256 of the toml file bytes is
//!    captured at load time (`LoadedWorkload::sha256_hex`) so two
//!    runs with different toml hashes can be detected and refused
//!    by the gate. The hash is computed over the on-disk bytes
//!    verbatim — no whitespace canonicalization, no
//!    re-serialization round-trip — so a single byte flip in the
//!    toml is visible in the hash.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Top-level workload spec. Mirrors the toml frozen for Phase 1 in
/// `benches/workloads/storage.toml`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Workload {
    /// Schema version. Phase 1 requires `version = 1`; any other
    /// value is rejected by [`parse`].
    pub version: u32,
    /// Master RNG seed for the workload (key bytes, value bytes,
    /// op-order generation). Read-order RNG is derived per-run.
    pub seed: u64,
    pub keys: KeysSpec,
    pub values: ValuesSpec,
    pub write: WriteSpec,
    pub read: ReadSpec,
    pub range: RangeSpec,
    pub size: SizeSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct KeysSpec {
    pub total: u64,
    pub size_bytes: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ValuesSpec {
    pub size_bytes: u32,
    pub fill: ValueFill,
}

/// Value byte-pattern fill mode. `Random` (the only supported mode
/// in Phase 1) means incompressible bytes — keeps the L830 lz4
/// pipeline honest and prevents the workload from degenerating into
/// a compression benchmark.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValueFill {
    Random,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct WriteSpec {
    pub batched_size: u32,
    pub unbatched_ops: u64,
    pub batched_ops: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ReadSpec {
    pub hot: ReadVariant,
    pub cold: ReadVariant,
    pub zipfian: ReadVariant,
}

/// One read variant: hot-cache uniform, cold-cache uniform, or
/// zipfian. The `theta` and `generator` fields are required iff
/// `distribution = "zipfian"`; [`parse`] rejects a zipfian variant
/// missing either field.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ReadVariant {
    pub ops: u64,
    pub distribution: Distribution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theta: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<Generator>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Distribution {
    Uniform,
    Zipfian,
}

/// Zipfian generator family. Pinned to `YcsbScrambled` in Phase 1 —
/// see N3 in `.planning/parity-bench-harness.plan.md`. `rand_distr::Zipf`
/// is explicitly forbidden (different tail mass).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Generator {
    YcsbScrambled,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RangeSpec {
    pub sizes: Vec<u64>,
    pub ops_per_size: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SizeSpec {
    pub measure_after: String,
    pub defragment_before_measure: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io reading workload toml: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("workload toml is not valid utf-8: {0}")]
    NotUtf8(#[from] std::str::Utf8Error),
    #[error("unsupported workload version: expected 1, got {0}")]
    UnsupportedVersion(u32),
    #[error(
        "[read.zipfian] requires both `theta` and `generator` when distribution = \"zipfian\""
    )]
    ZipfianMissingFields,
    #[error("[read.zipfian] distribution must be \"zipfian\" (got \"{0:?}\")")]
    ZipfianWrongDistribution(Distribution),
    #[error("workload field out of range: {0}")]
    OutOfRange(&'static str),
}

/// A parsed workload plus the bytes it was parsed from and their
/// sha256 hash. The hash is the gate's identity check between two
/// result JSONs — two runs with different `sha256_hex` values
/// cannot be compared (caught in the gate, not here).
#[derive(Debug, Clone)]
pub struct LoadedWorkload {
    pub spec: Workload,
    pub source_bytes: Vec<u8>,
    pub sha256_hex: String,
}

/// Read a workload toml from disk and parse it. Equivalent to
/// `parse(&std::fs::read(path)?)`.
pub fn load_from_path(path: &std::path::Path) -> Result<LoadedWorkload, LoadError> {
    let bytes = std::fs::read(path)?;
    parse(&bytes)
}

/// Parse a workload toml from raw bytes. Computes the verbatim
/// sha256 of the bytes and validates the schema (version,
/// zipfian-required fields, basic numeric ranges).
pub fn parse(bytes: &[u8]) -> Result<LoadedWorkload, LoadError> {
    let s = std::str::from_utf8(bytes)?;
    let spec: Workload = toml::from_str(s)?;

    if spec.version != 1 {
        return Err(LoadError::UnsupportedVersion(spec.version));
    }

    validate_zipfian(&spec.read.zipfian)?;
    validate_ranges(&spec)?;

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let sha256_hex = hex_lower(&digest);

    Ok(LoadedWorkload {
        spec,
        source_bytes: bytes.to_vec(),
        sha256_hex,
    })
}

fn validate_zipfian(v: &ReadVariant) -> Result<(), LoadError> {
    match v.distribution {
        Distribution::Zipfian => {
            if v.theta.is_none() || v.generator.is_none() {
                return Err(LoadError::ZipfianMissingFields);
            }
            Ok(())
        }
        Distribution::Uniform => Err(LoadError::ZipfianWrongDistribution(v.distribution)),
    }
}

fn validate_ranges(spec: &Workload) -> Result<(), LoadError> {
    if spec.keys.total == 0 {
        return Err(LoadError::OutOfRange("keys.total must be > 0"));
    }
    if spec.keys.size_bytes == 0 {
        return Err(LoadError::OutOfRange("keys.size_bytes must be > 0"));
    }
    if spec.values.size_bytes == 0 {
        return Err(LoadError::OutOfRange("values.size_bytes must be > 0"));
    }
    if spec.write.batched_size == 0 {
        return Err(LoadError::OutOfRange("write.batched_size must be > 0"));
    }
    if spec.range.sizes.is_empty() {
        return Err(LoadError::OutOfRange("range.sizes must be non-empty"));
    }
    for &n in &spec.range.sizes {
        if n == 0 {
            return Err(LoadError::OutOfRange("range.sizes entries must be > 0"));
        }
    }
    Ok(())
}

const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

fn hex_lower(bytes: &[u8]) -> String {
    // HEX_CHARS holds only ASCII digits, so the bytes pushed onto
    // `out` are always valid UTF-8. We avoid the obvious
    // `String::from_utf8(...).unwrap()` to comply with the
    // workspace `clippy::unwrap_used = deny` policy by emitting
    // chars directly via `.get().copied()` (the `.unwrap_or(b'0')`
    // branch is structurally unreachable: `b >> 4` and `b & 0x0f`
    // are both in `0..=15`, and `HEX_CHARS` is a fixed
    // 16-element array).
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for &b in bytes {
        let hi = usize::from(b >> 4);
        let lo = usize::from(b & 0x0f);
        out.push(char::from(HEX_CHARS.get(hi).copied().unwrap_or(b'0')));
        out.push(char::from(HEX_CHARS.get(lo).copied().unwrap_or(b'0')));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use super::*;

    const FROZEN_PHASE_1_TOML: &str = r#"
version = 1
seed = 7782220267274245

[keys]
total = 1_000_000
size_bytes = 32

[values]
size_bytes = 1024
fill = "random"

[write]
batched_size = 64
unbatched_ops = 10_000
batched_ops = 10_000

[read.hot]
ops = 50_000
distribution = "uniform"

[read.cold]
ops = 5_000
distribution = "uniform"

[read.zipfian]
ops = 50_000
distribution = "zipfian"
theta = 0.99
generator = "ycsb_scrambled"

[range]
sizes = [100, 10_000, 100_000]
ops_per_size = 100

[size]
measure_after = "all"
defragment_before_measure = true
"#;

    #[test]
    fn frozen_phase_1_toml_round_trips() {
        let loaded = parse(FROZEN_PHASE_1_TOML.as_bytes()).expect("parse");
        let s = &loaded.spec;
        assert_eq!(s.version, 1);
        assert_eq!(s.seed, 7_782_220_267_274_245);
        assert_eq!(s.keys.total, 1_000_000);
        assert_eq!(s.keys.size_bytes, 32);
        assert_eq!(s.values.size_bytes, 1024);
        assert_eq!(s.values.fill, ValueFill::Random);
        assert_eq!(s.write.batched_size, 64);
        assert_eq!(s.read.hot.distribution, Distribution::Uniform);
        assert_eq!(s.read.zipfian.distribution, Distribution::Zipfian);
        assert!((s.read.zipfian.theta.unwrap() - 0.99).abs() < 1e-12);
        assert_eq!(s.read.zipfian.generator, Some(Generator::YcsbScrambled));
        assert_eq!(s.range.sizes, vec![100, 10_000, 100_000]);
        assert!(s.size.defragment_before_measure);
        assert_eq!(s.size.measure_after, "all");
    }

    #[test]
    fn rejects_version_other_than_1() {
        let toml = FROZEN_PHASE_1_TOML.replace("version = 1", "version = 2");
        let err = parse(toml.as_bytes()).unwrap_err();
        assert!(matches!(err, LoadError::UnsupportedVersion(2)));
    }

    #[test]
    fn rejects_version_zero() {
        let toml = FROZEN_PHASE_1_TOML.replace("version = 1", "version = 0");
        let err = parse(toml.as_bytes()).unwrap_err();
        assert!(matches!(err, LoadError::UnsupportedVersion(0)));
    }

    #[test]
    fn rejects_zipfian_missing_theta() {
        let toml = FROZEN_PHASE_1_TOML.replace("theta = 0.99\n", "");
        let err = parse(toml.as_bytes()).unwrap_err();
        assert!(matches!(err, LoadError::ZipfianMissingFields));
    }

    #[test]
    fn rejects_zipfian_missing_generator() {
        let toml = FROZEN_PHASE_1_TOML.replace("generator = \"ycsb_scrambled\"\n", "");
        let err = parse(toml.as_bytes()).unwrap_err();
        assert!(matches!(err, LoadError::ZipfianMissingFields));
    }

    #[test]
    fn rejects_unknown_distribution() {
        let toml = FROZEN_PHASE_1_TOML
            .replace("distribution = \"zipfian\"", "distribution = \"power_law\"");
        let err = parse(toml.as_bytes()).unwrap_err();
        assert!(matches!(err, LoadError::Toml(_)));
    }

    #[test]
    fn rejects_unknown_generator() {
        let toml = FROZEN_PHASE_1_TOML.replace("\"ycsb_scrambled\"", "\"rand_distr_zipf\"");
        let err = parse(toml.as_bytes()).unwrap_err();
        assert!(matches!(err, LoadError::Toml(_)));
    }

    #[test]
    fn rejects_unknown_value_fill() {
        let toml = FROZEN_PHASE_1_TOML.replace("fill = \"random\"", "fill = \"sequential\"");
        let err = parse(toml.as_bytes()).unwrap_err();
        assert!(matches!(err, LoadError::Toml(_)));
    }

    #[test]
    fn rejects_keys_total_zero() {
        let toml = FROZEN_PHASE_1_TOML.replace("total = 1_000_000", "total = 0");
        let err = parse(toml.as_bytes()).unwrap_err();
        assert!(matches!(err, LoadError::OutOfRange(msg) if msg.contains("keys.total")));
    }

    #[test]
    fn rejects_empty_range_sizes() {
        let toml = FROZEN_PHASE_1_TOML.replace("sizes = [100, 10_000, 100_000]", "sizes = []");
        let err = parse(toml.as_bytes()).unwrap_err();
        assert!(matches!(err, LoadError::OutOfRange(msg) if msg.contains("range.sizes")));
    }

    #[test]
    fn rejects_zero_in_range_sizes() {
        let toml =
            FROZEN_PHASE_1_TOML.replace("sizes = [100, 10_000, 100_000]", "sizes = [100, 0, 1000]");
        let err = parse(toml.as_bytes()).unwrap_err();
        assert!(matches!(err, LoadError::OutOfRange(msg) if msg.contains("range.sizes")));
    }

    #[test]
    fn rejects_missing_section() {
        // Drop the [range] section.
        let toml = FROZEN_PHASE_1_TOML
            .lines()
            .filter(|l| {
                !l.contains("[range]") && !l.starts_with("sizes") && !l.starts_with("ops_per_size")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let err = parse(toml.as_bytes()).unwrap_err();
        assert!(matches!(err, LoadError::Toml(_)));
    }

    #[test]
    fn sha256_hex_is_byte_verbatim() {
        // A single trailing newline difference must change the hash.
        let a = parse(FROZEN_PHASE_1_TOML.as_bytes()).expect("a");
        let with_extra_nl = format!("{FROZEN_PHASE_1_TOML}\n");
        let b = parse(with_extra_nl.as_bytes()).expect("b");
        assert_ne!(a.sha256_hex, b.sha256_hex);
        assert_eq!(a.sha256_hex.len(), 64);
        assert!(a
            .sha256_hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn sha256_hex_known_value_for_empty_input() {
        // Sanity check the hex encoder against a published SHA-256
        // test vector. SHA-256("") =
        //   e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        // The fact that we never compute SHA-256 on an empty input
        // in practice is fine — this is a unit test of the hex
        // encoder + hasher binding.
        let mut h = Sha256::new();
        h.update(b"");
        let digest = h.finalize();
        assert_eq!(
            hex_lower(&digest),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hex_lower_round_trip_byte_range() {
        // Encode 0x00..=0xff and decode by hand; the encoder must
        // produce ascending two-char pairs that map back.
        let bytes: Vec<u8> = (0u32..=255)
            .map(|n| u8::try_from(n).expect("0..=255 fits u8"))
            .collect();
        let hex = hex_lower(&bytes);
        assert_eq!(hex.len(), 512);
        for (i, b) in bytes.iter().enumerate() {
            let pair = &hex[i.checked_mul(2).expect("i*2")
                ..i.checked_mul(2)
                    .expect("i*2")
                    .checked_add(2)
                    .expect("i*2+2")];
            let parsed = u8::from_str_radix(pair, 16).expect("hex parse");
            assert_eq!(parsed, *b, "byte index {i}");
        }
    }
}
