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

durable::glue!();
fn main() {
    let _ = (private::private_type(1), borrow_across(1), tokio_join(1), tokio_timeout(1), option_of_future(1), arc_state(1));
}
