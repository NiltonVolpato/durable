//! `poc start <classify|triage|leaky|racy> <req_id>` / `poc resume`

use durable_poc::*;
use std::future::Future;

const SNAP: &str = "snap.bin";
const ARENA_CAP: usize = 64 * 1024;

fn body_for(id: u64) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[0] = id as u8;
    b
}

fn sum(s: &[u8]) -> u64 {
    s.iter().map(|&b| b as u64).sum()
}

/// v0 workflow, plus a nested async fn with its own slow call.
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

/// v1 workflow: arena data held across slow awaits.
struct Note {
    weight: u64,
}

async fn triage(req_id: u64) -> u64 {
    let mut history: DVec<u64> = DVec::new();
    for i in 1..=32 {
        history.push(i); // grows 4 -> 8 -> 16 -> 32, now full
    }
    let note = ABox::new(Note { weight: 3 });
    // RelRef elements are !Unpin, so this DVec is fixed-capacity.
    let mut refs: DVec<RelRef<Note>> = DVec::with_capacity(1);
    refs.push_pinned(RelRef::null());
    refs.get_pin(0).set(note.get());
    println!("  note @ {:p}", refs.get(0).get().unwrap());

    let a = slow(req_id).await;

    history.push(a); // full: grows to 64 after resume
    println!("  note @ {:p} (via RelRef)", refs.get(0).get().unwrap());
    let w = refs.get(0).get().unwrap().weight;

    let b = slow(req_id + 1000).await;

    history.as_slice().iter().sum::<u64>() + w * b
}

/// Negative control: borrow of the frame across a slow await, forced past
/// the check with ForceDurable. Correct answer for id 7, result 40: 47.
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

fn drive<F: Future<Output = u64> + Durable>(task: Task<F>, tag: u32) {
    match run(task, tag, SNAP) {
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
                "classify" => drive(Task::new(id, ARENA_CAP, classify(id, body)), 1),
                "triage" => drive(Task::new(id, ARENA_CAP, triage(id)), 2),
                "leaky" => drive(Task::new(id, ARENA_CAP, ForceDurable(leaky(id, body))), 3),
                "racy" => drive(Task::new(id, ARENA_CAP, racy(id, body)), 4),
                w => panic!("unknown workflow {w}"),
            }
        }
        Some("resume") => {
            let (tag, snap) = read_snapshot(SNAP);
            let b = body_for(0);
            match tag {
                1 => drive(Task::resume(|| classify(0, b), &snap), 1),
                2 => drive(Task::resume(|| triage(0), &snap), 2),
                3 => drive(Task::resume(|| ForceDurable(leaky(0, b)), &snap), 3),
                4 => drive(Task::resume(|| racy(0, b), &snap), 4),
                t => panic!("unknown tag {t}"),
            }
        }
        _ => eprintln!("usage: poc start <classify|triage|leaky|racy> <id> | poc resume"),
    }
}
