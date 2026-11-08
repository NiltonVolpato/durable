# Durable native futures in Rust

Write a durable workflow as a plain `async fn`. The fast path runs in memory and stores nothing. When the function awaits a slow call (a request answered hours later, a timer), its state is serialized into SQLite and dropped from memory. When the answer arrives, possibly in another process after a restart or a deploy, the state is rebuilt and the function continues where it stopped.

There is no replay and no hand-written state machine, so the code between slow calls need not be deterministic: randomness, the clock and config reads are fine.

Status: **alpha**. One SQLite store, single-threaded, a subset of Rust. Code outside the subset gets an error, at compile time where possible. Nightly only.

```rust
use durable::{Either, Outcome, Runtime};

durable::glue!();

#[durable::workflow]
async fn classify(report: Report) -> String {
    let score = fast_score(&report.text).await;             // fast path: borrows, tokio I/O, anything
    if score < 30 {
        return "allow".into();
    }
    let review = durable::request::<Decision>("manual_review", &report);  // slow: parks here
    match durable::select2(review, durable::sleep_until(durable::now_ms() + DAY)).await {
        Either::Left(d) => if d.block { "block".into() } else { "allow".into() },
        Either::Right(()) => "review timed out".into(),
    }
}

let mut rt = Runtime::open("tasks.db", GLUE, |req| send_to_reviewers(req))?;
rt.register(classify)?;
match rt.start("r1", classify(report)).await? {
    Outcome::Done(verdict) => { /* fast path, nothing stored */ }
    Outcome::Parked => { /* stored; the review request was sent */ }
}
// Later, in any process:
rt.deliver("r1", req_id, Decision { block: true }).await?;
```

## How it works

A custom rustc driver (`durable-driver`) reads the compiler's own layout of each `async fn` state machine: which locals are live at each await, their types and offsets. From that it generates serde glue for every frame reachable from a workflow, including nested async fns, generic helpers, closures and combinators, from any crate. It also checks at compile time that every value live at a slow await can be stored:

```
error: `r` cannot be serialized at this slow await
  --> src/main.rs:24:35
24 |     let v: u64 = request("a", id).await;
   |                                   ^^^^^ the task can park here
note: `r` is live across the await: `&str` holds a reference
```

Details in [docs/serialized-frames.md](docs/serialized-frames.md).

## Try it

Requirements: `nightly-2026-11-08`, pinned in `rust-toolchain.toml` (rustup installs it, with `rustc-dev`, `llvm-tools` and `miri`). Tested on macOS (arm64) and Linux (Ubuntu 24.04, x86_64).

```sh
cargo build -p durable-driver                       # builds durable-driver and cargo-durable
apps/abuse/demo.sh                                  # abuse classification, one process per step
apps/moderation/demo.sh                             # malware scan -> per-client code -> review -> legal
apps/billing/demo.sh                                # charge -> invoice -> extend, surviving crashes
apps/unsupported/check.sh                           # code outside the subset, and its errors
target/debug/cargo-durable durable test -p durable  # runtime tests
```

Build your own crates with `cargo durable build|run|test` (`cargo install --path driver` puts it on your PATH). A plain `cargo build` compiles, but without the generated glue `Runtime::register` reports the missing glue.

## Layout

| Path | What |
|---|---|
| `durable/` | The runtime: `Runtime`, `request`, `sleep_until`, `join2`, `select2`, handlers |
| `durable-macros/` | `#[durable::workflow]`, `durable::glue!()` |
| `driver/` | `durable-driver` (glue generation, slow-await lint) and `cargo durable` |
| `apps/` | Examples (`abuse`, `moderation`, `billing`) and `unsupported` |
| `poc/`, `steps/`, `probe/` | The proof of concept: a raw-copy approach and the first serialized-frame experiments |
| `docs/` | Design and user docs |

## Docs

- [Alpha user guide](docs/alpha.md): API, what can live across a slow await, the error for everything else, deploys, limits.
- [Overview](docs/README.md): the motivating use case and the two approaches tried.
- [Serialized frames](docs/serialized-frames.md): the driver, glue generation, the lint, versioning, Miri.
- [Runtime](docs/runtime.md): task lifecycle, outbox, delivery guarantees, the plan beyond the alpha.
- [Related work](docs/related-work.md): Infinity, Flawless, replay-based engines.
- [Status](docs/status.md): what each step showed, and what is left.

## Limits

- Nightly only. The driver uses rustc internals and is tied to the pinned nightly.
- Single-threaded runtime, SQLite only.
- Workflow roots are free, non-generic `async fn`s.
- A parked state is matched to a new build by its live locals and types. Snapshots survive deploys that leave the parked await unchanged; others stay parked until a compatible build resumes them.
