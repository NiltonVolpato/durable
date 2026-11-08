# Durable native futures in Rust

Nov 8, 2026 · @Nilton Volpato

A Rust async runtime where a native `async fn` can be suspended at a slow call, stored, and resumed later in another process. There is no replay and no hand-written state machine.

## Motivating use case

Request-based flows with a fast path and a rare slow path. Example: abuse classification. Most requests get a verdict from a fast model in milliseconds. Some trigger a slow classifier (or human review) that answers minutes, hours or days later.

What we want:

- The fast path is a plain in-memory future and never touches storage.
- When a request parks on a slow call, its state goes to storage and its memory is released.
- A restart, crash or deploy does not lose parked requests.
- The workflow is written as a normal `async fn`, not as an explicit state machine or a replayed journal.

## Two approaches

Only the state live at a slow call has to survive. Fast sections can use borrows, tokio I/O and the heap freely. The PoC tried two ways to store that state:

| | [Raw copy](raw-copy.md) | [Serialized frames](serialized-frames.md) |
|---|---|---|
| Stored as | The task's memory, copied as bytes | Each live value, through serde (JSON today) |
| What may live across a slow await | Plain data and arena handles (`Durable` auto trait) | Anything `Serialize + Deserialize`, including heap data |
| How it is checked | Auto trait over the whole future, plus `Ephemeral` sections checked at run time | `durable-driver` (a rustc_driver) checks each slow await at compile time |
| Snapshot valid for | The exact build that wrote it | Any build where the parked state has the same locals and types |
| Needs | Nightly (`auto_traits`, `negative_impls`) | Nightly + `rustc-dev`; a two-pass build |

Serialized frames is the main path now. The raw-copy path is the original idea; it is kept for comparison and may be dropped for an alpha (see [status](status.md)).

## Docs

- [Raw copy](raw-copy.md): the `Durable` auto trait, the task arena, `ABox`/`DVec`/`RelRef`, `Ephemeral` sections, and their holes.
- [Serialized frames](serialized-frames.md): `durable-driver`, glue generation, the slow-await lint, snapshot versioning, Miri, and the realistic tokio probe.
- [Runtime](runtime.md): leaves, task lifecycle, deploys and binary identity, and the v2 runtime plan.
- [Related work](related-work.md): Infinity (Hydro), and how it compares.
- [Status](status.md): what each PoC step showed, how to run it, and what is left for an alpha.

Tested on `rustc 1.102.0-nightly (2026-11-07)`, macOS arm64 (PIE).
