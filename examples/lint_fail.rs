//! Workflows the durable-driver lint must reject. Build with
//! `driver/lint-check.sh`; a plain `cargo build --example lint_fail` passes.

use durable_poc::*;

/// Borrow of a local held across a slow await.
async fn borrow(id: u64, body: [u8; 64]) -> u64 {
    let s = &body[..];
    let a = slow(id).await;
    s.len() as u64 + a
}

/// Hidden temporary: the iterator of a for loop.
async fn hidden(ids: Vec<u64>) -> u64 {
    let mut t = 0;
    for id in ids.iter() {
        t += slow(*id).await;
    }
    t
}

/// A type without serde impls.
struct Opaque(u64);
async fn opaque(id: u64) -> u64 {
    let o = Opaque(1);
    slow(id).await + o.0
}

/// Slow call inside an ephemeral section: the section's borrow is live.
async fn slow_in_ephemeral(id: u64, body: [u8; 64]) -> u64 {
    ephemeral(async {
        let s = &body[..];
        slow(id).await + s.len() as u64
    })
    .await
}

/// Fine: heap data and plain values only.
async fn ok(id: u64) -> u64 {
    let v = vec![String::from("x")];
    slow(id).await + v.len() as u64
}

fn main() {
    let _ = (borrow(1, [0; 64]), hidden(vec![]), opaque(1), slow_in_ephemeral(1, [0; 64]), ok(1));
}
