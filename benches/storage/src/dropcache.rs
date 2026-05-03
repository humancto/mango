//! Cold-cache primitive for L829 Phase 1 (S2 in the plan).
//!
//! Two-stage drop on Linux (per N6 in the plan):
//!
//! 1. **Stage 1 — `posix_fadvise(POSIX_FADV_DONTNEED)`** on each
//!    open DB file. Per-file, no root required, both engines pay
//!    the same primitive symmetrically.
//! 2. **Stage 2 — `echo 3 > /proc/sys/vm/drop_caches`**. Best-
//!    effort, root only. If Stage 2 fails with `EACCES`, Stage 1
//!    has already evicted the per-file pages and the verdict is
//!    still `Pass`-eligible.
//!
//! On macOS / other platforms there is no reliable userspace API
//! to evict already-cached file pages, so the verdict is
//! [`ColdCacheVerdict::Incomplete`] and the gate refuses to
//! satisfy L829's acceptance check (Linux-only per plan §"Phase 1
//! acceptance — Linux only").

use std::path::{Path, PathBuf};

/// Outcome of attempting a cold-cache drop. The plan's three
/// states (S2): `Pass` (cache evicted), `Fail` (drop attempted
/// and errored — invalid run), `Incomplete` (drop unsupported on
/// this platform / privilege level — gate refuses to grade).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColdCacheVerdict {
    /// Cache evicted; the subsequent read loop measures cold-cache
    /// behaviour.
    Pass,

    /// Drop attempted and errored. The run is invalid — the
    /// harness must abort.
    Fail(String),

    /// Drop unsupported on this platform / privilege level. The
    /// metric is marked `incomplete`; the gate refuses to count
    /// it as either `tied` or `passing`.
    Incomplete(String),
}

impl ColdCacheVerdict {
    /// Stable string representation used in the result JSON.
    /// Matches the gate's enum lookup table.
    #[must_use]
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail(_) => "fail",
            Self::Incomplete(_) => "incomplete",
        }
    }
}

/// Platform/privilege capability for cold-cache eviction.
/// Returned by [`probe_capability`] at startup so the harness can
/// short-circuit on macOS without doing per-run filesystem
/// probes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropCacheCapability {
    /// Linux with Stage 1 (`posix_fadvise`) available.
    /// `drop_caches_writable` indicates whether Stage 2 (root
    /// write to `/proc/sys/vm/drop_caches`) is also available.
    Linux { drop_caches_writable: bool },

    /// macOS — `F_NOCACHE` disables future caching but does not
    /// evict already-cached pages. Cold-cache verdict will be
    /// [`ColdCacheVerdict::Incomplete`].
    Macos,

    /// Anything else (Windows, BSD, …). Also `Incomplete`.
    Other,
}

/// Probe the runtime environment for cold-cache eviction
/// capability. The probe is read-only on macOS / Other; on Linux
/// it attempts a no-op-but-actionable probe of
/// `/proc/sys/vm/drop_caches` to decide whether Stage 2 is
/// available.
#[must_use]
pub fn probe_capability() -> DropCacheCapability {
    #[cfg(target_os = "linux")]
    {
        // Probe writability of /proc/sys/vm/drop_caches by trying
        // to open it for write. We do NOT actually write a value
        // — that would drop caches as a side effect. `OpenOptions`
        // with `.write(true)` is enough to surface EACCES on a
        // non-root caller.
        use std::fs::OpenOptions;
        let writable = OpenOptions::new()
            .write(true)
            .open("/proc/sys/vm/drop_caches")
            .is_ok();
        DropCacheCapability::Linux {
            drop_caches_writable: writable,
        }
    }
    #[cfg(target_os = "macos")]
    {
        DropCacheCapability::Macos
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        DropCacheCapability::Other
    }
}

/// Drop the page cache for the given file paths. Returns the
/// verdict per the plan's three-state enum.
///
/// Behaviour by capability:
///
/// - [`DropCacheCapability::Linux`] — runs `posix_fadvise(...,
///   POSIX_FADV_DONTNEED)` on every path. If `drop_caches_writable`
///   was true at probe time, additionally writes `"3\n"` to
///   `/proc/sys/vm/drop_caches`. If Stage 1 succeeds and Stage 2
///   is unavailable, the verdict is still `Pass`.
/// - [`DropCacheCapability::Macos`] / [`DropCacheCapability::Other`]
///   → `Incomplete` with a platform-specific message.
///
/// The harness must call [`probe_capability`] once at startup and
/// pass the result here so capabilities are not re-probed under
/// the bench loop.
#[must_use]
pub fn drop_for_files(paths: &[PathBuf], capability: &DropCacheCapability) -> ColdCacheVerdict {
    match capability {
        DropCacheCapability::Linux {
            drop_caches_writable,
        } => linux_drop_for_files(paths, *drop_caches_writable),
        DropCacheCapability::Macos => ColdCacheVerdict::Incomplete(
            "macOS has no reliable userspace evict primitive (F_NOCACHE \
             disables future caching, does not evict cached pages); \
             cold-cache reads cannot be measured on this platform"
                .to_owned(),
        ),
        DropCacheCapability::Other => ColdCacheVerdict::Incomplete(
            "cold-cache eviction supported only on Linux (Stage 1 \
             posix_fadvise + optional Stage 2 drop_caches)"
                .to_owned(),
        ),
    }
}

#[cfg(target_os = "linux")]
fn linux_drop_for_files(paths: &[PathBuf], drop_caches_writable: bool) -> ColdCacheVerdict {
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd as _;

    use nix::fcntl::{posix_fadvise, PosixFadviseAdvice};

    // Stage 1: posix_fadvise(POSIX_FADV_DONTNEED) per file.
    for path in paths {
        let f = match OpenOptions::new().read(true).open(path) {
            Ok(f) => f,
            Err(err) => {
                return ColdCacheVerdict::Incomplete(format!(
                    "open {path:?} for fadvise failed: {err}"
                ));
            }
        };
        let fd = f.as_raw_fd();
        if let Err(err) = posix_fadvise(fd, 0, 0, PosixFadviseAdvice::POSIX_FADV_DONTNEED) {
            // `Incomplete` (not `Fail`): some filesystems silently
            // refuse fadvise; we don't want to abort the whole run
            // on a tmpfs / overlayfs quirk. The gate already
            // refuses `Incomplete` as a satisfaction of L829, so
            // this is the operationally-honest classification.
            return ColdCacheVerdict::Incomplete(format!(
                "posix_fadvise(POSIX_FADV_DONTNEED) on {path:?} failed: {err}"
            ));
        }
    }

    // Stage 2: best-effort root write to /proc/sys/vm/drop_caches.
    if drop_caches_writable {
        if let Err(err) = std::fs::write("/proc/sys/vm/drop_caches", "3\n") {
            // Stage 2 failed despite passing the probe (race or
            // sysctl tightening between probe and write). Stage 1
            // already evicted the per-file pages we care about, so
            // we don't promote to `Fail`. But Stage 2 was *expected*
            // to run on this platform (the probe said the file was
            // writable), and silently swallowing the failure would
            // produce a numerically optimistic cold-cache result
            // with no operator-visible trace. The honest verdict is
            // `Incomplete`: Stage 1 worked, but the contract of the
            // cold-cache primitive on Linux Tier-1 is "both stages
            // run", and we couldn't deliver the second.
            //
            // Cost: any operator who hits a sysctl race produces an
            // `Incomplete` instead of a `Pass`. The gate fails the
            // run, the operator re-runs, the bench is honest. The
            // alternative — silent `Pass` — is the bug the rust-
            // expert PR-#68 review (Risk #4) flagged.
            return ColdCacheVerdict::Incomplete(format!(
                "drop_caches write failed after probe succeeded: {err}"
            ));
        }
    }

    ColdCacheVerdict::Pass
}

#[cfg(not(target_os = "linux"))]
fn linux_drop_for_files(_paths: &[PathBuf], _drop_caches_writable: bool) -> ColdCacheVerdict {
    // Logically unreachable: on non-Linux, `probe_capability`
    // returns `Macos` / `Other`, never `Linux { ... }`. We
    // provide the stub so the module compiles on every supported
    // platform and the dispatch in `drop_for_files` is a single
    // function lookup, not a `cfg` ladder at the call site.
    ColdCacheVerdict::Incomplete("compiled without Linux-specific cold-cache primitives".to_owned())
}

/// Convenience wrapper: paths may be passed as a slice of
/// `&Path`. Internally allocates a `Vec<PathBuf>` so the cfg-
/// gated linux helper can take ownership-by-reference of stable
/// `PathBuf`s.
#[must_use]
pub fn drop_for_paths(paths: &[&Path], capability: &DropCacheCapability) -> ColdCacheVerdict {
    let owned: Vec<PathBuf> = paths.iter().map(|p| (*p).to_path_buf()).collect();
    drop_for_files(&owned, capability)
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

    #[test]
    fn probe_returns_platform_specific_capability() {
        let cap = probe_capability();
        #[cfg(target_os = "linux")]
        {
            assert!(matches!(cap, DropCacheCapability::Linux { .. }));
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(cap, DropCacheCapability::Macos);
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            assert_eq!(cap, DropCacheCapability::Other);
        }
    }

    #[test]
    fn macos_capability_yields_incomplete() {
        let verdict = drop_for_files(&[], &DropCacheCapability::Macos);
        assert!(
            matches!(verdict, ColdCacheVerdict::Incomplete(_)),
            "got {verdict:?}"
        );
        assert_eq!(verdict.as_wire_str(), "incomplete");
    }

    #[test]
    fn other_capability_yields_incomplete() {
        let verdict = drop_for_files(&[], &DropCacheCapability::Other);
        assert!(
            matches!(verdict, ColdCacheVerdict::Incomplete(_)),
            "got {verdict:?}"
        );
        assert_eq!(verdict.as_wire_str(), "incomplete");
    }

    #[test]
    fn pass_verdict_serializes_as_pass() {
        assert_eq!(ColdCacheVerdict::Pass.as_wire_str(), "pass");
    }

    #[test]
    fn fail_verdict_serializes_as_fail() {
        assert_eq!(
            ColdCacheVerdict::Fail("oom".to_owned()).as_wire_str(),
            "fail"
        );
    }

    #[test]
    fn incomplete_verdict_carries_message() {
        let v = ColdCacheVerdict::Incomplete("no /proc/sys/vm".to_owned());
        match v {
            ColdCacheVerdict::Incomplete(msg) => {
                assert!(msg.contains("/proc/sys/vm"));
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    /// On Linux, Stage 1 (per-file fadvise) succeeds on a regular
    /// file in tmpfs without root. The actual drop_caches write
    /// is gated on `drop_caches_writable: false` so this does not
    /// require sudo.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_stage1_only_passes_on_regular_file() {
        use std::io::Write as _;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"some bytes for the page cache").unwrap();
        let path = tmp.path().to_path_buf();
        let verdict = drop_for_files(
            &[path],
            &DropCacheCapability::Linux {
                drop_caches_writable: false,
            },
        );
        assert_eq!(verdict, ColdCacheVerdict::Pass);
    }

    /// On Linux, fadvise-ing a non-existent path surfaces as
    /// `Incomplete` (open fails before the syscall).
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_nonexistent_path_yields_incomplete() {
        let path = std::path::PathBuf::from("/nonexistent/path/for/dropcache/test");
        let verdict = drop_for_files(
            &[path],
            &DropCacheCapability::Linux {
                drop_caches_writable: false,
            },
        );
        assert!(
            matches!(verdict, ColdCacheVerdict::Incomplete(_)),
            "got {verdict:?}"
        );
    }

    #[test]
    fn drop_for_paths_wraps_paths() {
        let path = std::path::Path::new("/some/path");
        let paths: &[&std::path::Path] = &[path];
        let verdict = drop_for_paths(paths, &DropCacheCapability::Macos);
        assert!(
            matches!(verdict, ColdCacheVerdict::Incomplete(_)),
            "got {verdict:?}"
        );
    }
}
