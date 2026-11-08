//! Serialized-frame round trips through glue generated for this test crate.
//! Needs the glue: run `driver/serde-test.sh` (native and Miri). Without it
//! the test is skipped.

use durable_poc::*;
use std::future::Future;
use std::task::{Context, Poll, Waker};

mod durable_glue {
    include!(concat!(env!("OUT_DIR"), "/durable_test_glue.rs"));
}
use durable_glue::GLUE;

const CAP: usize = 16 * 1024;

async fn inner(id: u64) -> u64 {
    let w = [1u64; 4];
    slow(id).await * w[0]
}

async fn profile(id: u64) -> u64 {
    let mut words = vec![format!("req-{id}")];
    let label = String::from("abuse");
    let a = slow(id).await;
    words.push(format!("score-{a}"));
    let b = inner(id + 1000).await;
    (words.iter().map(|w| w.len()).sum::<usize>() + label.len()) as u64 + a + b
}

async fn classify(id: u64, body: [u8; 64]) -> u64 {
    let quick = ephemeral(async {
        let s = &body[..];
        yield_once().await;
        s.iter().map(|&b| b as u64).sum::<u64>()
    })
    .await;
    let a = slow(id).await;
    quick + a
}

/// Poll until Ready, or until parked: then serialize and drop the task.
fn step<F: Future>(mut t: Task<F>) -> Result<F::Output, serde_json::Value> {
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        match t.poll(&mut cx) {
            Poll::Ready(v) => return Ok(v),
            Poll::Pending if t.parked().is_some() => {
                let v = glue::serialize(GLUE, &t).unwrap();
                drop(t);
                return Err(v);
            }
            Poll::Pending => {}
        }
    }
}

#[test]
fn serde_roundtrips() {
    if GLUE.is_empty() {
        eprintln!("skipped: no glue (run driver/serde-test.sh)");
        return;
    }
    println!("glue entries: {}", GLUE.len());
    let dir = std::env::temp_dir().join(format!("durable-serde-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("results")).unwrap();
    std::env::set_current_dir(&dir).unwrap();

    // Heap data and a nested frame across two parks.
    let v = step(Task::new(7, CAP, profile(7))).unwrap_err();
    std::fs::write("results/7", "40").unwrap();
    let v = step(glue::deserialize(GLUE, || profile(0), CAP, &v).unwrap()).unwrap_err();
    std::fs::write("results/1007", "2").unwrap();
    let r = step(glue::deserialize(GLUE, || profile(0), CAP, &v).unwrap()).unwrap();
    assert_eq!(r, 5 + 8 + 5 + 40 + 2);

    // A >32-element array, after an ephemeral section.
    let mut body = [0u8; 64];
    body[0] = 9;
    let v = step(Task::new(9, CAP, classify(9, body))).unwrap_err();
    std::fs::write("results/9", "1").unwrap();
    let r = step(glue::deserialize(GLUE, || classify(0, [0; 64]), CAP, &v).unwrap()).unwrap();
    assert_eq!(r, 10);

    // Drop a rebuilt frame without finishing it: its drop glue must free the heap data.
    let v = step(Task::new(11, CAP, profile(11))).unwrap_err();
    drop(glue::deserialize(GLUE, || profile(0), CAP, &v).unwrap());

    std::fs::remove_dir_all(&dir).unwrap();
}
