//! Durable async workflows (alpha).
//!
//! A `#[durable::workflow]` is a plain `async fn`. When it awaits a slow call
//! (`request`, `sleep_until`), the runtime serializes its state, stores it,
//! and drops it from memory. When the result arrives, possibly in another
//! process, the runtime rebuilds the state and continues the function.
//! See `docs/alpha.md` for what is supported.

pub use durable_macros::{glue, workflow};

mod combinators;
pub mod glue;
mod leaves;
mod runtime;
mod task;

pub use combinators::{Either, Join2, Select2, Structural, join2, select2};
pub use leaves::{Request, SleepUntil, SlowLeaf, now_ms, request, sleep_until};
pub use runtime::{Error, Outcome, Outgoing, Runtime, Status, Workflow};

#[doc(hidden)]
pub mod __private {
    /// Marks a workflow root for durable-driver (see `#[durable::workflow]`).
    pub struct Root<F>(pub F);
}
