# Status

What each PoC step showed, how to run it, and what is left for an alpha ([overview](README.md)).

## Layout

The workspace is virtual. The alpha is `durable/`, `durable-macros/`, `driver/` and `apps/`; the proof of concept is `poc/` (crate `durable-poc`), `steps/` and `probe/`.

| Path | What |
|---|---|
| `durable/`, `durable-macros/` | The alpha: runtime on SQLite, leaves, combinators, `#[workflow]`, `glue!()` |
| `driver/src/cargo_durable.rs` | `cargo durable build|run|test` |
| `apps/abuse/` | Alpha example (`demo.sh`) |
| `apps/unsupported/` | Code outside the alpha subset and its errors (`check.sh`) |
| `apps/moderation/`, `apps/billing/` | Examples: slow steps on both sides of a call; crash-safe side effects (`demo.sh`) |
| `poc/src/lib.rs` | `Durable`, leaves (`Slow`, `SleepUntil`), `Ephemeral`, `Join2`, `Select2`, `Structural`, raw-copy `run` |
| `poc/src/arena.rs` | `Task` (arena + header), `ABox`, `DVec`, `RelRef` |
| `poc/src/glue.rs` | Serialized-frame runtime (`serialize`, `deserialize`, helpers for generated code) |
| `poc/src/main.rs` | `poc` binary: raw copy (`start`/`resume`) and serialized (`sstart`/`sresume`) |
| `poc/src/compile_checks.rs` | Doctests for the `Durable` auto trait |
| `driver/` | `durable-driver`: frame collection, glue, lint, drop flags |
| `steps/`, `probe/` | SDK crate and the tokio probe |
| `poc/tests/roundtrip.rs` | Raw-copy round trips (Miri) |
| `poc/tests/serde_roundtrip.rs` | Serialized round trips (native + Miri, via `driver/serde-test.sh`) |
| `poc/examples/lint_fail.rs` | Workflows the lint must reject |

## How to run

```
apps/abuse/demo.sh                          # alpha: abuse classification, one process per step
apps/unsupported/check.sh                   # alpha: every unsupported case gets its error
apps/moderation/demo.sh                     # scan -> per-client code -> review -> legal; scanner service
apps/billing/demo.sh                        # charge -> invoice -> extend, with crashes
target/debug/cargo-durable durable test -p durable --test runtime   # alpha runtime tests
cargo test                                  # every crate; PoC doctests + raw-copy round trip
poc/demo.sh                                 # PoC multi-process demo, both paths
poc/build-glue.sh [pkg crate main]          # PoC two-pass build with serde glue
probe/run.sh 7                              # realistic tokio workflow, 6 parks
driver/lint-check.sh                        # the lint rejects examples/lint_fail.rs
driver/evolve-demo.sh                       # old snapshot into a changed build
driver/serde-test.sh                        # serialized round trips, native + Miri
MIRIFLAGS=-Zmiri-disable-isolation cargo miri test -p durable-poc --test roundtrip
```

The toolchain is pinned (`rust-toolchain.toml`). After changing it, if Miri fails with `can't find crate for serde` (E0463), remove `<target dir>/miri`: cargo keeps the old dependency builds, but Miri's standard library is rebuilt per toolchain.

Run the binaries from `cargo durable` / `poc/build-glue.sh` directly. A plain `cargo build` afterwards rebuilds them with an empty glue table.

## Steps

### v0

Raw copy of a native `async fn`: frame in a `Box`, hand-written poll loop with `Waker::noop()`, snapshot checked against a hash of the executable.

- `poc start classify 7` → park → `resume` → park on the nested fn's slow call → `resume` → 49. Three processes, frame at a different address each time.
- User auto traits propagate through `async fn` exactly like `Send`, with the same diagnostics.
- Compile checks: borrows, `String`, `&'static str` and hidden temporaries across a slow await are rejected. Dead borrows and nested async fns are accepted.
- Negative controls: forcing a frame borrow past the check gives a segfault, bus error or wrong answer on resume. `join(slow, ephemeral)` led to the eviction-time check.

### v1

- Fixed-size task arena (64 KiB): header, root frame, task data. Eviction copies the used prefix (264 B for `classify`, 640–1152 B for `triage`).
- `ABox`, `DVec` (grows iff `T: Unpin`) and `RelRef`. `triage` holds all three across two slow awaits and finishes in a third process (574).
- Per-task state (ephemeral counter, parked flag) moved into the arena header.
- Negative impls for OS handles and `Instant`.
- Raw-copy round trips pass under Miri.

### Serialized frames

- `durable-driver` dumps coroutine layouts; a snapshot decoded with only the dump matched.
- Two-pass serde glue: `profile` holds a `Vec<String>` and a `String` across two slow calls and finishes in a third process (60).
- Compile-time lint at slow awaits (6 rejected cases).
- Versioned snapshots: a v1 snapshot finishes in a v2 that added a step; a v3 that changed the parked state refuses it.
- Miri: no UB and no leaks with uninitialized non-live bytes; the negative control is caught.

### Tokio probe

- One realistic workflow on tokio with real I/O, an SDK crate, a join, a select against a durable timer, a generic helper with a closure, and a drop flag.
- The driver now follows frames across crates and generics, with combinators, closures and drop flags.
- `probe/run.sh 7` → 65, `probe/run.sh 8` → 66.

See [serialized frames](serialized-frames.md#realistic-probe-on-tokio).

### Alpha subset

- Crates `durable` (runtime, leaves `request` / `sleep_until`, `join2` / `select2`, glue runtime) and `durable-macros` (`#[workflow]`, `glue!()`).
- Users need no `register_tool` and no build script. The macro adds a marker the driver finds, and `glue!()` includes the glue through tracked env/path.
- `cargo durable build|run|test`: tries the current glue first, which is one compile when nothing changed. Otherwise it regenerates the glue and builds again.
- Runtime on SQLite:
  - the fast path stores nothing;
  - store-then-dispatch outbox, plus `redispatch`;
  - idempotent `deliver`;
  - per-task lock and durable timers;
  - requests abandoned by `select2` are cancelled;
  - `resume` for tasks whose resume failed.
- Errors for code outside the subset: [table](alpha.md#everything-else-and-the-error-you-get). Building `apps/unsupported` found two lint gaps, now fixed:
  - slow leaves nested in opaque types (`Timeout<Request>`, `tokio::join!`) counted as fast;
  - types private to a module failed inside the generated code. The lint now names them.
- Verified: `apps/abuse/demo.sh`, `apps/unsupported/check.sh` (10 cases), `durable/tests/runtime.rs` (8 tests: fast path, restart, duplicate, deadline + cancellation, panic, hidden slow call, incompatible snapshot, redispatch, missing glue). PoC flows unchanged.

### Examples

- `apps/moderation` (moderation with a scanner service), `apps/billing` (Flawless's `extend_subscription` with crashes). See [examples](alpha.md#examples).
- New in the runtime: `handle(kind, async fn)` and `work()`, for in-process steps (the planned `step(f)`). At least once, with `(task_id, req_id)` as the idempotency key.
- Found by `moderate`: a drop flag whose value is dropped field by field was not paired. This happens for structs without a `Drop` impl, and for enums after a discriminant switch. The MIR matcher now takes the saved local as a prefix of the dropped place.

## Road to an alpha

The alpha subset above is usable: one store, single-threaded, a subset of Rust with errors for the rest. What remains for a broader release:

| Area | Missing | Size |
|---|---|---|
| Runtime | Multi-threaded tokio; other stores and queues (SQS FIFO, Postgres); cancellation notices; eviction policy ([v2 plan](runtime.md#v2-plan)) | Medium to large, mostly known |
| Build tooling | Single-pass build; CI; release process tied to a nightly | Medium |
| Deploys | Stable await keys; old builds draining their tasks | Medium, some design open |
| Coverage | `dyn` futures, enums holding frames, tokio combinators, private types in other modules, async closures, async fns in traits, `Arc` state | Unknown: the long tail |
| Robustness | Panics, leaks on failed resume, `step(f)`, cancellation, more Miri coverage | Medium |
| Usability | Docs, messages, inspecting parked tasks | Small to medium |

Risks:

- **Long tail.** One realistic workflow surfaced six special cases. Porting one or two real workflows from an existing codebase would measure how many more remain.
- **rustc internals.** Every nightly update can break the driver. Long term, compiler support upstream (reading coroutine layouts, or a derive for coroutines) would remove this.

The raw-copy path (arena, `Durable`, `RelRef`, `Ephemeral`) is not part of the alpha. It stays in the PoC crate.
