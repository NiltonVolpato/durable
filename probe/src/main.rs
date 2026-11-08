//! Realistic workflow on tokio, to find what the lint and glue cannot handle.
//! `probe start <id>` / `probe resume`; slow results go in `results/<id>`.

#![feature(register_tool)]
#![register_tool(durable)]

use durable_poc::*;
use durable_steps::{check, retry, wait_for_review};
use std::future::Future;
use std::task::Poll;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

mod durable_glue {
    include!(concat!(env!("OUT_DIR"), "/durable_glue.rs"));
}

const CAP: usize = 64 * 1024;
const SNAP: &str = "probe.json";

/// Fast path: real tokio I/O, borrows and a TcpStream across fast awaits.
async fn fast_score(addr: &str, req_id: u64) -> u64 {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{req_id}\n").as_bytes()).await.unwrap();
    let mut buf = [0u8; 32];
    let n = s.read(&mut buf).await.unwrap();
    std::str::from_utf8(&buf[..n]).unwrap().trim().parse().unwrap()
}

fn audit(note: String) {
    println!("  audit: {note}");
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

#[durable::workflow]
async fn moderate(req_id: u64, addr: String) -> u64 {
    // 1. Fast path on tokio.
    let quick = fast_score(&addr, req_id).await;
    if quick < 10 {
        return quick;
    }
    // 2. Conditional move before a slow await (drop flag).
    let note = format!("req {req_id} scored {quick}");
    if quick % 2 == 0 {
        audit(note);
    }
    // 3. A step from another crate.
    let a = wait_for_review(req_id).await;
    // 4. Two slow calls in parallel.
    let (b, c) = join2(wait_for_review(req_id + 1), wait_for_review(req_id + 2)).await;
    // 5. A slow call with a deadline.
    let d = match select2(wait_for_review(req_id + 3), sleep_until(now() + 2)).await {
        Either::Left(v) => v,
        Either::Right(()) => 0,
    };
    // 6. A generic helper with a closure.
    let e = retry(3, move |attempt| check(req_id + 4 + attempt as u64)).await;
    println!("  quick={quick} a={a} b={b} c={c} d={d} e={e}");
    quick + a + b + c + d + e
}

/// Poll the task on tokio; when it parks on a slow call, serialize and stop.
async fn host<F: Future<Output = u64>>(mut task: Task<F>) {
    let parked = std::future::poll_fn(|cx| match task.poll(cx) {
        Poll::Ready(v) => Poll::Ready(Ok(v)),
        Poll::Pending => match task.parked() {
            None => Poll::Pending, // fast work: tokio wakes us
            Some(ev) => Poll::Ready(Err(ev)),
        },
    })
    .await;
    match parked {
        Ok(v) => {
            println!("done: {v}");
            let _ = std::fs::remove_file(SNAP);
        }
        Err(ev) => match glue::serialize(durable_glue::GLUE, &task) {
            Ok(v) => {
                std::fs::write(SNAP, serde_json::to_string_pretty(&v).unwrap()).unwrap();
                println!("parked on {ev}; wrote {SNAP}");
            }
            Err(e) => {
                eprintln!("parked on {ev} but cannot serialize: {e}");
                std::process::exit(2);
            }
        },
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // A local "fast model": replies with req_id + 10.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 32];
            let n = s.read(&mut buf).await.unwrap();
            let id: u64 = std::str::from_utf8(&buf[..n]).unwrap().trim().parse().unwrap();
            s.write_all(format!("{}\n", id + 10).as_bytes()).await.unwrap();
        }
    });
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("start") => {
            let id: u64 = args[2].parse().unwrap();
            host(Task::new(id, CAP, moderate(id, addr))).await
        }
        Some("resume") => {
            let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(SNAP).unwrap()).unwrap();
            match glue::deserialize(durable_glue::GLUE, || moderate(0, String::new()), CAP, &v) {
                Ok(t) => host(t).await,
                Err(e) => eprintln!("cannot resume: {e}"),
            }
        }
        _ => eprintln!("usage: probe start <id> | probe resume"),
    }
}
