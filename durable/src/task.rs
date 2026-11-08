//! Per-task context, visible to leaves during a poll.

use serde_json::Value;
use std::cell::Cell;
use std::collections::HashMap;

/// A request created during a slice, to be stored and then sent.
#[derive(Debug, Clone)]
pub struct NewRequest {
    pub req_id: u64,
    pub kind: String,
    pub payload: Value,
}

/// Runtime state of one task. Saved with its frame.
#[derive(Debug, Default)]
pub struct TaskCtx {
    /// Next request id; ids are per task.
    pub next_req: u64,
    /// Delivered results not yet consumed, by request id.
    pub inbox: HashMap<u64, Value>,
    /// Results consumed during this slice.
    pub consumed: Vec<u64>,
    /// Requests created during this slice.
    pub outbox: Vec<NewRequest>,
    /// Requests abandoned during this slice (dropped before their result).
    pub cancelled: Vec<u64>,
    /// Earliest durable timer, ms since the epoch.
    pub wake_at: Option<u64>,
    /// Set when a slow leaf returned Pending in the current poll.
    pub parked: bool,
}

thread_local! {
    /// The task being polled. Set for one poll at a time.
    static CURRENT: Cell<*mut TaskCtx> = const { Cell::new(std::ptr::null_mut()) };
}

/// Run `f` with `ctx` as the current task.
pub fn enter<R>(ctx: &mut TaskCtx, f: impl FnOnce() -> R) -> R {
    struct Restore(*mut TaskCtx);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.set(self.0);
        }
    }
    let _restore = Restore(CURRENT.replace(ctx));
    f()
}

pub(crate) fn with_ctx<R>(f: impl FnOnce(&mut TaskCtx) -> R) -> R {
    try_with_ctx(f).expect("a durable leaf was awaited outside a durable task (start it with Runtime::start)")
}

/// None outside a poll (e.g. when a parked task's frame is dropped).
pub(crate) fn try_with_ctx<R>(f: impl FnOnce(&mut TaskCtx) -> R) -> Option<R> {
    let p = CURRENT.get();
    (!p.is_null()).then(|| f(unsafe { &mut *p }))
}
