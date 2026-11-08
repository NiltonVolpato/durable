# Raw copy: `Durable` futures in a relocatable arena

The original approach ([overview](README.md)): a parked task is stored as raw bytes. Its frame and data live in one arena, and everything in the arena refers to other things in it by offset, so the bytes can be copied to storage and back to any address, in another process. The compiler checks, through an auto trait, that nothing in a parked frame is an absolute pointer.

Code (in the `poc/` crate): `poc/src/lib.rs` (trait, `Ephemeral`, `run`), `poc/src/arena.rs` (`Task`, `ABox`, `DVec`, `RelRef`), `poc start|resume`, `poc/tests/roundtrip.rs`. Results: [status](status.md#v0) and [v1](status.md#v1).

Snapshots are only valid for the exact build that wrote them; see [binary identity](runtime.md#binary-identity).

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

Built, for the serialized path: see [the slow-await lint](serialized-frames.md#compile-time-lint). The original plan, for `Durable`:

`tcx.coroutine_layout(..)` lists, for each suspension variant, the saved locals and their types (`variant_fields`, `field_tys`). The awaited future is one of those saved locals. Define a property `DurableAtSlow`:

- A `Slow<T>` leaf is `DurableAtSlow`.
- A coroutine variant is a slow point if its awaited future is `DurableAtSlow`. At every slow point, all other saved locals must be `Durable`, using the auto trait as the per-type test.
- A coroutine is `DurableAtSlow` if all its slow points pass and it has at least one.
- Hand-written combinators (`join`, `select`) cannot be inspected. They must be fully `Durable`, or come in annotated versions.

With these rules, any await on something that can reach a slow leaf is a slow point. So if a slow leaf returned `Pending` during a poll, every frame above it is at a checked variant. Fast paths need no annotation.

A proc macro is not a good replacement. It sees names, not liveness or types, and misses hidden temporaries held across an await: the iterator in `for x in v.iter() { slow().await }`, the scrutinee of `match &foo`, the `&x` in `f(&x, slow().await)`. Each miss is an absolute pointer restored in a new process.

## Holes and open questions

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
- The arena is fixed-size and panics when full. Everything in it is relocatable, so it could grow between polls. Growing during a poll needs chunks or reserved virtual memory.

Answered:

- Do user auto traits propagate through `async fn` witnesses like `Send`? Yes, with the same diagnostics (v0).
- How does `DVec` grow? Only when `T: Unpin` (v1).

Open questions:

- Option A vs. the lint: the lint exists now (serialized path); it could also check `Durable` at slow points here, which would remove `Ephemeral`.
- Branded token for arena access, and whether it can also fix the aliasing hole.
- How should the bump arena reclaim memory (abandoned `DVec` buffers, dead values)? And should arena values run destructors?
