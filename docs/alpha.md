# Alpha user guide

`durable` runs a plain `async fn` as a durable workflow. The fast path runs in memory and stores nothing. When the function awaits a slow call (a request answered later, a timer), its state is serialized into SQLite and dropped from memory. When the answer arrives, possibly in another process after a restart, the state is rebuilt and the function continues where it stopped.

The alpha supports a subset of Rust. Code outside the subset gets an error: at compile time where possible, otherwise at startup or when the task parks. How it works: [serialized frames](serialized-frames.md).

## Setup

- Toolchain: `nightly-2026-11-08`, pinned in `rust-toolchain.toml`, with `rustc-dev` and `llvm-tools`. The driver uses rustc internals, so it is tied to that exact nightly; moving to another one means updating the driver.
- Build with `cargo durable build|run|test` (`driver/src/cargo_durable.rs`). It compiles workflow crates through `durable-driver`, which generates the serde glue and checks slow awaits. Install it with `cargo install --path driver` (it installs `cargo-durable` and `durable-driver`). In this repo you can use `target/debug/cargo-durable durable build -p <pkg>`.
- A plain `cargo build` still compiles, but `Runtime::register` then fails: "no glue for workflow ...: build with `cargo durable build`".
- Crates built by `cargo durable` get `cfg(durable)`. Tests that need the glue can use `#![cfg(durable)]`, so a plain `cargo test` skips them (see `durable/tests/runtime.rs`).

## Example

`apps/abuse` (run `apps/abuse/demo.sh`):

```rust
use durable::{Either, Outcome, Runtime};

durable::glue!(); // the generated glue, at the crate root: `GLUE`

#[durable::workflow]
async fn classify(report: Report) -> String {
    let score = fast_score(&report.text).await;        // fast path: borrows, tokio I/O, anything
    if score < 30 { return "allow".into(); }
    let deadline = durable::now_ms() + 3_000;
    let reviews = durable::join2(review("reviewer-a".into(), report.clone()),
                                 review("reviewer-b".into(), report.clone()));
    match durable::select2(reviews, durable::sleep_until(deadline)).await {
        Either::Left((a, b)) => decide(a, b),
        Either::Right(()) => {
            let d: Decision = durable::request("supervisor", &report).await;
            format!("supervisor: {}", d.block)
        }
    }
}

async fn review(reviewer: String, report: Report) -> Decision {
    durable::request(&reviewer, &report).await          // slow: the task parks here
}

let mut rt = Runtime::open("abuse.db", GLUE, |req| send_to_humans(req))?;
rt.register(classify)?;
match rt.start("r3", classify(report)).await? {
    Outcome::Done(verdict) => {}                        // fast path, nothing stored
    Outcome::Parked => {}                               // stored; requests sent
}
// later, any process:
rt.deliver("r3", req_id, Decision { block: true }).await?;
rt.run_due_timers().await?;
```

## Examples

| App | Shows |
|---|---|
| `apps/abuse` | Fast model first; two reviewers in parallel under a deadline; supervisor on a split or a timeout. |
| `apps/moderation` | Slow steps on both sides of a call. `moderate`: malware scan (~15 min) → per-client code (`acme`, `globex`) → manual review (1–3 days) → legal review for some categories. The client code uses randomness (spot checks) and the clock (business hours) without determinism rules. `scan`: the scanner service as a workflow (submit to a sandbox, poll with durable sleeps, resubmit on crash). |
| `apps/billing` | Flawless's `extend_subscription`: charge → invoice → extend, with a crash after each side effect. Handlers and idempotency keys give one charge and one email. |

Each app has a `demo.sh` that runs one process per step.

## API

| | |
|---|---|
| `#[durable::workflow]` | On a free, non-generic `async fn`. Marks a root. |
| `durable::glue!()` | Once, at the crate root. Defines `GLUE`. |
| `request::<T>(kind, payload)` | Slow call. Parks until `deliver(task, req_id, T)`. |
| `sleep_until(ms)` | Durable timer, wall clock in ms (`now_ms()`). |
| `DropOnPark` | Opt-in for a type whose destructor may run at park (see below). |
| `join2`, `select2`, `Either` | Combinators usable at slow awaits. The loser of `select2` is dropped, which cancels its requests. |
| `Runtime::open(path, GLUE, dispatch)` | `dispatch` sends a parked task's requests, after they are stored. |
| `register(wf)` | Every root, at startup, before resuming anything. |
| `start(id, wf(args))` | `Done(output)` or `Parked`. |
| `deliver(id, req_id, result)` | Records the result once, then runs the task until done or parked. Duplicate or late (cancelled) results are ignored. |
| `run_due_timers()`, `next_wake()` | Resume tasks whose timer is due. |
| `redispatch()` | Resend requests stored but not sent (crash between store and send). Call at startup. |
| `handle(kind, async fn)` | Run requests of `kind` in this process (an HTTP call, a DB write) instead of dispatching them. At least once; use `(task_id, req_id)` as the idempotency key. |
| `work()` | Run handlers for pending requests and due timers until idle. A failing handler's request stays pending. |
| `status(id)`, `output::<T>(id)`, `snapshot(id)`, `open_requests()` | Inspection. |
| `resume(id)` | Retry a task whose last resume failed (e.g. after deploying a compatible build). |

Delivery guarantees:

- A parked task's requests are sent at least once. They are sent only after the task is stored, so a result can never arrive for a task that does not exist yet.
- `deliver` is idempotent per `(task, req_id)`.
- Slices of one task never overlap.

## What can live across a slow await

The values live at a slow await are stored, in every frame from the workflow down to the slow call. Allowed:

- any type that is `Serialize + DeserializeOwned` and nameable from the crate root, including `String`, `Vec`, `HashMap`, your own `#[derive(Serialize, Deserialize)]` types, and arrays of any length;
- nested `async fn` calls, including from other crates and generic helpers (`retry::<F, Fut>`);
- closures that capture such values by move;
- `join2` / `select2` of the above; `request` and `sleep_until`;
- values moved conditionally before the await (drop flags).

Parking drops the in-memory frame, so the destructor of a live value runs when the task parks, and again on the rebuilt copy when its scope really ends. std types (`String`, `Vec`, `HashMap`, ...) are fine: their destructors only free memory. A type with its own `Drop` impl, anywhere inside a live value (a field, a `Vec` element, a closure capture, an argument of an unstarted future), is rejected: a lease release, a metrics timer, a span guard, or "mark as abandoned on drop" would run twice. End it before the await, or, if running its destructor at park is harmless, opt in with `impl durable::DropOnPark for T {}` (`request` does: its destructor cancels only when dropped during a poll).

Across *fast* awaits anything goes: borrows, sockets, tokio types.

## Everything else, and the error you get

| Code | Error | When |
|---|---|---|
| Generic workflow, `impl Trait` argument | "durable workflows cannot be generic (alpha)" | compile (macro) |
| Method workflow (`&self`) | "methods cannot be durable workflows (alpha)" | compile (macro) |
| `#[durable::workflow]` on a non-async fn | "goes on an `async fn`" | compile (macro) |
| A reference live at a slow await (including hidden temporaries, `for x in v.iter()`) | "`x` cannot be serialized at this slow await ... holds a reference" | compile (lint) |
| A type without serde impls (`Arc<T>`, `File`, tokio types) | "... does not implement Serialize + DeserializeOwned" | compile (lint) |
| A value kept only for its destructor | same, plus "`drop` it or close its block before the await" | compile (lint) |
| A value with a user `Drop` impl inside (`struct Lease` with `impl Drop`), not `DropOnPark` | "`x` has a destructor that would run when the task parks" | compile (lint) |
| A type private to its module | "... is private to its module ... make it `pub(crate)`" | compile (lint) |
| `tokio::join!`, `tokio::select!`, `tokio::time::timeout` around a slow call | the `MaybeDone` / `Timeout` value cannot be serialized | compile (lint) |
| An unstarted future held across a slow await (`Option<{async fn}>`) | "... holds an unnameable type" | compile (lint) |
| Not built with `cargo durable` / root not registered | `Error::NoGlue` / `Error::NotRegistered` | startup / `start` |
| A slow call hidden behind `Pin<Box<dyn Future>>` | `Error::NotSerializable("... trait object")`; nothing is sent | when it parks |
| Parking while another branch still runs fast work (e.g. `join2(request, http_call)`) | `Error::NotSerializable`; the task fails | when it parks |
| A panic | `Error::Panicked`; the task fails | any slice |
| A snapshot from a build where that state changed | `Error::Incompatible`; the task **stays parked** with the error recorded; `resume` it from a compatible build | resume |

`apps/unsupported/check.sh` compiles the compile-time cases and checks each error. `durable/tests/runtime.rs` covers the runtime ones.

## Code that decides (the per-client scripts)

The code between slow calls can be anything: user-written policy, randomness, the clock, a call to a config service. It is not re-run on resume, so it need not be deterministic. A decision made before a slow call is part of the stored state, and the task continues from it.

The flip side: whatever is live at a slow await is frozen in the snapshot. If a policy or config should apply to tasks already waiting, read it *after* the slow call, not before.

## Deploys

A parked state is matched by its state number plus its live locals and their types (`sig`). A new build accepts old snapshots whose parked state is unchanged, even if the rest of the function changed (steps added after it, fixed code after it). If it changed, resume fails with `Incompatible` and the task stays parked; keep an old build running to drain those tasks.

Caveat: states are numbered by await order. An await inserted *before* the parked one can make an old snapshot match the wrong await if the locals happen to be the same. Until awaits get stable keys, add new awaits after existing ones, or drain before deploying.

## Limits of the alpha

- Single-threaded: run the runtime on a current-thread tokio runtime (or a `LocalSet`). Tasks are polled one slice at a time per task.
- SQLite only. The store is trusted: snapshots are not authenticated.
- Cancelled requests are not reported to `dispatch`; late results are ignored.
- A failed resume of a corrupted snapshot can leak the values already rebuilt.
- Workflows are free `async fn`s; no async closures, no trait methods as roots.
- The two-pass build compiles workflow crates twice when the glue changes.
- The driver depends on rustc internals and the pinned nightly.
