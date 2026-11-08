//! Rejected by durable-driver at compile time: values live at a slow await
//! that cannot be stored.

use durable::request;
use std::sync::Arc;

mod private {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Secret(u64);

    /// A type private to this module, held across a slow await.
    #[durable::workflow]
    pub(super) async fn private_type(id: u64) -> u64 {
        let s = Secret(id);
        let v: u64 = durable::request("a", id).await;
        v + s.0
    }
}

#[durable::workflow]
async fn borrow_across(id: u64) -> u64 {
    let name = format!("req-{id}");
    let r: &str = &name;
    let v: u64 = request("a", id).await;
    v + r.len() as u64
}

#[durable::workflow]
async fn tokio_join(id: u64) -> u64 {
    let (a, b) = tokio::join!(request::<u64>("a", id), request::<u64>("b", id));
    a + b
}

#[durable::workflow]
async fn tokio_timeout(id: u64) -> u64 {
    tokio::time::timeout(std::time::Duration::from_secs(1), request::<u64>("a", id)).await.unwrap_or(0)
}

async fn step(id: u64) -> u64 {
    request("a", id).await
}

#[durable::workflow]
async fn option_of_future(id: u64) -> u64 {
    let later = Some(step(id));
    let v = step(id + 1).await;
    v + later.unwrap().await
}

#[durable::workflow]
async fn arc_state(id: u64) -> u64 {
    let shared = Arc::new(id);
    let v: u64 = request("a", id).await;
    v + *shared
}

/// Serializable, but its destructor has a side effect: it would run at park
/// and again at the end of the scope on resume.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Lease {
    id: u64,
}

impl Drop for Lease {
    fn drop(&mut self) {
        println!("release {}", self.id);
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Job {
    name: String,
    leases: Vec<Lease>,
}

#[durable::workflow]
async fn lease_across(id: u64) -> u64 {
    let lease = Lease { id };
    let v: u64 = request("a", id).await;
    v + lease.id
}

/// The `Drop` impl is nested: a field, inside a `Vec`.
#[durable::workflow]
async fn nested_lease(id: u64) -> u64 {
    let job = Job { name: "j".into(), leases: vec![Lease { id }] };
    let v: u64 = request("a", id).await;
    v + job.name.len() as u64
}

/// Captured by a closure live across the await.
#[durable::workflow]
async fn closure_lease(id: u64) -> u64 {
    let lease = Lease { id };
    let f = move || consume_now(lease);
    let v: u64 = request("a", id).await;
    v + f()
}

fn consume_now(l: Lease) -> u64 {
    l.id
}

async fn consume(l: Lease) -> u64 {
    l.id
}

/// Held by a future that has not started yet.
#[durable::workflow]
async fn unstarted_lease(id: u64) -> u64 {
    let later = consume(Lease { id });
    let v: u64 = request("a", id).await;
    v + later.await
}

durable::glue!();
fn main() {
    let _ = (private::private_type(1), borrow_across(1), tokio_join(1), tokio_timeout(1), option_of_future(1), arc_state(1));
    let _ = (lease_across(1), nested_lease(1), closure_lease(1), unstarted_lease(1));
}
