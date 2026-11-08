# Durable native futures in Rust

Nov 8, 2026 · @Nilton Volpato

## Summary

A Rust async runtime where a native `async fn` can be suspended at a slow call, have its frame copied to storage, and be resumed later in another process. There is no replay and no hand-written state machine.

The core ideas:

- A `Durable` auto trait marks types that hold no absolute pointers. The compiler checks it structurally, the same way it checks `Send` on async blocks.
- Each task lives in one contiguous arena. Pointers inside the arena are offsets (from the arena base, or self-relative `RelRef`), so the whole arena can be moved with a memcpy.
- Only the state live at a slow call has to be durable. Fast sections can use borrows, tokio I/O and the heap freely.
- Alternative found in the PoC: a rustc_driver reads the compiler's coroutine layouts and generates serde glue, so a parked frame is serialized field by field (heap data allowed) instead of copied as raw bytes.
- Frame bytes are only valid for the code that wrote them. Workflows are built as shared libraries, named by the hash of the `.so`, and loaded with `dlopen` on resume.

It needs nightly (`auto_traits`, `negative_impls`).

Status: v0, v1 and the serialized-frame experiment work (see [PoC status](#poc-status)). Tested on `rustc 1.102.0-nightly (2026-11-07)`, macOS arm64 (PIE).

## Motivating use case

Request-based flows with a fast path and a rare slow path. Example: abuse classification. Most requests get a verdict from a fast model in milliseconds. Some trigger a slow classifier (or human review) that answers minutes, hours or days later.

What we want:

- The fast path is a plain in-memory future and never touches storage.
- When a request parks on a slow call, its state goes to storage and its memory is released.
- A restart, crash or deploy does not lose parked requests.
- The workflow is written as a normal `async fn`, not as an explicit state machine or a replayed journal.

## The `Durable` auto trait

`Durable` means: this value stays valid if the whole task arena it lives in is moved to another address, including in another process.

```rust
#![feature(auto_traits, negative_impls)]

/// Safety: the type holds no absolute addresses or other process-local
/// identity, and nothing outside the task arena holds its address.
pub unsafe auto trait Durable {}

impl<T: ?Sized> !Durable for &T {}
impl<T: ?Sized> !Durable for &mut T {}
impl<T: ?Sized> !Durable for *const T {}
impl<T: ?Sized> !Durable for *mut T {}

// Process-local handles: plain integers inside, so they pass the
// structural check, but are meaningless in another process.
impl !Durable for std::os::fd::OwnedFd {}
impl !Durable for std::fs::File {}
impl !Durable for std::net::TcpStream {}
// ... TcpListener, UdpSocket, Unix sockets, Child + stdio, ThreadId,
// Instant (monotonic clock: use SystemTime instead).
```

`NonNull` needs no impl of its own: it wraps a raw pointer (on recent nightlies a pattern type `*const T is !null`), so it is excluded structurally. So are `Box`, `Vec`, `String`, `Rc`, `Arc` and `AtomicPtr`. `Waker` holds a vtable and a data pointer, so it is excluded.

The handle list cannot be complete. It names std types; crates with their own handle newtypes (indices into process-wide tables, pids, tokio types once their `Arc` is gone) need their own negative impls. A negative impl on `OwnedFd` already covers every std type built on it.

Auto traits propagate through a coroutine's captured values and the values it holds across awaits. So `impl Future + Durable` makes the compiler check a native `async fn` with no macro. Confirmed in v0, with the same diagnostics as `Send`:

```
note: future does not implement `Durable` as this value is used across an await
3 |     let s = &body;   - has type `&[u8; 64]` which does not implement `Durable`
4 |     let a = slow(id).await;   ^^^^^ await occurs here, with `s` maybe used later
```

The witness is computed from MIR liveness, so a borrow that ends before the await is accepted.

Relation to `Unpin`:

| Type | `Unpin` | `Durable` |
| --- | --- | --- |
| `u64`, `[u8; N]`, plain structs | yes | yes |
| `RelRef<'a, T>` | no | yes |
| `&T`, `Box<T>`, `Vec<T>` | yes | no |
| compiler self-borrows in a frame | no | no |

`Unpin` means safe to move on its own. `Durable` means safe to move together with the whole arena.

The arena move goes against `Pin`'s drop guarantee: pinned memory must not be reused without running drop. That is why the trait is `unsafe` and its contract includes "no pointers from outside the arena into this value". The structural check cannot see such incoming pointers.

## Task arena and `RelRef`

Each task owns one contiguous arena: a header, then the root future's frame, then bump-allocated task data. Eviction copies the used prefix. Resume copies it into a new arena at any address (same size and alignment).

```rust
#[repr(C)]
struct Header {        // offset 0, moves with the arena
    task_id: u64,
    parked: u64,       // slow event that parked during this poll, or NONE
    cap: u32, align: u32, used: u32,
    root_off: u32, root_size: u32,
    ephemeral: u32,    // live ephemeral sections in this task
}
```

All per-task runtime state lives in the header. The only thread-local is the base of the arena being polled, set around each poll and restored after (like tokio's current-task context). Nothing persists in it between polls, so it works with many tasks per thread.

### Two kinds of in-arena pointer

v1 showed that self-relative pointers cannot be held by the frame itself. The frame is in the arena, but user code moves frame locals freely (`let b = a;`, passing by value), and `!Unpin` does not stop moves of an owned value. Only `Pin<&mut T>` does, and that is a borrow, so it is `!Durable` across an await. So there are two kinds:

```rust
/// Offset from the arena base. Plain value: the frame holds and moves it.
/// Deref reads the current task's arena base; panics outside a poll.
pub struct ABox<T> { off: u32, task: u64, _t: PhantomData<T> }
pub struct DVec<T> { off: u32, len: u32, cap: u32, task: u64, _t: PhantomData<T> }

/// Offset from &self. Deref needs no context. Lives only at stable
/// addresses (arena data, pinned slots) and is set through Pin.
pub struct RelRef<T> { off: isize, _pin: PhantomPinned, _t: PhantomData<T> }
impl<T> RelRef<T> {
    pub const fn null() -> Self;                         // the only by-value constructor
    pub fn set(self: Pin<&mut Self>, target: &T);        // asserts both ends in this arena
    pub fn get(&self) -> Option<&T>;
}
```

- All are auto `Durable` when `T` is: no pointer fields, `PhantomData<T>` not `PhantomData<&T>`.
- Handles store `task` and check it on deref, so a handle used in another task panics instead of reading the wrong arena.
- `RelRef::set` asserts that both ends are inside the current arena. This runtime check replaces the brand lifetime for now.
- `RelRef` is not `Clone`, and the API never hands one out by value once set, so it cannot be moved to a wrong address in safe code.
- `RelRef::get` rebuilds the pointer with `with_exposed_provenance` (the arena base is exposed on allocation). Miri accepts this, with a warning.

### `DVec` growth

`DVec<T>` grows only if `T: Unpin`, because `Unpin` already means "safe to move on its own". `RelRef` is `!Unpin`, and so is any struct that holds one, so those vectors are fixed-capacity (`with_capacity` + `push_pinned`). A growable `push` of `!Unpin` elements does not compile (E0599). Old buffers are abandoned in the bump arena. Element destructors are not run.

### Brand lifetime: deferred

The planned `RelRef<'a, T>` brand needs a higher-ranked entry point (`for<'a> AsyncFnOnce(Token<'a>)`), which makes naming `F` for resume harder. A branded ZST token held in the frame would be `Durable`. It could also act as a GhostCell-style permission token (`&Token` to read, `&mut Token` to write), which would close the aliasing hole below. Not tried yet.

### Is `RelRef` needed at all?

Its only advantage over base-relative handles is that deref needs no context. But `ABox`/`DVec` already need the per-poll context, so that advantage buys little. A base-relative `ARef<T> { off: u32 }` could replace it:

- It can be `Copy` and needs no `Pin`.
- It works as a frame local.
- A `DVec` of them can grow.
- Cost: a thread-local read plus the task check on each deref, and no deref outside a poll.

Decision: replace `RelRef` with base-relative `ARef`. Not implemented yet.

Decision: do not grow vectors in the arena (bump allocation wastes every old buffer). Build with a normal heap `Vec`. If the data is not needed across a slow await, that is it. If it is, freeze it into the arena once it has its final size: copy into an exact-size arena slice (`AVec`/`ASlice`, no spare capacity). Not implemented yet.

Known aliasing hole: `RelRef::get` returns `&T` without tracking it against `ABox::get_pin` / `DVec::get_pin` (`&mut` to the same target). Safe code can alias.

The compiler's own borrows (`let s = &buf; x.await;`) are absolute pointers and cannot be made relative. Rust has no relative pointer type, even on nightly. So the arena only covers user data. Compiler borrows must not be live at a slow call (next section).

## Only slow calls need `Durable`

When a task is parked on a slow call, the bytes that matter are the values live across that await, in every frame from the root down to the slow leaf. Bytes of inactive coroutine variants are leftovers and copying them is harmless.

The leaf alone cannot enforce this. A `where Self: Durable` on the slow future checks only its own fields. A future cannot bound whoever awaits it, and `poll` gets no type information about callers. The callers' frames are the real problem.

The auto trait over-approximates: it is computed over the union of all variants of a coroutine. One `&T` held across a fast await makes the whole future `!Durable`. A finished fast sub-future still makes it `!Durable`, because its type is one of the variants.

Two ways to get a check that applies only at slow calls:

### Option A: `Durable` on the whole chain, plus `Ephemeral` (no compiler work)

Every fn in the chain returns `impl Future + Durable`. Fast, non-durable sections go inside a wrapper that asserts `Durable` and is checked at runtime:

```rust
pub struct Ephemeral<F> { inner: F, entered: bool }
unsafe impl<F> Durable for Ephemeral<F> {}
// poll: on first poll, increment the task's ephemeral counter;
// on Ready or Drop, decrement it.

async fn classify(req: Req) -> Verdict {
    let quick = ephemeral(async {
        let s = &req.body;           // borrows, tokio I/O, Box: all fine here
        fast_model(s).await
    }).await;
    if quick.confident() { return quick.verdict(); }
    slow_model(req.id).await          // Slow leaf: requires counter == 0
}
```

The `async {}` block's type is hidden inside `Ephemeral`, so the outer witness only sees `Ephemeral<_>: Durable`. The slow leaf checks the counter when polled. If it is nonzero, a slow call is running inside a fast section. The runtime then keeps that task in memory, or panics in debug builds. Pointers from an ephemeral section into outer frames are fine, because the section is gone before any eviction.

The check in the leaf alone is not enough (found in v0). In `join(slow(..), ephemeral(..))` the slow leaf parks first, sees a counter of 0, and then the ephemeral section starts in the same poll. Evicting that task saved a live borrow, and the resume segfaulted. Fix: the runtime also checks before it evicts, and evicts only if the task is parked on a slow call and its counter is 0.

The counter must be per task, not per thread: on tokio, task A's ephemeral section can stay pending while task B runs on the same thread. In v1 it lives in the arena header. Because the header moves with the arena, a bad eviction is also caught on resume.

Open hole (found after v1): the counter only counts sections that have been polled. `let e = ephemeral(async { use(&body) }); slow().await; e.await` compiles, is evicted with the counter at 0, and crashed on resume (bus error). The unpolled section already holds its borrows. Fix: count from construction (`ephemeral()` runs inside a poll), not from first poll.

Cost: one `ephemeral(...)` per fast section, and a slow call inside one is caught at run time, not compile time. Still not satisfying; open to a better idea.

### Option B: MIR lint (precise, needs a rustc\_driver)

`tcx.coroutine_layout(..)` lists, for each suspension variant, the saved locals and their types (`variant_fields`, `field_tys`). The awaited future is one of those saved locals. Define a property `DurableAtSlow`:

- A `Slow<T>` leaf is `DurableAtSlow`.
- A coroutine variant is a slow point if its awaited future is `DurableAtSlow`. At every slow point, all other saved locals must be `Durable`, using the auto trait as the per-type test.
- A coroutine is `DurableAtSlow` if all its slow points pass and it has at least one.
- Hand-written combinators (`join`, `select`) cannot be inspected. They must be fully `Durable`, or come in annotated versions.

With these rules, any await on something that can reach a slow leaf is a slow point. So if a slow leaf returned `Pending` during a poll, every frame above it is at a checked variant. Fast paths need no annotation.

A proc macro is not a good replacement. It sees names, not liveness or types, and misses hidden temporaries held across an await: the iterator in `for x in v.iter() { slow().await }`, the scrutinee of `match &foo`, the `&x` in `f(&x, slow().await)`. Each miss is an absolute pointer restored in a new process.

## Runtime mechanics

This can likely sit on top of tokio. No custom executor seems necessary. The runtime adds a supervisor, an event table and a store.

Leaf futures:

- `Slow<T>` / `wait_event(id)`: on poll, look up the result in the store. If missing, register `(task_id, event_id)` and the current waker in a table outside the arena, set `header.parked` for this poll, and return `Pending`. The leaf itself stores no `Waker`.
- `sleep_until(t)`: same, with a wall-clock deadline stored as an integer.
- `step(f)`: runs an ordinary tokio future as a separate task keyed by ID and saves its output (which must be `Durable` or serializable) in the store. The frame holds only the ID. After a crash the step runs again, so steps are at-least-once.

Compiler-generated frames never store the `Context`, so the only wakers are in leaves, and slow leaves keep theirs outside the arena.

Task lifecycle:

1. A request starts as an ordinary in-memory task: a tokio task wrapping the arena and the root future.
2. After each poll that returns `Pending` with `header.parked` set and `header.ephemeral == 0`, the task may be evicted. Policy: right away, after a delay, or under memory pressure. The fast path never reaches this point.
3. Evict: copy the used part of the arena to the store with `(task_id, workflow_hash, size, align)`. Then `mem::forget` the in-memory future, since ownership is now on disk. Free the arena memory.
4. When the event arrives, the result is written to the store and the task is marked ready.
5. Resume: load the workflow `.so` for the hash, allocate an arena with the right size and alignment, copy the bytes in, and spawn a tokio task that polls the root future again.
6. Cancel a task that exists only on disk: load it the same way and drop it, so its drop glue runs.

## Binary identity: workflows as shared libraries

The coroutine layout is unspecified. It changes with the source, the rustc version, dependency versions, the target and the MIR opt level. A snapshot is valid only for the exact code that wrote it, and slow calls can outlive deploys.

Design:

- Each workflow is built as a `cdylib` and stored as `workflow_<hash>.so`, where the hash is of the built `.so` bytes. Hashing the output covers every input that affects the layout. With a reproducible build, an unchanged workflow keeps its hash when the rest of the host binary changes.
- Each snapshot row stores `(task_id, workflow_hash, size, align, bytes)`.
- On resume, the host `dlopen`s the matching `.so`. Several versions can be loaded at once until their tasks drain.
- The host and the `.so` talk only through a C ABI, so a newer host can drive an old workflow:

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

`HostVTable` gives the workflow access to the store, the event table and step spawning. Waking crosses the boundary as task and event IDs, not as Rust `Waker`s.

Open problem (ignored for now): a `cdylib` statically links its own std and tokio. The `.so`'s tokio has its own runtime-context thread-local, so `tokio::spawn` or tokio I/O inside a workflow sees no runtime ("no reactor running"). That conflicts with "fast sections can use tokio freely". Linking std and tokio dynamically would fix it, but then updating their versions is hard. Other options: route fast I/O through `HostVTable`, or run a small runtime inside each `.so`.

## Serialized frames (rustc_driver experiment)

Idea: serialize a parked coroutine field by field with serde instead of copying raw bytes. A library cannot do this: it needs the live locals, types and offsets at each suspension point, which only the compiler knows. A proc macro sees too little (see Option B). `coroutine_clone` derives `Clone` only for movable coroutines; `async fn` futures are immovable and get no impl (checked, E0277).

The experiment uses `durable-driver` (`driver/`): rustc plus a pass after analysis, run as `RUSTC_WORKSPACE_WRAPPER`. It needs the `rustc-dev` and `llvm-tools` components (in `rust-toolchain.toml`).

What it reads, per non-generic coroutine in the crate:

- `tcx.coroutine_layout(def_id, args)`: for each variant, the saved locals, their types and debuginfo names.
- `tcx.layout_of(ty)`: field offsets per variant, the tag (direct encoding, 1 byte here), size and alignment.
- `rustc_const_eval::util::type_name`: matches `std::any::type_name::<F>()` at run time.

Validated: `driver/decode_snapshot.py` decodes a real parked `snap.bin` from the raw-copy path using only the dump, including the nested `second_opinion` frame.

### Glue generation (two passes)

`build-glue.sh` runs two builds:

1. Pass 1 builds with an empty glue table. The driver writes `glue.rs`.
2. Pass 2 includes `glue.rs` (via `build.rs` → `$OUT_DIR/durable_glue.rs`). The driver regenerates the glue and fails the build if it differs.

Pass 2 confirms that adding the glue does not change any coroutine layout.

Generated code, per coroutine `i` and suspension state:

```rust
unsafe fn __ser_0(p: *const u8) -> Result<Value> { unsafe {
    match ::core::ptr::read(p.add(160) as *const u8) {       // tag
        5 => {
            let mut m = Map::new();
            m.insert("state".into(), 5u64.into());
            m.insert("at".into(), "src/main.rs:38:43: 38:48".into());
            m.insert("body".into(), ser_array::<u8, 64>(p.add(80))?);
            m.insert("quick".into(), ser_field::<u64>(p.add(144))?);
            m.insert("a".into(), ser_field::<u64>(p.add(152))?);
            m.insert("__awaitee".into(), __ser_2(p.add(168))?);   // nested async fn frame
            Ok(m.into())
        }
        3 => Err("...state 3 at src/main.rs:33:6 is not serializable: __awaitee: \
                  `Ephemeral<{async block}>` holds an unnameable type".into()),
        ...
}}}
// __de_i writes each field at its offset, then the tag.
pub static GLUE: &[Entry] = &[Entry { type_name, size, align, ser: __ser_0, de: __de_0 }, ...];
```

Rules:

- A field goes through `T: Serialize + Deserialize` if the driver can prove the impl (trait query against `serde_core`).
- `[T; N]` goes through a slice, because serde stops at N = 32.
- A nested coroutine recurses into its own glue.
- A state with a reference, raw pointer, function, trait object, unnameable type, missing serde impl, or unnamed `bool` (likely a drop flag) gets an error arm with the reason. This is a precise, per-state version of the `Durable` check, but it reports at run time.
- `async fn` bodies skip upvars after the first resume, because their arguments were moved into locals. Other coroutines also serialize their upvars.

Runtime: `glue::serialize(GLUE, &task)` / `glue::deserialize(GLUE, || wf(..), cap, &json)`. Parking serializes the frame and then drops the task normally, which frees its heap data. Resume builds a fresh frame: it writes the live fields of the state and the tag, and leaves every other byte uninitialized.

### Results

- `profile` holds a `Vec<String>` and a `String` (heap, not `Durable`) across two slow calls. It finishes in a third process with 60. The snapshot is readable JSON, nested frame included.
- `classify` works the same way (49).
- `leaky` is refused: `s: &[u8] holds a reference`. `racy` is refused by the ephemeral check.
- The rejection messages for `triage` (arena handles), `classify` state 3 (ephemeral) and `leaky` are exactly the answers the Option B lint needs.

What it changes in the design:

- Heap data can live across slow calls, so the arena, `ARef`, the frozen-`Vec` scheme and `Durable` are no longer needed for that.
- The format is keyed by state number and local name, not offsets. In principle a recompile that keeps a function's await points and locals could still load old snapshots. Not tested yet.
- Snapshots no longer depend on the target architecture.

Open:

- Turn the per-state errors into compile errors at slow points (the lint): needs "which states are slow".
- Not checked under Miri. Resume leaves non-live bytes uninitialized. That should be fine for `async fn` (only the live fields and the tag are read), but it is unverified. Under Miri the build flags differ, so the glue would need to be generated for Miri's build.
- Generic coroutines, closures and `async` blocks with upvars held by a parent frame, drop flags.
- Types are named by path from the glue module, so private types in other modules fail to compile in pass 2.
- A failed deserialize leaks the fields written before the failure.
- Glue from a different build: `Entry` checks size and alignment only. State numbering is not versioned yet.
- The driver pins the project to rustc internals, which change on every nightly (several APIs changed between the 2025 docs and this nightly).

## Known holes and open questions

Holes in the structural check:

- Function pointers get auto traits for free. Negative impls can be written per arity with a macro, but not for higher-ranked ones like `for<'a> fn(&'a T)`.
- `&'static str` is a common thing to hold across an await, and it is an absolute address under PIE/ASLR, so it is `!Durable`. A `dstr!("...")` could store an offset from a known symbol in the `.so` instead.
- `PhantomData<&'a T>` is `!Durable` even though it holds no pointer, so some iterator and guard types fail for no real reason.
- A pointer cast to `usize` passes the check. That needs unsafe code to use, so it is acceptable.
- Pointers from outside the arena into it are invisible to the check (see the safety contract).
- Process-local identity that is not a pointer passes structurally: fds, sockets, pids, indices into process-wide tables, monotonic `Instant`s. Dropping a restored fd closes an unrelated fd in the new process. Covered by explicit negative impls for std types; the list cannot be complete.
- The frame copy includes padding bytes, which are uninitialized. Reading them as `&[u8]` is UB in theory. Miri does not flag the in-memory round trip (a plain memcpy keeps them uninit), but writing them to a file would count as a read.
- Aliasing between `RelRef::get` and `get_pin` on the same target is not tracked (see the brand section).
- PoC-only: `ForceDurable` is public and safe to construct, so safe code can bypass the check. It must become unsafe or demo-only.
- Resume trusts the snapshot bytes. Today the binary maps a hand-picked tag to a workflow type (a wrong tag means a wrong type). A partial or corrupted write, or a tampered store, means arbitrary memory access. Needs: a type id generated from the workflow, a checksum, atomic writes (temp file + rename, or a DB transaction), and a store treated as trusted.

Runtime gaps:

- The arena is fixed-size and panics when full. Everything in it is relocatable, so it could grow between polls. Growing during a poll needs chunks or reserved virtual memory.
- Two processes can resume the same snapshot and run it twice. Needs a lease or ownership row in the store.
- What happens to a task that panics mid-poll?
- Parked tasks run old code to completion; a bug fix cannot reach them. Needs a drain policy, or explicit migration points where state is serialized instead of copied.

Answered:

- Do user auto traits propagate through `async fn` witnesses like `Send`? Yes, with the same diagnostics (v0).
- How does `DVec` grow? Only when `T: Unpin` (v1).

Open questions:

- Should a non-slow `Pending` with an eviction pending just wait, or should eviction only ever happen right after a slow park?
- How long to keep old `.so` versions, and what to do with a snapshot whose `.so` is gone.
- Is Option A enough in practice, or is the MIR lint worth building?
- Branded token for arena access, and whether it can also fix the aliasing hole.
- How should the bump arena reclaim memory (abandoned `DVec` buffers, dead values)? And should arena values run destructors?

## PoC status

Code: `src/lib.rs` (trait, leaves, `Ephemeral`, `run`), `src/arena.rs` (`Task`, `ABox`, `DVec`, `RelRef`), `src/glue.rs` (serialized-frame runtime), `src/main.rs` (`poc` binary), `src/compile_checks.rs` (doctests), `tests/roundtrip.rs`, `driver/` (rustc_driver), `build-glue.sh`, `demo.sh`.

```
cargo test                    # compile checks + in-process round trip
./build-glue.sh               # two-pass build with serde glue (demo.sh runs it)
./demo.sh                     # multi-process demo
MIRIFLAGS=-Zmiri-disable-isolation cargo miri test --test roundtrip
```

### v0: done

Native `async fn`, frame in a `Box`, hand-written poll loop with `Waker::noop()`, snapshot checked against a hash of the executable.

- `poc start classify 7` → park → `resume` → park on the nested fn's slow call → `resume` → 49. Three processes, frame at a different address each time.
- Compile checks: borrows, `String`, `&'static str`, and hidden temporaries (`for x in v.iter()`, `match &x`, `f(&x, slow().await)`) held across a slow await are rejected. Dead borrows and nested async fns are accepted.
- Ephemeral: a slow call inside a section fires the leaf's assert.
- Negative controls: forcing a frame borrow past the check (`ForceDurable`) gives a segfault, bus error or wrong answer on resume. The `join(slow, ephemeral)` case led to the eviction-time check (Option A section).

### v1: done

- Fixed-size task arena (64 KiB) with the header, root frame and task data. Eviction copies only the used prefix (264 B for `classify`, 640–1152 B for `triage`).
- `ABox`, `DVec` (grows iff `T: Unpin`) and `RelRef` (self-relative, set through `Pin`).
- `triage` workflow: a `DVec<u64>` that grows before and after resume, an `ABox<Note>`, and a fixed-capacity `DVec<RelRef<Note>>` pointing at it, all held across two slow awaits. Finishes in a third process with 574. The `RelRef` resolves to the same offset in each new arena.
- Negative impls for OS handles and `Instant`, with compile checks.
- `tests/roundtrip.rs` passes under Miri (evict → scribble over old bytes → resume into a fresh arena, twice), apart from the expected warning from `with_exposed_provenance`.

### Serialized-frame experiment: done

See [Serialized frames](#serialized-frames-rustc_driver-experiment). `./build-glue.sh`, then `poc sstart profile 7` / `poc sresume`.

### v2: next

- Workflow as `cdylib` with the C ABI vtable, stored as `workflow_<hash>.so`, loaded with `dlopen`. The tokio-in-cdylib problem is set aside for now.
- SQLite store behind a `Store` trait.
- tokio-hosted tasks, `step(f)` for non-durable work, eviction policy for the fast/slow split.
- Maybe: branded token for arena access.
