//! Sim-only smoke test — compiled and executed only under
//! `RUSTFLAGS="--cfg madsim"`, a no-op otherwise.
//!
//! Scope: proves that `mango-storage` *compiles* against the
//! simulator's `tokio` substitute (madsim-tokio) and that the
//! synchronous registry path runs deterministically inside a
//! `#[madsim::test]` runtime. We intentionally do NOT drive the
//! async commit path here — madsim's runtime does not substitute
//! `std::fs` or the memory-mapped I/O redb uses, and the real
//! commit path runs `tokio::task::spawn_blocking` which madsim-
//! tokio implements as a plain-thread shim. The full redb
//! semantics are covered by `tests/redb_backend.rs` under the
//! default (non-madsim) build.

#![cfg(madsim)]

use mango_storage::{Backend, BackendConfig, BucketId, RedbBackend};
use tempfile::TempDir;

const KV: BucketId = BucketId::new(1);

#[madsim::test]
async fn open_register_close_roundtrips_under_madsim() {
    let tmp = TempDir::new().expect("tempdir");
    let b = RedbBackend::open(BackendConfig::new(tmp.path().to_path_buf(), false)).expect("open");
    b.register_bucket("kv", KV).expect("register_bucket");
    assert!(b.size_on_disk().expect("size_on_disk") > 0);
    b.close().expect("close");
    // Re-opening must see the registered bucket — proves hydration
    // still works under the simulator's deterministic scheduler.
    let b2 =
        RedbBackend::open(BackendConfig::new(tmp.path().to_path_buf(), false)).expect("reopen");
    // Idempotent re-registration succeeds; name-rebinding to a fresh
    // id fails with `BucketNameConflict`, proving the registry was
    // hydrated from disk.
    b2.register_bucket("kv", KV).expect("idempotent");
    let err = b2
        .register_bucket("kv", BucketId::new(99))
        .expect_err("name conflict");
    assert!(matches!(
        err,
        mango_storage::BackendError::BucketNameConflict { .. }
    ));
    b2.close().expect("close");
}
