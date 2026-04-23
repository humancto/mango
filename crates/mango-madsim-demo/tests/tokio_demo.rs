//! Real-runtime test — compiled and executed only under the
//! default build; a no-op under `--cfg madsim`.
//!
//! Proves that the same library source compiles and runs under
//! real tokio via the `madsim-tokio` re-export (the package
//! rename is link-time only; absent `--cfg madsim`, the renamed
//! crate is a thin passthrough).

#![cfg(not(madsim))]

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn producer_consumer_runs_on_real_tokio() {
    let out = mango_madsim_demo::producer_consumer(5).await;
    assert_eq!(out, vec![0, 1, 2, 3, 4]);
}
