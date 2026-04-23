//! Sim-only test — compiled and executed only under
//! `RUSTFLAGS="--cfg madsim"`; a no-op otherwise.
//!
//! Proves the library's `producer_consumer` compiles against the
//! simulated `tokio` (the workspace-renamed `madsim-tokio`) and
//! that the output order is deterministic under a fixed seed.

#![cfg(madsim)]

#[madsim::test]
async fn producer_consumer_is_deterministic() {
    let out = mango_madsim_demo::producer_consumer(5).await;
    assert_eq!(out, vec![0, 1, 2, 3, 4]);
}
