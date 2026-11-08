//! Slow leaves: the only places where a task parks.

use crate::task::{NewRequest, try_with_ctx, with_ctx};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Marks leaf futures that can park a task. durable-driver treats any await
/// that can reach one as a slow point.
pub trait SlowLeaf {}

/// An external call whose result is delivered later with `Runtime::deliver`.
///
/// On first poll it gets a request id and is queued for dispatch; the request
/// is sent only after the parked task is stored.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(bound = "")]
pub struct Request<T> {
    kind: String,
    payload: Value,
    id: Option<u64>,
    /// Result consumed: the request is over.
    #[serde(skip)]
    done: bool,
    #[serde(skip)]
    _t: PhantomData<fn() -> T>,
}

pub fn request<T: DeserializeOwned>(kind: &str, payload: impl Serialize) -> Request<T> {
    let payload = serde_json::to_value(payload).expect("request payload must serialize to JSON");
    Request { kind: kind.to_string(), payload, id: None, done: false, _t: PhantomData }
}

impl<T> SlowLeaf for Request<T> {}

impl<T: DeserializeOwned> Future for Request<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, _: &mut Context) -> Poll<T> {
        // Request has no self-references and no pinned fields.
        let this = unsafe { self.get_unchecked_mut() };
        with_ctx(|c| {
            let id = *this.id.get_or_insert_with(|| {
                let id = c.next_req;
                c.next_req += 1;
                c.outbox.push(NewRequest { req_id: id, kind: this.kind.clone(), payload: this.payload.clone() });
                id
            });
            match c.inbox.remove(&id) {
                Some(v) => {
                    c.consumed.push(id);
                    this.done = true;
                    match serde_json::from_value(v) {
                        Ok(t) => Poll::Ready(t),
                        Err(e) => panic!("result of request {id} ({}) does not match its type: {e}", this.kind),
                    }
                }
                None => {
                    c.parked = true;
                    Poll::Pending
                }
            }
        })
    }
}

/// Dropped during a poll before its result arrived (e.g. the losing side of
/// `select2`): the request is cancelled. Dropped outside a poll, the task is
/// only being parked, which is not a cancellation.
impl<T> Drop for Request<T> {
    fn drop(&mut self) {
        if let (Some(id), false) = (self.id, self.done) {
            try_with_ctx(|c| {
                c.outbox.retain(|o| o.req_id != id); // created and dropped in the same slice
                c.cancelled.push(id);
            });
        }
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

/// Durable timer: ready once the wall clock reaches `deadline_ms`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SleepUntil {
    deadline_ms: u64,
}

pub fn sleep_until(deadline_ms: u64) -> SleepUntil {
    SleepUntil { deadline_ms }
}

impl SlowLeaf for SleepUntil {}

impl Future for SleepUntil {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _: &mut Context) -> Poll<()> {
        if now_ms() >= self.deadline_ms {
            return Poll::Ready(());
        }
        with_ctx(|c| {
            c.wake_at = Some(c.wake_at.map_or(self.deadline_ms, |w| w.min(self.deadline_ms)));
            c.parked = true;
        });
        Poll::Pending
    }
}
