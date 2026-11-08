//! In-process evict/resume round trips. Also meant to run under Miri:
//! `MIRIFLAGS=-Zmiri-disable-isolation cargo miri test --test roundtrip`

use durable_poc::*;
use std::future::Future;
use std::task::{Context, Poll, Waker};

struct Note {
    weight: u64,
}

async fn triage(req_id: u64) -> u64 {
    let mut history: DVec<u64> = DVec::new();
    for i in 1..=32 {
        history.push(i);
    }
    let note = ABox::new(Note { weight: 3 });
    let mut refs: DVec<RelRef<Note>> = DVec::with_capacity(1);
    refs.push_pinned(RelRef::null());
    refs.get_pin(0).set(note.get());
    let a = slow(req_id).await;
    history.push(a);
    let w = refs.get(0).get().unwrap().weight;
    let b = slow(req_id + 1000).await;
    history.as_slice().iter().sum::<u64>() + w * b
}

async fn classify(req_id: u64, body: [u8; 64]) -> u64 {
    let quick = ephemeral(async {
        let s = &body[..];
        yield_once().await;
        s.iter().map(|&b| b as u64).sum::<u64>()
    })
    .await;
    let a = slow(req_id).await;
    quick + a
}

/// Poll until Ready or parked; on park, evict and return the snapshot.
fn step<F: Future + Durable>(mut t: Task<F>) -> Result<F::Output, Snapshot> {
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        match t.poll(&mut cx) {
            Poll::Ready(v) => return Ok(v),
            Poll::Pending if t.parked().is_some() => {
                assert_eq!(t.ephemeral(), 0);
                return Err(t.evict());
            }
            Poll::Pending => {}
        }
    }
}

/// Re-copy the bytes so the resumed arena never shares an allocation with
/// the evicted one, and scribble over the old copy.
fn reload(mut s: Snapshot) -> Snapshot {
    let bytes = s.bytes.clone();
    s.bytes.fill(0xAA);
    Snapshot { bytes, ..s }
}

#[test]
fn roundtrips() {
    let dir = std::env::temp_dir().join(format!("durable-poc-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("results")).unwrap();
    std::env::set_current_dir(&dir).unwrap();

    // triage: two slow points, arena data, RelRef, DVec regrow after resume.
    let s = step(Task::new(7, 16 * 1024, triage(7))).err().unwrap();
    std::fs::write("results/7", "40").unwrap();
    let s = step(Task::resume(|| triage(0), &reload(s))).err().unwrap();
    std::fs::write("results/1007", "2").unwrap();
    let v = step(Task::resume(|| triage(0), &reload(s))).ok().unwrap();
    assert_eq!(v, 528 + 40 + 3 * 2);

    // classify: ephemeral section with a borrow, then a slow point.
    let mut body = [0u8; 64];
    body[0] = 9;
    let s = step(Task::new(9, 16 * 1024, classify(9, body))).err().unwrap();
    std::fs::write("results/9", "1").unwrap();
    let v = step(Task::resume(|| classify(0, [0; 64]), &reload(s))).ok().unwrap();
    assert_eq!(v, 10);

    // Dropping a live (never evicted) task runs its drop glue in context.
    drop(Task::new(11, 16 * 1024, classify(11, body)));

    std::fs::remove_dir_all(&dir).unwrap();
}
