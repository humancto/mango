//! Public configuration surface for [`super::RaftEngineLogStore`].
//!
//! Wraps the subset of [`::raft_engine::Config`] we intentionally
//! expose. Fields that affect crash-consistency (`recovery_mode`) and
//! operational sizing (`target_file_size`, `purge_threshold`) are
//! first-class; everything else uses upstream defaults.
//!
//! Opaque types from raft-engine (`ReadableSize`, `RecoveryMode`) are
//! re-exported so callers don't need to import the engine crate
//! directly.

use std::path::PathBuf;

pub use ::raft_engine::{ReadableSize, RecoveryMode};

/// Configuration for opening a [`super::RaftEngineLogStore`].
///
/// Construct with [`RaftEngineConfig::new`] and (optionally) chain
/// the builder methods to override the upstream defaults. The
/// `#[non_exhaustive]` attribute forces this style so adding a new
/// field is not a breaking change for downstream crates.
///
/// # Defaults
///
/// * `target_file_size` — `128 MiB` (upstream default).
/// * `purge_threshold` — `10 GiB` (upstream default).
/// * `recovery_mode` — [`RecoveryMode::TolerateTailCorruption`]
///   (upstream default; tail corruption from dirty shutdown is
///   recoverable without operator intervention).
///
/// # Example
///
/// ```ignore
/// use std::path::PathBuf;
/// use mango_storage::{RaftEngineConfig, ReadableSize};
///
/// let cfg = RaftEngineConfig::new(PathBuf::from("/var/lib/mango/raft"))
///     .with_target_file_size(ReadableSize::mb(64))
///     .with_purge_threshold(ReadableSize::gb(4));
/// ```
#[derive(Debug, Clone)]
#[must_use]
#[non_exhaustive]
pub struct RaftEngineConfig {
    /// Directory where raft-engine stores its append-only log files.
    /// Required. Must exist; raft-engine itself creates missing
    /// directories on `Engine::open`, but a pre-flight check keeps
    /// the error surface homogeneous with [`super::super::RedbBackend`].
    pub data_dir: PathBuf,
    /// Rollover size for a single log file. Smaller values mean more
    /// files and faster purge granularity; larger values mean fewer
    /// files and less per-file overhead.
    pub target_file_size: ReadableSize,
    /// Soft ceiling on total log-file bytes before
    /// [`super::RaftEngineLogStore::purge_expired_files`] is expected
    /// to reclaim space. Raft-engine does not enforce this as a hard
    /// cap — callers must drive purge themselves.
    pub purge_threshold: ReadableSize,
    /// How raft-engine treats a dirty-shutdown tail during replay.
    /// Default is [`RecoveryMode::TolerateTailCorruption`]; switch
    /// to [`RecoveryMode::AbsoluteConsistency`] in tests that want
    /// every replay mismatch to be an error.
    pub recovery_mode: RecoveryMode,
}

impl RaftEngineConfig {
    /// Construct a config with upstream defaults for every field
    /// except `data_dir`.
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            target_file_size: ReadableSize::mb(128),
            purge_threshold: ReadableSize::gb(10),
            recovery_mode: RecoveryMode::TolerateTailCorruption,
        }
    }

    /// Set [`Self::target_file_size`].
    pub fn with_target_file_size(mut self, size: ReadableSize) -> Self {
        self.target_file_size = size;
        self
    }

    /// Set [`Self::purge_threshold`].
    pub fn with_purge_threshold(mut self, size: ReadableSize) -> Self {
        self.purge_threshold = size;
        self
    }

    /// Set [`Self::recovery_mode`].
    pub fn with_recovery_mode(mut self, mode: RecoveryMode) -> Self {
        self.recovery_mode = mode;
        self
    }

    /// Lower this config into a [`::raft_engine::Config`]. Forces
    /// `batch_compression_threshold = ReadableSize(0)` so the
    /// `lz4-compression` feature cannot be exercised at runtime —
    /// the humancto fork feature-gates the C FFI `lz4-sys` dep out
    /// of the default graph (see ADR 0002 §W5), and the sanitizer
    /// in `Config::sanitize` also forces it to zero when the cargo
    /// feature is absent. Setting it explicitly here means a future
    /// upstream bump that re-enables compression by default cannot
    /// silently change mango's on-disk posture.
    pub(crate) fn into_engine_config(self) -> ::raft_engine::Config {
        ::raft_engine::Config {
            dir: self.data_dir.to_string_lossy().into_owned(),
            target_file_size: self.target_file_size,
            purge_threshold: self.purge_threshold,
            recovery_mode: self.recovery_mode,
            batch_compression_threshold: ReadableSize(0),
            ..::raft_engine::Config::default()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstream() {
        let cfg = RaftEngineConfig::new(PathBuf::from("/tmp/r"));
        assert_eq!(cfg.target_file_size, ReadableSize::mb(128));
        assert_eq!(cfg.purge_threshold, ReadableSize::gb(10));
        assert!(matches!(
            cfg.recovery_mode,
            RecoveryMode::TolerateTailCorruption
        ));
    }

    #[test]
    fn into_engine_config_forces_compression_off() {
        let cfg = RaftEngineConfig::new(PathBuf::from("/tmp/r"));
        let lowered = cfg.into_engine_config();
        assert_eq!(
            lowered.batch_compression_threshold,
            ReadableSize(0),
            "batch_compression_threshold must be zero — lz4 FFI is \
             gated out of the build by the humancto fork (ADR 0002 §W5), \
             and forcing 0 here means a future default flip cannot \
             silently re-enable it"
        );
    }

    #[test]
    fn into_engine_config_propagates_sizes() {
        let cfg = RaftEngineConfig::new(PathBuf::from("/tmp/r"))
            .with_target_file_size(ReadableSize::mb(32))
            .with_purge_threshold(ReadableSize::gb(2));
        let lowered = cfg.into_engine_config();
        assert_eq!(lowered.target_file_size, ReadableSize::mb(32));
        assert_eq!(lowered.purge_threshold, ReadableSize::gb(2));
    }

    #[test]
    fn into_engine_config_propagates_data_dir() {
        let cfg = RaftEngineConfig::new(PathBuf::from("/var/lib/mango/raft"));
        let lowered = cfg.into_engine_config();
        assert_eq!(lowered.dir, "/var/lib/mango/raft");
    }

    #[test]
    fn builder_overrides_recovery_mode() {
        let cfg = RaftEngineConfig::new(PathBuf::from("/tmp/r"))
            .with_recovery_mode(RecoveryMode::AbsoluteConsistency);
        assert!(matches!(
            cfg.recovery_mode,
            RecoveryMode::AbsoluteConsistency
        ));
    }
}
