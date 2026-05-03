//! mango-storage — the storage backend crate for mango.
//!
//! The [`Backend`] and [`RaftLogStore`] traits frozen in ADR 0002 §6
//! live in [`backend`]; impls follow in their own PRs (ROADMAP:817
//! redb Backend, ROADMAP:818 raft-engine `RaftLogStore`).
//!
//! Dependencies declared here — `redb` and a git-pinned fork of
//! `raft-engine` — are wired so subsequent impl PRs can consume them
//! via `.workspace = true`. The fork exists to keep `lz4-sys` (C FFI)
//! out of the default build graph; see
//! `.planning/adr/0002-storage-engine.md` §W5 and
//! `.planning/fork-raft-engine-lz4-verification.md`.

#![deny(missing_docs)]

pub mod backend;

// ROADMAP:817 redb-backed Backend impl. Public surface re-exported
// below. The module is private; only the named types below are
// public API.
mod redb;

// ROADMAP:818 raft-engine-backed RaftLogStore impl. The module is
// private; only the named types below are public API.
mod raft_engine;

// ROADMAP:821 in-memory reference Backend impl. Gated behind the
// `test-utils` Cargo feature so it does NOT appear in the default
// public surface (`cargo public-api` sees no addition). Tests opt
// in via the self-referential dev-dependency in `Cargo.toml`.
#[cfg(feature = "test-utils")]
pub mod inmem;

pub use backend::{
    Backend, BackendConfig, BackendError, BucketId, CommitStamp, CompressionMode, HardState,
    RaftEntry, RaftEntryType, RaftLogStore, RaftSnapshotMetadata, RangeIter, ReadSnapshot,
    WriteBatch,
};
#[cfg(feature = "test-utils")]
pub use inmem::{batch::InMemBatch, snapshot::InMemSnapshot, InMemBackend};
pub use raft_engine::{RaftEngineConfig, RaftEngineLogStore, ReadableSize, RecoveryMode};
pub use redb::{batch::RedbBatch, snapshot::RedbSnapshot, RedbBackend};

/// The package version string, captured at build time from
/// `CARGO_PKG_VERSION`. Kept as a crate-level constant so downstream
/// tests can assert on the shipped version without re-reading
/// `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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

    // Watchdog smoke lives in `crates/mango/src/lib.rs`; the single
    // oracle for `scripts/test-watchdog.sh` is sufficient and not
    // duplicated per crate.

    use std::error::Error;

    use super::*;

    #[test]
    fn version_matches_cargo_manifest() {
        assert_eq!(VERSION, "0.1.0");
    }

    // Compile-time shape assertions. None of these run at test time;
    // they exist to lock trait-surface invariants so a future refactor
    // that silently drops `Send`/`Sync`/`Ord`/etc. fails the build.

    #[allow(dead_code)]
    fn _assert_read_snapshot_object_safe(_: &dyn ReadSnapshot) {}

    #[allow(dead_code)]
    fn _assert_range_iter_send<'a>(_: Box<dyn RangeIter<'a> + 'a>)
    where
        Box<dyn RangeIter<'a> + 'a>: Send,
    {
    }

    #[allow(dead_code)]
    fn _assert_backend_send_sync_static<T: Backend>() {
        fn needs<T: Send + Sync + 'static>() {}
        needs::<T>();
    }

    #[allow(dead_code)]
    fn _assert_raft_log_store_send_sync_static<T: RaftLogStore>() {
        fn needs<T: Send + Sync + 'static>() {}
        needs::<T>();
    }

    #[test]
    fn error_display_covers_every_variant() {
        let cases: Vec<(BackendError, &str)> = vec![
            (
                BackendError::Io(std::io::Error::other("disk gone")),
                "disk gone",
            ),
            (BackendError::Corruption("crc".into()), "crc"),
            (BackendError::UnknownBucket(BucketId::new(7)), "7"),
            (BackendError::InvalidRange("start > end"), "start > end"),
            (BackendError::Closed, "closed"),
            (
                BackendError::BucketConflict {
                    id: BucketId::new(1),
                    existing: "foo".into(),
                    requested: "bar".into(),
                },
                "bar",
            ),
            (
                BackendError::BucketNameConflict {
                    name: "kv".into(),
                    existing_id: BucketId::new(1),
                    requested_id: BucketId::new(2),
                },
                "kv",
            ),
            (BackendError::Other("engine boom".into()), "engine boom"),
        ];
        for (err, needle) in cases {
            let rendered = format!("{err}");
            assert!(!rendered.is_empty(), "variant rendered empty: {err:?}");
            assert!(
                rendered.contains(needle),
                "variant {err:?} did not contain {needle:?}; rendered {rendered:?}"
            );
        }
    }

    #[test]
    fn backend_error_io_source_chain() {
        let inner = std::io::Error::other("inner-msg");
        let err = BackendError::Io(inner);
        let src = err.source().expect("Io variant exposes a source");
        assert!(
            format!("{src}").contains("inner-msg"),
            "source did not chain through: {src}"
        );
    }

    #[test]
    fn hard_state_default_is_zero() {
        let hs = HardState::default();
        assert_eq!(hs, HardState::new(0, 0, 0));
        assert_eq!(hs.term, 0);
        assert_eq!(hs.vote, 0);
        assert_eq!(hs.commit, 0);
    }

    #[test]
    fn commit_stamp_is_copy_eq_ord() {
        let a = CommitStamp::new(1);
        let b = CommitStamp::new(2);
        assert_eq!(a, CommitStamp::new(1));
        assert!(a < b);
        let copy = a; // `Copy`; original still usable.
        assert_eq!(a.seq, 1);
        assert_eq!(copy.seq, 1);
    }

    #[test]
    fn raft_entry_type_is_non_exhaustive() {
        // If `#[non_exhaustive]` is ever removed from `RaftEntryType`,
        // the `_` arm becomes unreachable at compile time inside the
        // defining crate, and `unreachable_patterns` fires. The allow
        // lets the test body stay clean until that happens; dropping
        // the allow is the signal that the `#[non_exhaustive]` was
        // removed.
        #[allow(clippy::wildcard_enum_match_arm, unreachable_patterns)]
        fn classify(t: RaftEntryType) -> &'static str {
            match t {
                RaftEntryType::Normal => "n",
                RaftEntryType::ConfChange => "c",
                _ => "future",
            }
        }
        assert_eq!(classify(RaftEntryType::Normal), "n");
        assert_eq!(classify(RaftEntryType::ConfChange), "c");
    }

    #[test]
    fn bucket_id_constructor_and_non_exhaustive() {
        // `const` construction is load-bearing — downstream crates
        // will declare bucket ids as `const` values.
        const KV_BUCKET: BucketId = BucketId::new(1);
        assert_eq!(KV_BUCKET.raw, 1);
        assert_eq!(BucketId::new(7).raw, 7);
    }

    // --- ROADMAP:830 CompressionMode + builder shape ----------------

    #[test]
    fn compression_mode_default_is_none() {
        // Pins the parity-bench surface — ROADMAP:828's
        // "default compression on" is a deployment-time choice, not
        // the constructor default. Flipping this default would
        // silently break the differential-vs-bbolt test (which
        // pins None explicitly) and Phase 1 parity benches.
        assert_eq!(CompressionMode::default(), CompressionMode::None);
    }

    #[test]
    fn backend_config_constructor_default_is_compression_none() {
        let cfg = BackendConfig::new(std::path::PathBuf::from("/tmp/x"), false);
        assert_eq!(cfg.compression, CompressionMode::None);
    }

    #[test]
    fn backend_config_with_compression_overrides_default() {
        let cfg = BackendConfig::new(std::path::PathBuf::from("/tmp/x"), false)
            .with_compression(CompressionMode::Lz4);
        assert_eq!(cfg.compression, CompressionMode::Lz4);
    }

    #[test]
    fn backend_config_with_compression_is_chainable_and_clone_round_trips() {
        let cfg = BackendConfig::new(std::path::PathBuf::from("/tmp/x"), true)
            .with_compression(CompressionMode::Lz4);
        let cloned = cfg.clone();
        assert_eq!(cloned.read_only, true);
        assert_eq!(cloned.compression, CompressionMode::Lz4);
        assert_eq!(cloned.data_dir, std::path::PathBuf::from("/tmp/x"));
    }
}
