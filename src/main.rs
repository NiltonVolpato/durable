//! `poc start <classify|leaky|racy> <req_id>` / `poc resume`

use durable_poc::*;
use std::future::Future;
use std::pin::Pin;

const SNAP: &str = "snap.bin";

fn body_for(id: u64) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[0] = id as u8;
    b
}

fn sum(s: &[u8]) -> u64 {
    s.iter().map(|&b| b as u64).sum()
}

/// The design's workflow, plus a nested async fn with its own slow call.
async fn classify(req_id: u64, body: [u8; 64]) -> u64 {
    let quick = ephemeral(async {
        let s = &body[..]; // borrow, allowed inside ephemeral
        yield_once().await; // a non-slow await
        sum(s)
    })
    .await;
    if quick % 2 == 0 {
        return quick;
    }
    let a = slow(req_id).await;
    let b = second_opinion(req_id + 1000).await;
    quick + a + b
}

async fn second_opinion(id: u64) -> u64 {
    let weights = [1u64; 4]; // plain data held across the slow await
    slow(id).await * weights[0]
}

/// Negative control: borrow of the frame across a slow await, forced past
/// the check with ForceDurable. Same answer as classify's first step if sound.
async fn leaky(req_id: u64, body: [u8; 64]) -> u64 {
    let s = &body[..];
    let a = slow(req_id).await;
    sum(s) + a
}

/// Negative control for Option A: a slow leaf and an ephemeral section in a
/// join. The leaf parks before the section starts, so the leaf's assert passes.
async fn racy(req_id: u64, body: [u8; 64]) -> u64 {
    let (a, quick) = join2(
        slow(req_id),
        ephemeral(async {
            let s = &body[..];
            yield_once().await;
            sum(s)
        }),
    )
    .await;
    a + quick
}

fn drive<F: Future<Output = u64> + Durable>(fut: Pin<Box<F>>, tag: u32) {
    match run(fut, tag, SNAP) {
        Outcome::Done(v) => {
            println!("done: {v}");
            let _ = std::fs::remove_file(SNAP);
        }
        Outcome::Parked { event } => println!("parked on event {event}; wrote {SNAP}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("start") => {
            let id: u64 = args[3].parse().unwrap();
            let body = body_for(id);
            match args[2].as_str() {
                "classify" => drive(Box::pin(classify(id, body)), 1),
                "leaky" => drive(Box::pin(ForceDurable(leaky(id, body))), 2),
                "racy" => drive(Box::pin(racy(id, body)), 3),
                w => panic!("unknown workflow {w}"),
            }
        }
        Some("resume") => {
            let (tag, bytes) = read_snapshot(SNAP);
            let b = body_for(0);
            match tag {
                1 => drive(resume(|| classify(0, b), &bytes), 1),
                2 => drive(resume(|| ForceDurable(leaky(0, b)), &bytes), 2),
                3 => drive(resume(|| racy(0, b), &bytes), 3),
                t => panic!("unknown tag {t}"),
            }
        }
        _ => eprintln!("usage: poc start <classify|leaky|racy> <id> | poc resume"),
    }
}
