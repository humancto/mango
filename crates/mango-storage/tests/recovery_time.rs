//! Recovery-time measurement (ROADMAP:822).
//!
//! Measures wall-clock time from [`mango_storage::RedbBackend::open`]
//! to first successful snapshot read after an unclean shutdown
//! (`std::process::abort` — no destructors, no buffered-stdio flush,
//! no `Drop` for `redb::Database`) at 1, 4, and 8 GiB of data on
//! disk.
//!
//! # Why this exists
//!
//! ADR 0002 §W8 flags "no published recovery-time SLO per GB for
//! raft-engine" (and, for the same reason, redb) as an information
//! gap. The mitigation is to measure ourselves and set the budget by
//! observation: **≤ 30 seconds at 8 GiB after unclean shutdown**.
//! Exceeding this budget triggers an engine swap per ADR 0002 §5
//! Tier-1 trigger #4 ("Crash-recovery time > 30 seconds at 8 GiB").
//! See `.planning/adr/0002-storage-engine.md` lines 168-172, 291.
//!
//! # What this test pins (and what it does NOT pin)
//!
//! **Pinned (P1, P2, P3):**
//!
//! - **Cold-open recovery time** — wall-clock from `open(...)` to
//!   first successful `snap.get(...)` returning the recovered marker
//!   key.
//! - **Recovery integrity** — bulk-data sample probe (~64 keys
//!   distributed across the entire keyspace, each verified against
//!   an embedded `i.to_le_bytes()` index in the value) inside the
//!   timed window. Catches "redb opened but lost a chunk of pages"
//!   failure modes that a marker-only probe would miss.
//! - **The 30s/8GiB budget** — hard-asserted on the 8 GiB scenario
//!   when sufficient disk is available. Lower-bound 50ms gate
//!   catches false-pass when the timer didn't include real work.
//!
//! **Deliberately NOT pinned:**
//!
//! 1. **Real torn-fsync recovery.** The child awaits each
//!    `commit_batch` to completion before issuing the next, and
//!    before `process::abort()`. There is no in-flight fsync at
//!    abort time — the test measures recovery after a *clean fsync
//!    sequence* terminated by abort, not recovery from a torn
//!    write. Torn-fsync recovery is the scope of
//!    [`crash_recovery_eio.rs`].
//! 2. **Real-workload commit shape.** The default workload is 1 MiB
//!    per fsync (`BATCH_KEYS`=1024, 1024 B values → 1024 keys per
//!    batch → 8192 commits at 8 GiB). The original plan called for
//!    8 KiB per fsync (`BATCH_KEYS`=64) for a more etcd-like cadence,
//!    but that produced ~16K fsyncs per GiB and wedged the write
//!    phase on commodity SSDs. Recovery for redb depends on b-tree
//!    state, not commit cadence (redb is copy-on-write, no WAL
//!    replay), so coarser batches do not weaken the test. Etcd-class
//!    commit cadence is out of scope for this Phase 1 floor; the
//!    Phase 2 follow-up under ROADMAP:829 covers it.
//! 3. **Etcd-style power-loss simulation.** SIGABRT bypasses
//!    destructors but does NOT clear the page cache without an
//!    explicit drop. We attempt `drop_caches` (Linux) /
//!    `purge` (macOS) between abort and parent open; if sudo is
//!    unavailable the measurement is recorded as `cache=warm` and
//!    the budget interpretation is documented as a floor.
//! 4. **Hardware-signature gating.** The 30s budget is asserted on
//!    whatever hardware runs the test. Self-hosted bench-rig
//!    integration is the long-term answer (ROADMAP:829 +
//!    `benches/runner/HARDWARE.md`).
//!
//! # Reproducing
//!
//! ```text
//! cargo test -p mango-storage --test recovery_time -- \
//!     --ignored --nocapture recovery_time_1gib
//! ```
//!
//! `--nocapture` is required to see the
//! `MANGO_RECOVERY_TIME scenario=1GiB wall_ms=N cache=cold|warm samples_ok=64`
//! line on pass; libtest captures stdout per-test by default.
//!
//! Substitute `recovery_time_4gib` / `recovery_time_8gib` for the
//! larger scenarios (each requires ≥ 2.5× the scenario size in free
//! tempdir disk; smaller volumes soft-skip).
//!
//! # CI
//!
//! Only `recovery_time_1gib` is run in CI (GitHub Actions runners
//! have ~14 GiB free disk; the 4 and 8 GiB scenarios soft-skip
//! there). The 30s/8GiB budget is therefore a **manual-cert** gate
//! today; the bench-rig integration follow-up makes it automated.
//!
//! # MIRROR-WITH `crash_recovery_panic.rs`
//!
//! This test reuses the child-process re-exec scaffold from
//! `crash_recovery_panic.rs` (ROADMAP:825): same `MANGO_TEST_*`
//! env-var convention, same `--exact --include-ignored` re-exec,
//! same `assert_aborted(SIGABRT)` discipline. Any change to that
//! contract MUST be applied to both files. The differences are
//! captured in this module's helpers — `child_role`, `spawn_child`,
//! and `assert_aborted` are intentionally re-implemented (not
//! shared via a `mod` import) because integration-test files
//! cannot share modules without `tests/common/mod.rs` plumbing
//! that's overkill for two files.
//!
//! # Clock policy
//!
//! Uses [`std::time::Instant`] (monotonic) per `docs/time.md`. Wall
//! clock ([`std::time::SystemTime`]) can jump and is forbidden for
//! durations.

#![cfg(not(madsim))]
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::cast_possible_wrap,
    // Test target: report lines on stdout (machine-grep-able) and
    // status warnings on stderr are intentional. The harness has no
    // tracing subscriber initialized; switching to `tracing::info!`
    // would silently drop the recovery-time line that
    // scripts/parse-recovery-time.sh and the CI artifact step rely on.
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mango_storage::{
    Backend, BackendConfig, BackendError, BucketId, ReadSnapshot, RedbBackend, WriteBatch as _,
};
use tempfile::TempDir;

const KV: BucketId = BucketId::new(1);
const ENV_SCENARIO: &str = "MANGO_TEST_RECOVERY_TIME_SCENARIO";
const ENV_PATH: &str = "MANGO_TEST_RECOVERY_TIME_PATH";
const ENV_SIZE: &str = "MANGO_TEST_RECOVERY_TIME_SIZE_BYTES";

// SIGABRT signal number on every Unix the workspace targets. We
// assert positively on this rather than only `code().is_none()` so a
// child killed by a different signal (e.g. SIGSEGV from an unrelated
// bug) fails the test attribution clearly instead of being mistaken
// for a successful abort.
const SIGABRT: i32 = 6;

const VALUE_LEN: usize = 1024; // 1 KiB
                               // 1024 keys × 1024 B value = 1 MiB per `commit_batch`. Earlier
                               // design used 64 keys (8 KiB/fsync) per the plan's "realistic
                               // commit shape" goal; that produced ~16K fsyncs per GiB which
                               // wedged the write phase on commodity SSDs (16K × 6 ms ≈ 96 s
                               // just for the 1 GiB scenario). Recovery time for redb depends on
                               // the on-disk b-tree shape (same regardless of commit cadence —
                               // redb is copy-on-write, no WAL replay), so coarser batches do
                               // not weaken the recovery-time signal. The marker is still
                               // committed alone via a separate `commit_batch` call (see
                               // `run_child_writer`), so "last commit before abort survived"
                               // is still a load-bearing probe.
const BATCH_KEYS: u64 = 1024;

// Bulk-sample probe: 64 keys distributed across the keyspace, each
// verified against the embedded `i.to_le_bytes()` index. Catches
// "redb opened but lost a page-range" failure modes that a
// marker-only probe would miss. 64 is small enough that the probe
// cost (~64 cheap point lookups) does not move the 30s recovery
// gate; large enough to hit pages from across the file rather than
// just the tail.
const BULK_SAMPLE_COUNT: usize = 64;

const SCENARIO_1G: u64 = 1 << 30;
const SCENARIO_4G: u64 = 4 << 30;
const SCENARIO_8G: u64 = 8 << 30;

const BUDGET_8G_MS_MAX: u128 = 30_000;
// Lower-bound sanity: catches false-pass when the timer didn't
// include real work (e.g., the temp dir was wiped between child
// and parent, RedbBackend::open short-circuited).
const BUDGET_8G_MS_MIN: u128 = 50;

// Disk-space precondition: 2.5× the scenario size. Same fudge as
// `disk_full.rs` — redb writes meta + data + repair scratch.
fn required_free_bytes(scenario_size: u64) -> u64 {
    scenario_size + scenario_size / 2 + scenario_size
}

// `statvfs(2)` via the `nix` safe wrapper for free-disk probe.
// `set_len` ENOSPC probing is unreliable on ext4/apfs/tmpfs (lazy
// allocation); statvfs is the right primitive.
//
// On Linux/glibc `blocks_available` and `fragment_size` are
// `c_ulong`; on macOS they are `u32`. nix exposes both as `u64`
// already, so the multiplication is straight `u64 * u64` —
// `saturating_mul` defends against pathological FS reports without
// arithmetic-policy violation.
fn available_bytes(path: &Path) -> Option<u64> {
    let stat = nix::sys::statvfs::statvfs(path).ok()?;
    Some((stat.blocks_available() as u64).saturating_mul(stat.fragment_size() as u64))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheMode {
    Cold,
    Warm,
}

impl CacheMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Warm => "warm",
        }
    }
}

// Best-effort page-cache drop. Returns Cold on success, Warm if
// sudo isn't available or the platform isn't supported. Stderr is
// discarded so a denied sudo prompt doesn't pollute test output.
fn drop_page_caches() -> CacheMode {
    // Use Command with explicit args (not `sh -c`) to avoid
    // shell-injection if any future change leaks a path through.
    #[cfg(target_os = "linux")]
    {
        // Two-step: sync, then write 3 to drop_caches. Both must
        // succeed for cold cache. `sudo -n` (non-interactive)
        // returns nonzero immediately if a password would be
        // required; that's the soft-fall-through to Warm.
        let sync_ok = Command::new("sync")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !sync_ok {
            return CacheMode::Warm;
        }
        let drop_ok = Command::new("sudo")
            .args(["-n", "tee", "/proc/sys/vm/drop_caches"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
            .and_then(|mut child| {
                use std::io::Write as _;
                if let Some(stdin) = child.stdin.as_mut() {
                    let _ = stdin.write_all(b"3\n");
                }
                child.wait().ok()
            })
            .map(|s| s.success())
            .unwrap_or(false);
        if drop_ok {
            return CacheMode::Cold;
        }
        CacheMode::Warm
    }
    #[cfg(target_os = "macos")]
    {
        // `sudo -n purge` is the macOS equivalent. Same -n
        // semantics: returns nonzero immediately if a password
        // would be required.
        let ok = Command::new("sudo")
            .args(["-n", "purge"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return CacheMode::Cold;
        }
        CacheMode::Warm
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        CacheMode::Warm
    }
}

fn child_watchdog(size_bytes: u64) -> Duration {
    // Per-GiB allowance + a fixed setup overhead. Replaces the
    // inherited 60s `crash_recovery_panic.rs` watchdog which would
    // fire mid-write at 8 GiB. With BATCH_KEYS=1024 (1 MiB per
    // fsync, 1024 fsyncs per GiB), real measurements on Apple
    // Silicon SSD are ~10s/GiB; CI runners and slower volumes can
    // be 3-5x. 90s/GiB + 60s setup gives 1G=150s, 4G=420s,
    // 8G=780s ≈ 13min — comfortably inside the 30min nextest
    // recovery class.
    let gibs = (size_bytes / (1 << 30)).max(1);
    Duration::from_secs(90).saturating_mul(gibs as u32) + Duration::from_secs(60)
}

// Stable, machine-grep-able report line. Downstream tooling
// (`scripts/parse-recovery-time.sh`) parses this format. The shape
// is locked in via `format_recovery_line_shape_is_stable` below;
// changing the format breaks scrapers.
fn format_recovery_line(
    scenario: &str,
    wall_ms: u128,
    cache_mode: CacheMode,
    samples_ok: usize,
) -> String {
    format!(
        "MANGO_RECOVERY_TIME scenario={scenario} wall_ms={wall_ms} cache={cache} samples_ok={samples_ok}",
        cache = cache_mode.as_str()
    )
}

fn format_skip_line(scenario: &str, free_bytes: u64, required: u64) -> String {
    format!(
        "MANGO_RECOVERY_TIME scenario={scenario} skipped=insufficient_disk free_bytes={free_bytes} required_bytes={required}"
    )
}

fn key_at(i: u64) -> Vec<u8> {
    // 10 digits — covers 8M+ keys at 8 GiB (1 KiB values) with
    // headroom. Format is stable; parser is the embedded index in
    // the value, not the key.
    format!("k{i:010}").into_bytes()
}

fn make_value(i: u64) -> Vec<u8> {
    // Embed `i.to_le_bytes()` at [0..8]. Defeats FS dedup (apfs
    // clone, ZFS dedup) AND serves as the bulk-sample probe's
    // verification key. The remaining 1016 bytes are zeros — they
    // contribute to on-disk volume but their content is not
    // verified.
    let mut v = vec![0u8; VALUE_LEN];
    v[0..8].copy_from_slice(&i.to_le_bytes());
    v
}

fn parse_value_index(value: &[u8]) -> Option<u64> {
    if value.len() < 8 {
        return None;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&value[0..8]);
    Some(u64::from_le_bytes(buf))
}

fn child_role() -> Option<(String, PathBuf, u64)> {
    // Partial-env footgun guard: setting only one or two of the
    // three child env vars (e.g., when a developer manually exports
    // `MANGO_TEST_RECOVERY_TIME_PATH` to debug the child) would
    // silently drop into the *parent* role and recurse via
    // spawn_child, producing a confusing watchdog timeout. Detect
    // partial state explicitly and fail loudly.
    let s = std::env::var(ENV_SCENARIO).ok();
    let p = std::env::var(ENV_PATH).ok();
    let n = std::env::var(ENV_SIZE).ok();
    match (s, p, n) {
        (None, None, None) => None,
        (Some(scenario), Some(path), Some(size)) => {
            let size: u64 = size
                .parse()
                .expect("MANGO_TEST_RECOVERY_TIME_SIZE_BYTES must parse as u64");
            Some((scenario, PathBuf::from(path), size))
        }
        (s, p, n) => panic!(
            "child_role: partial env state — set ALL of {ENV_SCENARIO}/{ENV_PATH}/{ENV_SIZE} or NONE. \
             scenario={s:?} path={p:?} size={n:?}"
        ),
    }
}

fn spawn_child(
    test_name: &str,
    scenario: &str,
    db_path: &Path,
    size_bytes: u64,
) -> std::process::Output {
    Command::new(std::env::current_exe().expect("current_exe should resolve in test"))
        .arg(test_name)
        .arg("--exact")
        .arg("--include-ignored")
        .env(ENV_SCENARIO, scenario)
        .env(ENV_PATH, db_path)
        .env(ENV_SIZE, size_bytes.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn child test binary")
}

fn assert_aborted(out: &std::process::Output, ctx: &str) {
    let signal = out.status.signal();
    if signal != Some(SIGABRT) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        panic!(
            "child '{ctx}' did not abort cleanly: code={:?} signal={:?} (expected SIGABRT={SIGABRT}).\n\
             ---- child stderr ----\n{stderr}\n\
             ---- child stdout ----\n{stdout}\n",
            out.status.code(),
            signal,
        );
    }
}

// Child-side: write `total_bytes` of (key, value) data via batched
// commits, then write the marker key in its own commit, then
// `process::abort()`. The marker MUST commit alone (not batched
// with bulk) because its presence-on-recover is the first
// load-bearing assertion the parent makes.
async fn run_child_writer(path: PathBuf, total_bytes: u64, watchdog: Duration) -> ! {
    let started = Instant::now();
    let fut = async move {
        let backend = RedbBackend::open(BackendConfig::new(path, false)).expect("child open");
        backend.register_bucket("kv", KV).expect("register kv");

        let total_keys = total_bytes / (VALUE_LEN as u64);
        // Defensive: refuse to start if the workload arithmetic
        // would silently leave a fractional batch. SCENARIO_*
        // constants are powers-of-two GiB so this holds for all
        // three.
        assert!(
            total_keys.is_multiple_of(BATCH_KEYS),
            "total_keys ({total_keys}) must be multiple of BATCH_KEYS ({BATCH_KEYS})"
        );

        let mut i: u64 = 0;
        while i < total_keys {
            let mut batch = backend.begin_batch().expect("begin_batch");
            for j in 0..BATCH_KEYS {
                let k = i + j;
                let key = key_at(k);
                let value = make_value(k);
                batch.put(KV, &key, &value).expect("put");
            }
            // force_fsync=true: every batch must be durable before
            // the next begins. This ensures no in-flight fsync at
            // abort time (the recovery semantics this test pins).
            // `let _ =` because `commit_batch` returns a #[must_use]
            // `CommitStamp` we deliberately discard — the test only
            // cares that the commit fsynced.
            let _ = backend
                .commit_batch(batch, true)
                .await
                .expect("commit_batch");
            i += BATCH_KEYS;
        }

        // Marker commit — alone, fsynced, no intervening .await
        // before abort. Its presence-on-recover proves the LAST
        // commit before abort survived (vs. just "redb opened a
        // file").
        let mut marker_batch = backend.begin_batch().expect("begin_batch (marker)");
        marker_batch
            .put(KV, b"_recovery_marker", b"present")
            .expect("put (marker)");
        let _ = backend
            .commit_batch(marker_batch, true)
            .await
            .expect("commit_batch (marker)");

        // No `.await`, no yield_now between marker commit and
        // abort. Tokio cannot reorder this.
        std::process::abort();
    };
    let outcome = tokio::time::timeout(watchdog, fut).await;
    assert!(
        outcome.is_ok(),
        "child timed out before abort — write phase wedged or watchdog too tight. \
         elapsed={elapsed:?} watchdog={watchdog:?} total_bytes={total_bytes}",
        elapsed = started.elapsed(),
    );
    unreachable!("child fut must abort or time out");
}

// Parent-side: time the recovery + verify the bulk-sample probe.
// Returns (wall_ms, samples_ok). Caller asserts the budget /
// reports the line.
fn measure_recovery(db_path: &Path, total_bytes: u64) -> (u128, usize) {
    let total_keys = total_bytes / (VALUE_LEN as u64);

    let start = Instant::now();

    let backend = RedbBackend::open(BackendConfig::new(db_path.to_path_buf(), false))
        .expect("BUG: redb failed to recover from process::abort — this is the bug we're testing");

    // Registry probe — DO NOT re-register the bucket. Re-registering
    // would mask a registry-loss bug by silently re-creating the
    // binding.
    let snap = backend.snapshot().expect("snapshot");

    // Marker probe (load-bearing assertion #1: last commit survived).
    match snap.get(KV, b"_recovery_marker") {
        Ok(Some(v)) => {
            assert_eq!(
                v.as_ref(),
                b"present",
                "marker key recovered with wrong value (corruption signal)"
            );
        }
        Ok(None) => panic!("marker missing — last commit before abort lost"),
        Err(BackendError::UnknownBucket(_)) => panic!("registry lost across abort"),
        Err(e) => panic!("snapshot probe failed (marker): {e}"),
    }

    // Bulk-sample probe (load-bearing assertion #2: bulk pages
    // recovered, not just file opened). 64 keys distributed across
    // the keyspace; each value's [0..8] must equal the key's
    // index. A redb bug that "opens but loses a page range" gets
    // caught here with `samples_ok < 64`, NOT silently passed.
    let mut samples_ok = 0usize;
    for j in 1..=BULK_SAMPLE_COUNT {
        let k = total_keys
            .saturating_mul(j as u64)
            .saturating_div(BULK_SAMPLE_COUNT as u64 + 1);
        let key = key_at(k);
        match snap.get(KV, &key) {
            Ok(Some(v)) => {
                let idx =
                    parse_value_index(v.as_ref()).expect("recovered value too short for index");
                assert_eq!(
                    idx, k,
                    "bulk-sample: key {k} recovered with index {idx} (page-corruption signal)"
                );
                samples_ok += 1;
            }
            Ok(None) => panic!("bulk-sample: key {k} missing after recovery (lost page range)"),
            Err(e) => panic!("bulk-sample: snap.get({k}) failed: {e}"),
        }
    }

    let wall_ms = start.elapsed().as_millis();
    (wall_ms, samples_ok)
}

// Shared parent-side flow: precondition check, spawn child, drop
// caches, measure, emit line, return measurement for caller's
// budget assertion (if any).
struct Measurement {
    wall_ms: u128,
    cache_mode: CacheMode,
    samples_ok: usize,
}

fn run_parent_measurement(test_name: &str, scenario: &str, size_bytes: u64) -> Option<Measurement> {
    let dir = TempDir::new().expect("tempdir");

    // Precondition: enough free disk?
    let required = required_free_bytes(size_bytes);
    if let Some(free) = available_bytes(dir.path()) {
        if free < required {
            println!("{}", format_skip_line(scenario, free, required));
            return None;
        }
    } else {
        // statvfs failed — proceed anyway; soft-skip on ENOSPC
        // mid-write would surface as a child failure, which is
        // honest if uglier than a precondition skip.
        eprintln!(
            "warn: statvfs failed for {}; proceeding without precondition check",
            dir.path().display()
        );
    }

    let out = spawn_child(test_name, scenario, dir.path(), size_bytes);
    assert_aborted(&out, scenario);

    // Drop page caches between child abort and parent open. If
    // sudo is unavailable, falls through to Warm — the report line
    // records which.
    let cache_mode = drop_page_caches();

    let (wall_ms, samples_ok) = measure_recovery(dir.path(), size_bytes);
    let line = format_recovery_line(scenario, wall_ms, cache_mode, samples_ok);
    println!("{line}");

    Some(Measurement {
        wall_ms,
        cache_mode,
        samples_ok,
    })
}

// --- T1: 1 GiB ------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns child process; writes 1 GiB to tempdir; gated behind --ignored. See module doc."]
async fn recovery_time_1gib() {
    if let Some((scenario, path, size)) = child_role() {
        assert_eq!(scenario, "1GiB");
        assert_eq!(size, SCENARIO_1G);
        run_child_writer(path, size, child_watchdog(size)).await;
    }
    // Parent role.
    // 1 GiB is not budget-gated (only 8 GiB is per ADR 0002 §W8);
    // the line is printed for trend tooling, the integrity probe is
    // the only assertion here.
    if let Some(m) = run_parent_measurement("recovery_time_1gib", "1GiB", SCENARIO_1G) {
        assert_eq!(
            m.samples_ok, BULK_SAMPLE_COUNT,
            "1 GiB: bulk-sample probe found only {} of {} keys",
            m.samples_ok, BULK_SAMPLE_COUNT
        );
    }
}

// --- T2: 4 GiB ------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns child process; writes 4 GiB to tempdir; gated behind --ignored. See module doc."]
async fn recovery_time_4gib() {
    if let Some((scenario, path, size)) = child_role() {
        assert_eq!(scenario, "4GiB");
        assert_eq!(size, SCENARIO_4G);
        run_child_writer(path, size, child_watchdog(size)).await;
    }
    let m = run_parent_measurement("recovery_time_4gib", "4GiB", SCENARIO_4G);
    if let Some(m) = m {
        assert_eq!(
            m.samples_ok, BULK_SAMPLE_COUNT,
            "4 GiB: bulk-sample probe found only {} of {} keys",
            m.samples_ok, BULK_SAMPLE_COUNT
        );
        // 4 GiB also not budget-gated; trend-only.
    }
}

// --- T3: 8 GiB (ADR 0002 §W8 budget gate) ---------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns child process; writes 8 GiB to tempdir; gated behind --ignored. See module doc."]
async fn recovery_time_8gib() {
    if let Some((scenario, path, size)) = child_role() {
        assert_eq!(scenario, "8GiB");
        assert_eq!(size, SCENARIO_8G);
        run_child_writer(path, size, child_watchdog(size)).await;
    }
    let m = run_parent_measurement("recovery_time_8gib", "8GiB", SCENARIO_8G);
    let Some(m) = m else { return };

    assert_eq!(
        m.samples_ok, BULK_SAMPLE_COUNT,
        "8 GiB: bulk-sample probe found only {} of {} keys (page-recovery integrity failure)",
        m.samples_ok, BULK_SAMPLE_COUNT
    );

    // Lower-bound sanity gate: implausibly fast = the timer didn't
    // include real work (e.g., short-circuited open, wrong file).
    assert!(
        m.wall_ms >= BUDGET_8G_MS_MIN,
        "8 GiB recovery completed in {wall_ms} ms (cache={cache}) — \
         implausibly fast; the timer likely did not include real work \
         (RedbBackend::open short-circuited, or temp dir was wiped between child and parent)",
        wall_ms = m.wall_ms,
        cache = m.cache_mode.as_str(),
    );

    // ADR 0002 §W8 budget. Exceeding triggers engine swap per §5
    // Tier-1 trigger #4. Cache-mode is in the message so reviewers
    // know whether the failure is cold-cache (the load-bearing
    // signal) or warm-cache (an optimistic floor that still
    // exceeded the budget — even worse).
    assert!(
        m.wall_ms <= BUDGET_8G_MS_MAX,
        "8 GiB recovery exceeded ADR 0002 §W8 budget: {wall_ms} ms > {budget} ms (cache={cache}). \
         Per ADR 0002 §5 Tier-1 trigger #4, this triggers the engine-swap process.",
        wall_ms = m.wall_ms,
        budget = BUDGET_8G_MS_MAX,
        cache = m.cache_mode.as_str(),
    );
}

// --- Line-shape stability test (M8) ---------------------------------

// Locks the `MANGO_RECOVERY_TIME` line shape so a refactor cannot
// silently break `scripts/parse-recovery-time.sh`. NOT `#[ignore]`
// — runs in default `cargo test`.
#[test]
fn format_recovery_line_shape_is_stable() {
    assert_eq!(
        format_recovery_line("8GiB", 12345, CacheMode::Cold, 64),
        "MANGO_RECOVERY_TIME scenario=8GiB wall_ms=12345 cache=cold samples_ok=64"
    );
    assert_eq!(
        format_recovery_line("1GiB", 50, CacheMode::Warm, 64),
        "MANGO_RECOVERY_TIME scenario=1GiB wall_ms=50 cache=warm samples_ok=64"
    );
    // Skip line shape:
    assert_eq!(
        format_skip_line("8GiB", 1024, 21_474_836_480),
        "MANGO_RECOVERY_TIME scenario=8GiB skipped=insufficient_disk free_bytes=1024 required_bytes=21474836480"
    );
}

#[test]
fn parse_value_index_round_trips() {
    for i in [0u64, 1, 100, 1_000_000, u64::MAX / 2] {
        let v = make_value(i);
        assert_eq!(parse_value_index(&v), Some(i), "round-trip for {i}");
    }
}

#[test]
fn child_watchdog_scales_with_size() {
    assert_eq!(child_watchdog(SCENARIO_1G), Duration::from_secs(90 + 60));
    assert_eq!(child_watchdog(SCENARIO_4G), Duration::from_secs(360 + 60));
    assert_eq!(child_watchdog(SCENARIO_8G), Duration::from_secs(720 + 60));
}

// Pins the `.max(1)` floor inside `child_watchdog`. A zero or
// sub-GiB total should still get a valid (non-zero) watchdog so a
// fat-fingered call site does not produce a Duration::ZERO that
// trips the timeout immediately.
#[test]
fn child_watchdog_has_one_gib_floor() {
    assert_eq!(child_watchdog(0), Duration::from_secs(90 + 60));
    assert_eq!(child_watchdog(1), Duration::from_secs(90 + 60));
}
