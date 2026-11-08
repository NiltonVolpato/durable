# Runtime

How parked tasks are stored, woken and resumed ([overview](README.md)). What exists today is minimal: a poll loop (`poc`) and a single-threaded tokio host (`probe`), with slow results read from `results/<id>` files. This doc covers the current mechanics and the v2 design.

## Leaves

Leaf futures are the only places that park. Compiler-generated frames never store the `Context`, so the only wakers live in leaves, and slow leaves keep theirs outside the task.

- `slow(id)` (`Slow`): on poll, look up the result. If it is missing, set `header.parked = id` for this poll and return `Pending`. Today the result is a file; in v2 it is a message (below).
- `sleep_until(t)` (`SleepUntil`): a durable timer. The deadline is wall-clock seconds stored in the frame, so it survives a park and can expire between processes.
- `step(f)` (planned): run an ordinary tokio future as a separate task keyed by id, and store its output. The frame holds only the id. After a crash the step runs again, so steps are at-least-once.

Both leaves implement `SlowLeaf`, which the [lint](serialized-frames.md#compile-time-lint) uses to find slow awaits.

## Task lifecycle

1. A request starts as an ordinary in-memory task. The fast path completes without touching storage.
2. A poll that returns `Pending` with `header.parked` set means the task is waiting on a slow call. A poll without it means fast work, which tokio will wake.
3. **Park:** serialize the root frame, store it, drop the task. On the raw-copy path, copy the arena instead and `mem::forget` the future.
4. When the slow result arrives, store it and wake the task.
5. **Resume:** load the snapshot, rebuild the frame, poll again.
6. **Cancel a parked task:** rebuild it and drop it, so its destructors run.

Open: if a task is parked on a slow call but another branch is still doing fast work (e.g. a join), its current state may not be serializable yet. The host should then keep it in memory until the next wake, rather than fail.

## Lessons from Infinity

[Infinity](related-work.md) runs agents as stateless slices over durable storage, on infrastructure with only at-least-once delivery. Its runtime answers several of our open problems:

1. **Everything is a message.** Slow results, timer wake-ups and external events arrive as one kind of input on one queue, keyed by task id. A resume is one *slice*: load the snapshot → poll until the next slow park → store → exit. The `results/<id>` files are a crude version of this.
2. **Ordering per task in the queue.** Messages for a task are grouped (SQS FIFO `MessageGroupId`, or one channel and worker per task when embedded). That serializes slices of the same task without locks, which removes the double-resume problem: no leases needed.
3. **Remember processed message ids.** Each input carries a stable id, and the store records which ids a task has consumed. A redelivered message is skipped, so slices are idempotent on top of at-least-once delivery.
4. **Persist before dispatching.** Infinity flushes the turn before it sends a tool call, so the call is durable before its result can arrive. For us: a slow call's side effect (asking for a review, scheduling a timer) must be sent only after the snapshot that waits for it is stored. Today `Slow` only parks; in v2 a leaf records an outgoing request, and the host sends it after the store commits (an outbox). Otherwise the request carries an idempotency key.
5. **Serverless falls out of the design.** With 1–4, no process has to stay up for a parked task, and the platform's scale-to-zero is the hibernation.

## v2 plan

- **Store** behind a `Store` trait (SQLite first): snapshots by task id, processed message ids, the outbox.
- **Queue** of input messages keyed by task id, FIFO per task. In-process first (one channel per task), so the same code can later run on SQS FIFO + Lambda.
- **Leaves** as above. `Slow` and `SleepUntil` register their requests in the outbox; results and timer firings come back as messages.
- **Host** on tokio: run a slice per message; park when a slow leaf parked and the state serializes; otherwise wait for tokio to wake the fast work.
- **Recovery:** on startup, nothing to scan. Parked tasks resume when their next message arrives. Undelivered outbox entries are re-sent.
- **Multi-threaded tokio:** `Task` is `!Send` today. It should be `Send` when its root future is.

## Binary identity

A snapshot is only valid for code that matches it, and slow calls can outlive deploys.

- **Raw copy:** valid only for the exact build. The coroutine layout changes with the source, rustc, dependencies, target and MIR opt level. The `poc` binary checks a hash of its executable.
- **Serialized frames:** valid for any build whose parked state has the same locals and types ([versioning](serialized-frames.md#loading-snapshots-into-a-changed-build)). A snapshot that no build accepts needs an old build to finish it.

The original plan for running old code: build each workflow as a `cdylib` named by the hash of its bytes (`workflow_<hash>.so`), and `dlopen` the matching one on resume. Host and workflow talk through a C ABI:

```rust
#[repr(C)]
pub struct WorkflowVTable {
    pub size: usize,
    pub align: usize,
    /// Build the root future in `frame` from serialized input.
    pub init: unsafe extern "C" fn(frame: *mut u8, input: *const u8, len: usize),
    /// Poll once. Returns READY, PENDING or PARKED_SLOW.
    pub poll: unsafe extern "C" fn(frame: *mut u8, host: *const HostVTable) -> u32,
    /// Copy out the serialized output after READY.
    pub output: unsafe extern "C" fn(frame: *mut u8, out: *mut OutBuf),
    pub drop: unsafe extern "C" fn(frame: *mut u8),
}
```

Problem: a `cdylib` statically links its own std and tokio. The `.so`'s tokio has its own runtime-context thread-local, so tokio I/O inside a workflow sees no runtime ("no reactor running"). Linking std and tokio dynamically fixes that but makes updating them hard. With serialized frames, a simpler option is to keep old *binaries* running as workers for snapshots only they accept, until those tasks drain.

## Open

- What happens to a task that panics mid-poll: fail it, or retry the slice?
- How long to keep old builds, and what to do with a snapshot that no build accepts.
- Eviction policy for the fast/slow split: park right away, after a delay, or under memory pressure.
- Resume trusts the store. Snapshots need a checksum and atomic writes, and the store must be trusted. Raw-copy snapshots are arbitrary memory; serialized ones are only as safe as the glue's checks.
