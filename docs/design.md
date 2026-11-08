# Durable native futures in Rust

Nov 8, 2026 · @Nilton Volpato

## Summary

A Rust async runtime where a native `async fn` can be suspended at a slow call, have its frame copied to storage, and be resumed later in another process. There is no replay and no hand-written state machine.

The core ideas:

- A `Durable` auto trait marks types that hold no absolute pointers. The compiler checks it structurally, the same way it checks `Send` on async blocks.
- Each task lives in one contiguous arena. Pointers inside the arena are self-relative (`RelRef`), so the whole arena can be moved with a memcpy.
- Only the state live at a slow call has to be durable. Fast sections can use borrows, tokio I/O and the heap freely.
- Frame bytes are only valid for the code that wrote them. Workflows are built as shared libraries, named by the hash of the `.so`, and loaded with `dlopen` on resume.

It needs nightly (`auto_traits`, `negative_impls`).

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

/// Safety: the type holds no absolute addresses, and nothing outside
/// the task arena holds the address of a value of this type.
pub unsafe auto trait Durable {}

impl<T: ?Sized> !Durable for &T {}
impl<T: ?Sized> !Durable for &mut T {}
impl<T: ?Sized> !Durable for *const T {}
impl<T: ?Sized> !Durable for *mut T {}
impl<T: ?Sized> !Durable for core::ptr::NonNull<T> {}
```

Because `NonNull` is excluded, `Box`, `Vec`, `String`, `Rc` and `Arc` are excluded too. `Waker` holds a vtable and a data pointer, so it is excluded.

Auto traits propagate through a coroutine's captured values and the values it holds across awaits. So `impl Future + Durable` makes the compiler check a native `async fn` with no macro.

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

Each task owns one contiguous arena. The root future's frame is allocated in it, and so is any heap data the task keeps across slow calls. Eviction copies the used part of the arena. Resume copies it into a new arena at any address.

Pointers inside the arena are self-relative:

```rust
/// Offset from &self to the target. Valid only inside one task arena.
pub struct RelRef<'a, T> {
    off: isize,
    _brand: PhantomData<fn(&'a ()) -> &'a ()>, // invariant arena brand
    _pin: PhantomPinned,
    _ty: PhantomData<T>,  // not PhantomData<&T>, which would be !Durable
}
unsafe impl<'a, T: Durable> Durable for RelRef<'a, T> {}
```

The brand lifetime ties a `RelRef` to its arena so it cannot escape or point into another arena. Several arenas can be live on one thread at once, and deref needs no context.

Costs of self-relative pointers:

- A `RelRef` is `!Unpin`. It must be built in place through `Pin<&mut Self>`, at its final address. A constructor that returns it by value gives a wrong offset after the move.
- It cannot be `Copy` or `Clone` in the usual way, since a copy sits at a different address. Cloning means recomputing the offset at the destination.
- An arena container that reallocates (a `DVec` growing) memcpys its elements. Self-relative pointers stored in those elements break. Options: no growth after first use, a fix-up pass on growth, or store indices instead of pointers in elements.

The compiler's own borrows (`let s = &buf; x.await;`) are absolute pointers and cannot be made relative. Rust has no relative pointer type, even on nightly. So `RelRef` only covers user data. Compiler borrows must not be live at a slow call (next section).

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

Cost: one `ephemeral(...)` per fast section, and a slow call inside one is caught at run time, not compile time.

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

- `Slow<T>` / `wait_event(id)`: on poll, look up the result in the store. If missing, register `(task_id, event_id)` and the current waker in a table outside the arena, set a "parked on slow" flag for this poll, and return `Pending`. The leaf itself stores no `Waker`.
- `sleep_until(t)`: same, with a wall-clock deadline stored as an integer.
- `step(f)`: runs an ordinary tokio future as a separate task keyed by ID and saves its output (which must be `Durable` or serializable) in the store. The frame holds only the ID. After a crash the step runs again, so steps are at-least-once.

Compiler-generated frames never store the `Context`, so the only wakers are in leaves, and slow leaves keep theirs outside the arena.

Task lifecycle:

1. A request starts as an ordinary in-memory task: a tokio task wrapping the arena and the root future.
2. After each poll that returns `Pending` with the "parked on slow" flag set, the task may be evicted. Policy: right away, after a delay, or under memory pressure. The fast path never reaches this point.
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

## Known holes and open questions

Holes in the structural check:

- Function pointers get auto traits for free. Negative impls can be written per arity with a macro, but not for higher-ranked ones like `for<'a> fn(&'a T)`.
- `&'static str` is a common thing to hold across an await, and it is an absolute address under PIE/ASLR, so it is `!Durable`. A `dstr!("...")` could store an offset from a known symbol in the `.so` instead.
- `PhantomData<&'a T>` is `!Durable` even though it holds no pointer, so some iterator and guard types fail for no real reason.
- A pointer cast to `usize` passes the check. That needs unsafe code to use, so it is acceptable.
- Pointers from outside the arena into it are invisible to the check (see the safety contract).
- The frame copy includes padding bytes, which are uninitialized. Reading them as `&[u8]` is UB in theory, and Miri will flag it.

Open questions:

- Do user-defined auto traits propagate through `async fn` witnesses exactly like `Send`, with negative impls applied to borrows held across awaits? This is the first thing the PoC must confirm.
- Should a non-slow `Pending` with an eviction pending just wait, or should eviction only ever happen right after a slow park?
- How does `DVec` grow: no growth, fix-up, or index-based elements?
- How long to keep old `.so` versions, and what to do with a snapshot whose `.so` is gone.
- Is Option A enough in practice, or is the MIR lint worth building?

## PoC plan

Goal of v0: show that a native `async fn` can be suspended at a slow call, written to a file, and finished by a different process where the frame sits at a different address. Plus one compile-fail and one runtime-fail case.

### v0 shortcuts

- Nightly, `auto_traits` + `negative_impls`. One binary, no `.so`, no SQLite, no tokio.
- A hand-written poll loop with `Waker::noop()`.
- No arena yet. The frame lives in a `Box<F>` and holds only plain data (`u64`, `[u8; N]`).
- Build ID = hash of `/proc/self/exe`, checked on resume.
- Option A (`Ephemeral` + thread-local counter) for fast sections.
- Ignore padding UB and the fn-pointer hole.

### v0 code sketch (not compiled)

```rust
thread_local! {
    static PARKED: Cell<Option<u64>> = Cell::new(None);
    static EPHEMERAL: Cell<usize> = Cell::new(0);
}

pub struct Slow { id: u64 }
impl Future for Slow {
    type Output = u64;
    fn poll(self: Pin<&mut Self>, _: &mut Context) -> Poll<u64> {
        assert_eq!(EPHEMERAL.get(), 0, "slow call inside ephemeral section");
        match std::fs::read_to_string(format!("results/{}", self.id)) {
            Ok(s) => Poll::Ready(s.trim().parse().unwrap()),
            Err(_) => { PARKED.set(Some(self.id)); Poll::Pending }
        }
    }
}

fn run<F: Future + Durable>(mut fut: Pin<Box<F>>, path: &str) -> Option<F::Output> {
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        PARKED.set(None);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return Some(v),
            Poll::Pending if PARKED.get().is_some() => {
                let p = &*fut as *const F as *const u8;
                let bytes = unsafe { std::slice::from_raw_parts(p, size_of::<F>()) };
                write_snapshot(path, build_id(), bytes);
                std::mem::forget(fut); // ownership is now on disk
                return None;
            }
            Poll::Pending => continue, // non-slow yield: poll again
        }
    }
}

/// `ctor` is only used to infer the unnameable F from the async fn item.
fn resume<A, F: Future + Durable>(_ctor: impl Fn(A) -> F, bytes: &[u8]) -> Pin<Box<F>> {
    assert_eq!(bytes.len(), size_of::<F>());
    let mut b = Box::<F>::new_uninit();
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), b.as_mut_ptr() as *mut u8, bytes.len());
        Box::into_pin(b.assume_init())
    }
}

async fn classify(req_id: u64, body: [u8; 64]) -> u64 {
    let quick = ephemeral(async {
        let s = &body[..];       // borrow, allowed inside ephemeral
        yield_once().await;      // a non-slow await
        s.iter().map(|&b| b as u64).sum::<u64>()
    }).await;
    if quick % 2 == 0 { return quick; }
    let a = Slow { id: req_id }.await;
    let b = Slow { id: req_id + 1000 }.await;
    quick + a + b
}
```

### v0 demo

1. `poc start 7`: prints the frame address, writes `snap.bin`, exits.
2. `echo 40 > results/7; poc resume`: prints a different frame address, passes the first slow call, parks on the second, writes the snapshot again.
3. `echo 2 > results/1007; poc resume`: prints the final sum.
4. Build as PIE so ASLR moves addresses between runs.
5. Compile-fail: hold `let s = &body;` across a `Slow` await in the outer fn. `run`'s `F: Durable` bound should reject it, and the error should name the borrow.
6. Runtime-fail: put a `Slow` await inside `ephemeral`. The assert fires.

Steps 5 and 6 test the main open question: that user auto traits propagate through `async fn` witnesses.

### v1

- A fixed-size task arena. The root future and task data are allocated in it.
- `RelRef<'a, T>`, self-relative, built in place through `Pin`.
- One arena `DVec` (no growth after first slow call) held across a slow await, holding a `RelRef` to another arena value. Shows heap data surviving the move.

### v2

- Workflow as `cdylib` with the C ABI vtable, stored as `workflow_<hash>.so`, loaded with `dlopen`.
- SQLite store behind a `Store` trait.
- tokio-hosted tasks, `step(f)` for non-durable work, eviction policy for the fast/slow split.
