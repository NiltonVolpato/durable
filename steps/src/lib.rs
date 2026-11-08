//! A small "SDK" of workflow steps in a separate crate.

use durable_poc::slow;
use std::future::Future;

/// Wait for a human review; the result is the review score.
pub async fn wait_for_review(id: u64) -> u64 {
    let requested_at: u64 = id * 10; // plain state held across the slow call
    slow(id).await + requested_at - id * 10
}

/// A check that may come back empty (result 0).
pub async fn check(id: u64) -> Option<u64> {
    let v = slow(id).await;
    (v > 0).then_some(v)
}

/// Generic helper: run `f()` until it returns Some, at most `times` times.
pub async fn retry<F, Fut>(times: u32, f: F) -> u64
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = Option<u64>>,
{
    for attempt in 0..times {
        if let Some(v) = f(attempt).await {
            return v;
        }
    }
    0
}
