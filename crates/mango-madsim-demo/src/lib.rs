//! Canonical `madsim` demo — a tiny async primitive exercising
//! `tokio::sync::mpsc`, `tokio::spawn`, and `tokio::time::sleep`.
//!
//! The exact same source compiles under both the default build
//! (real `tokio`, surfaced via the workspace `package =
//! "madsim-tokio"` rename) and under `RUSTFLAGS="--cfg madsim"`
//! (simulated runtime). This file MUST NOT contain any
//! `#[cfg(madsim)]` gates — the whole point of the scaffold is
//! that library code is unchanged; sim-only scaffolding lives in
//! tests.
//!
//! See [`docs/madsim.md`](../../../docs/madsim.md) for the full
//! policy.

#![deny(missing_docs)]

use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

/// Runs a producer-consumer pair where the producer sends `n`
/// sequential indices spaced 1ms apart and the consumer collects
/// them. Returns the collected values in arrival order.
///
/// Under `RUSTFLAGS="--cfg madsim"`, `sleep` and `spawn` are
/// driven by the deterministic simulator — the output is
/// reproducible from a fixed seed. Under the default build, the
/// function runs on real tokio.
pub async fn producer_consumer(n: usize) -> Vec<usize> {
    let (tx, mut rx) = mpsc::channel::<usize>(16);
    let producer = tokio::spawn(async move {
        for i in 0..n {
            sleep(Duration::from_millis(1)).await;
            if tx.send(i).await.is_err() {
                break;
            }
        }
    });
    let mut seen = Vec::with_capacity(n);
    while let Some(v) = rx.recv().await {
        seen.push(v);
    }
    let _ = producer.await;
    seen
}
