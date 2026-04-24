//! Sim-only compile-check for [`mango_storage::RaftEngineLogStore`],
//! compiled (but NOT run) under `RUSTFLAGS="--cfg madsim"`.
//!
//! **This is a compile-check only, and deliberately has no
//! `#[madsim::test]` — the sim cannot execute this crate.**
//!
//! Why: `raft_engine::Engine::open` spawns its own `std::thread`
//! background purge/rewrite workers before returning. Under
//! `--cfg madsim`, madsim's runtime intercepts `std::thread::spawn`
//! and panics ("attempt to spawn a system thread in simulation") so
//! the engine cannot be opened inside the simulator at all — never
//! mind the further `tokio::task::spawn_blocking` hops our async
//! methods rely on. The filesystem layer (`fs2::FileExt::lock_exclusive`,
//! `fdatasync`) is also not substituted by madsim.
//!
//! What this file DOES buy us: the crate and its public surface
//! monomorphize against madsim-tokio, catching any accidental
//! breakage of the `#![cfg(not(madsim))]` gates or the tokio
//! re-export path. The full async semantics live in
//! `tests/raft_engine_logstore.rs` and
//! `tests/raft_engine_crash_recovery.rs` under the default
//! (non-madsim) build.

#![cfg(madsim)]

use mango_storage::{RaftEngineConfig, RaftEngineLogStore};

/// Compile-only reference. Never called at runtime. The function
/// signature forces monomorphization of [`RaftEngineLogStore::open`]
/// against madsim-tokio so a future refactor that breaks the
/// simulator build fails here.
#[allow(dead_code)]
fn _compile_check(cfg: RaftEngineConfig) -> Option<RaftEngineLogStore> {
    RaftEngineLogStore::open(cfg).ok()
}
