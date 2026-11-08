//! Runtime behavior end to end. Needs the generated glue, so it only exists
//! in a durable build: `cargo durable test -p durable --test runtime`.
#![cfg(durable)]

use durable::{Either, Error, Outcome, Outgoing, Runtime, Status, request, select2, sleep_until};
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

durable::glue!();

#[durable::workflow]
async fn fast(x: u64) -> u64 {
    x + 1
}

#[durable::workflow]
async fn one(x: u64) -> u64 {
    let a: u64 = request("a", x).await;
    a + x
}

#[durable::workflow]
async fn deadline(at_ms: u64) -> String {
    match select2(request::<u64>("slow", ()), sleep_until(at_ms)).await {
        Either::Left(v) => format!("got {v}"),
        Either::Right(()) => "timeout".into(),
    }
}

#[durable::workflow]
async fn boom(x: u64) -> u64 {
    let a: u64 = request("a", x).await;
    assert!(a != 0, "zero");
    a
}

/// The slow call is behind `dyn Future`: the lint cannot see it.
#[durable::workflow]
async fn hidden(x: u64) -> u64 {
    let f: Pin<Box<dyn Future<Output = u64>>> = Box::pin(request("a", x));
    f.await
}

#[durable::workflow]
async fn two_steps(x: u64) -> u64 {
    let a: u64 = request("double", x).await;
    let b: u64 = request("double", a).await;
    b
}

thread_local! {
    /// (lease id, `is_parking()`) for each drop.
    static RELEASED: RefCell<Vec<(u64, bool)>> = const { RefCell::new(Vec::new()) };
}

/// A value whose destructor has a side effect. Without the opt-in,
/// durable-driver rejects it at a slow await (see apps/unsupported).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Lease {
    id: u64,
}

impl Drop for Lease {
    fn drop(&mut self) {
        RELEASED.with(|r| r.borrow_mut().push((self.id, durable::is_parking())));
    }
}

impl durable::DropOnPark for Lease {}

#[durable::workflow]
async fn leased(id: u64) -> u64 {
    let lease = Lease { id };
    let v: u64 = request("a", id).await;
    v + lease.id
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Config {
    bonus: u64,
}

/// Immutable shared values: a copy on resume behaves the same, so allowed.
#[durable::workflow]
async fn shared_config(x: u64) -> String {
    let cfg = std::sync::Arc::new(Config { bonus: 100 });
    let name: std::sync::Arc<str> = "n".into();
    let a: u64 = request("a", x).await;
    format!("{name}{}", a + cfg.bonus)
}

type Log = Rc<RefCell<Vec<Outgoing>>>;

fn db(name: &str) -> String {
    let p = std::env::temp_dir().join(format!("durable-test-{}-{name}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p.to_string_lossy().into_owned()
}

fn runtime(path: &str) -> (Runtime, Log) {
    let log: Log = Rc::default();
    let l = log.clone();
    let mut rt = Runtime::open(path, GLUE, move |o| l.borrow_mut().push(o.clone())).unwrap();
    rt.register(fast).unwrap();
    rt.register(one).unwrap();
    rt.register(deadline).unwrap();
    rt.register(boom).unwrap();
    rt.register(hidden).unwrap();
    rt.register(two_steps).unwrap();
    rt.register(leased).unwrap();
    rt.register(shared_config).unwrap();
    (rt, log)
}

fn sql(path: &str, q: &str) {
    rusqlite::Connection::open(path).unwrap().execute_batch(q).unwrap();
}

#[tokio::test]
async fn fast_path_stores_nothing() {
    let path = db("fast");
    let (rt, log) = runtime(&path);
    assert!(matches!(rt.start("t", fast(1)).await.unwrap(), Outcome::Done(2)));
    assert!(matches!(rt.status("t"), Err(Error::UnknownTask(_))));
    assert!(log.borrow().is_empty());
}

#[tokio::test]
async fn park_restart_deliver_duplicate() {
    let path = db("park");
    {
        let (rt, log) = runtime(&path);
        assert!(matches!(rt.start("t", one(5)).await.unwrap(), Outcome::Parked));
        let sent = log.borrow();
        assert_eq!((sent.len(), sent[0].kind.as_str(), sent[0].payload.clone()), (1, "a", serde_json::json!(5)));
    } // "process exits"
    let (rt, _) = runtime(&path);
    assert_eq!(rt.deliver("t", 0, 10u64).await.unwrap(), Status::Done(serde_json::json!(15)));
    assert_eq!(rt.deliver("t", 0, 99u64).await.unwrap(), Status::Done(serde_json::json!(15)));
    assert_eq!(rt.output::<u64>("t").unwrap(), Some(15));
    // Same id again: runs, parks, and is refused before anything is sent.
    assert!(matches!(rt.start("t", one(5)).await, Err(Error::TaskExists(_))));
}

#[tokio::test]
async fn deadline_cancels_the_request() {
    let path = db("deadline");
    let (rt, log) = runtime(&path);
    let at = durable::now_ms() + 50;
    assert!(matches!(rt.start("t", deadline(at)).await.unwrap(), Outcome::Parked));
    assert_eq!(log.borrow().len(), 1);
    assert!(matches!(rt.status("t").unwrap(), Status::Parked { wake_at: Some(w), .. } if w == at));
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let ran = rt.run_due_timers().await.unwrap();
    assert_eq!(ran.len(), 1);
    assert_eq!(rt.status("t").unwrap(), Status::Done(serde_json::json!("timeout")));
    assert!(rt.open_requests().unwrap().is_empty());
    // A late result for the abandoned request is ignored.
    assert_eq!(rt.deliver("t", 0, 1u64).await.unwrap(), Status::Done(serde_json::json!("timeout")));
}

#[tokio::test]
async fn panic_fails_the_task() {
    let path = db("panic");
    let (rt, _) = runtime(&path);
    rt.start("t", boom(1)).await.unwrap();
    assert!(matches!(rt.deliver("t", 0, 0u64).await, Err(Error::Panicked(m)) if m.contains("zero")));
    assert!(matches!(rt.status("t").unwrap(), Status::Failed(_)));
}

#[tokio::test]
async fn hidden_slow_call_is_a_clear_error() {
    let path = db("hidden");
    let (rt, log) = runtime(&path);
    let err = rt.start("t", hidden(1)).await.unwrap_err();
    assert!(matches!(&err, Error::NotSerializable(m) if m.contains("trait object")), "{err}");
    assert!(log.borrow().is_empty(), "nothing is dispatched for a task that cannot park");
}

#[tokio::test]
async fn incompatible_snapshot_stays_parked() {
    let path = db("incompatible");
    let (rt, _) = runtime(&path);
    rt.start("t", one(5)).await.unwrap();
    // As if the parked state were written by a build with different locals.
    sql(&path, r#"UPDATE tasks SET frame = replace(frame, '"sig":"', '"sig":"old,')"#);
    assert!(matches!(rt.deliver("t", 0, 10u64).await, Err(Error::Incompatible(_))));
    assert!(matches!(rt.status("t").unwrap(), Status::Parked { error: Some(_), .. }));
    // A compatible build can resume it later; the result was kept.
    sql(&path, r#"UPDATE tasks SET frame = replace(frame, '"sig":"old,', '"sig":"')"#);
    assert_eq!(rt.resume("t").await.unwrap(), Status::Done(serde_json::json!(15)));
}

#[tokio::test]
async fn redispatch_after_crash() {
    let path = db("redispatch");
    let (rt, _) = runtime(&path);
    rt.start("t", one(5)).await.unwrap();
    // As if the process died between storing the task and sending its request.
    sql(&path, "UPDATE requests SET dispatched = 0");
    let (rt2, log2) = runtime(&path);
    assert_eq!(rt2.redispatch().unwrap(), 1);
    assert_eq!(log2.borrow().len(), 1);
    assert_eq!(rt2.redispatch().unwrap(), 0);
    drop(rt);
}

#[tokio::test]
async fn missing_glue_is_reported_at_register() {
    let mut rt = Runtime::open(&db("noglue"), &[], |_| {}).unwrap();
    let err = rt.register(one).unwrap_err();
    assert!(err.to_string().contains("cargo durable build"), "{err}");
}

#[tokio::test]
async fn handlers_run_in_process_and_retry() {
    let path = db("handlers");
    let (mut rt, log) = runtime(&path);
    let calls = Rc::new(RefCell::new(0));
    let c = calls.clone();
    rt.handle("double", move |o| {
        let c = c.clone();
        async move {
            *c.borrow_mut() += 1;
            if *c.borrow() == 2 {
                return Err("flaky".to_string()); // the second call fails once
            }
            Ok(o.payload.as_u64().unwrap() * 2)
        }
    });
    assert!(matches!(rt.start("t", two_steps(3)).await.unwrap(), Outcome::Parked));
    assert!(log.borrow().is_empty(), "handled kinds are not dispatched");
    rt.work().await.unwrap(); // step 1 ok, step 2 fails and stays pending
    assert!(matches!(rt.status("t").unwrap(), Status::Parked { ref waiting, .. } if waiting == &[1]));
    rt.work().await.unwrap(); // step 2 retried
    assert_eq!(rt.status("t").unwrap(), Status::Done(serde_json::json!(12)));
    assert_eq!(*calls.borrow(), 3);
}

/// What `DropOnPark` accepts: the destructor runs when the task parks, with
/// `is_parking()` true, and again on the rebuilt copy at the real end.
#[tokio::test]
async fn opted_in_destructor_sees_parking() {
    let path = db("lease");
    let (rt, _) = runtime(&path);
    let released = || RELEASED.with(|r| r.borrow_mut().drain(..).collect::<Vec<_>>());
    released();
    assert!(matches!(rt.start("t", leased(7)).await.unwrap(), Outcome::Parked));
    assert_eq!(released(), vec![(7, true)]);
    // Refused at store: the task never existed, so that drop is a real end.
    assert!(matches!(rt.start("t", leased(9)).await, Err(Error::TaskExists(_))));
    assert_eq!(released(), vec![(9, false)]);
    assert_eq!(rt.deliver("t", 0, 1u64).await.unwrap(), Status::Done(serde_json::json!(8)));
    assert_eq!(released(), vec![(7, false)]);
    assert!(!durable::is_parking());
}

#[tokio::test]
async fn immutable_arc_survives_park() {
    let path = db("arc");
    let (rt, _) = runtime(&path);
    assert!(matches!(rt.start("t", shared_config(1)).await.unwrap(), Outcome::Parked));
    assert_eq!(rt.deliver("t", 0, 2u64).await.unwrap(), Status::Done(serde_json::json!("n102")));
}
