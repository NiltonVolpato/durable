//! Rejected by durable-driver at compile time: values live at a slow await
//! that cannot be stored. A stored value must serialize and have none of:
//!
//! - Borrow: a reference into the frame or elsewhere (lifetimes).
//! - Effect: a destructor with a side effect (`Drop` without `DropOnPark`).
//! - Opaque: a value whose state the driver cannot see or name (`dyn`,
//!   unnameable types, foreign futures without serde impls).
//! - Identity: sharing that a copy loses (`Rc` / `Arc` to mutable data, `Weak`).
//!
//! Each case below is labeled with its class. A new case that fits none of
//! them points at a gap in this model.

use durable::request;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

// ---------- Glue limit (not a class: a restriction of the generated code) ----------

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

// ---------- Borrow ----------

#[durable::workflow]
async fn borrow_across(id: u64) -> u64 {
    let name = format!("req-{id}");
    let r: &str = &name;
    let v: u64 = request("a", id).await;
    v + r.len() as u64
}

// ---------- Opaque ----------

/// `MaybeDone<{async block}>`: an unnameable future inside a tokio type.
#[durable::workflow]
async fn tokio_join(id: u64) -> u64 {
    let (a, b) = tokio::join!(request::<u64>("a", id), request::<u64>("b", id));
    a + b
}

/// A foreign future (and its timer registration) with no serde impl.
#[durable::workflow]
async fn tokio_timeout(id: u64) -> u64 {
    tokio::time::timeout(std::time::Duration::from_secs(1), request::<u64>("a", id)).await.unwrap_or(0)
}

async fn step(id: u64) -> u64 {
    request("a", id).await
}

/// An unstarted `async fn` future inside an `Option`.
#[durable::workflow]
async fn option_of_future(id: u64) -> u64 {
    let later = Some(step(id));
    let v = step(id + 1).await;
    v + later.unwrap().await
}

// ---------- Effect ----------

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

// ---------- Identity ----------
// serde's `rc` feature is on (Cargo.toml), so all of these serialize.
// `Arc<str>` or `Arc<Config>` would be accepted: immutable, a copy is equivalent.

#[durable::workflow]
async fn arc_state(id: u64) -> u64 {
    let counter = Arc::new(Mutex::new(id));
    let v: u64 = request("a", id).await;
    v + *counter.lock().unwrap()
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Shared {
    names: Rc<RefCell<Vec<String>>>,
}

/// Nested in a field of the user's own struct.
#[durable::workflow]
async fn rc_field(id: u64) -> u64 {
    let s = Shared { names: Rc::new(RefCell::new(vec![])) };
    let v: u64 = request("a", id).await;
    v + s.names.borrow().len() as u64
}

/// `Freeze` at the top, but mutable behind the `Vec`.
#[durable::workflow]
async fn arc_vec_cell(id: u64) -> u64 {
    let cells = Arc::new(vec![std::cell::Cell::new(id)]);
    let v: u64 = request("a", id).await;
    v + cells[0].get()
}

/// Immutable pointee, but a stored `Weak` comes back empty.
#[durable::workflow]
async fn weak_ref(id: u64) -> u64 {
    let strong = Arc::new(id);
    let weak = Arc::downgrade(&strong);
    drop(strong);
    let v: u64 = request("a", id).await;
    v + weak.upgrade().map_or(0, |a| *a)
}

durable::glue!();
fn main() {
    let _ = (private::private_type(1), borrow_across(1), tokio_join(1), tokio_timeout(1), option_of_future(1));
    let _ = (lease_across(1), nested_lease(1), closure_lease(1), unstarted_lease(1));
    let _ = (arc_state(1), rc_field(1), arc_vec_cell(1), weak_ref(1));
}
