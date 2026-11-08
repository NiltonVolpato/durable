# Serialized frames: serde glue from the compiler's coroutine layouts

The main path ([overview](README.md)): a parked task is stored field by field. For every value live at the parked await, in every frame from the workflow root down to the slow leaf, the glue writes the value through serde. Resume builds a fresh frame and writes the values back. Heap data is fine; what cannot be serialized is rejected at compile time.

Code: `driver/` (`durable-driver`). In the alpha: `durable/src/glue.rs` and `cargo durable`. In the PoC: `poc/src/glue.rs`, `poc/build-glue.sh`, `poc sstart|sresume`, `probe/`, `steps/`, `poc/tests/serde_roundtrip.rs`.

## Why a compiler pass

Serializing a frame needs, for each suspension point, the live locals, their types and their offsets. Only the compiler knows those, and the coroutine type cannot be named in source.

- A proc macro sees names, not liveness or types. It misses hidden temporaries held across an await: the iterator in `for x in v.iter()`, the scrutinee of `match &foo`, the `&x` in `f(&x, slow().await)`.
- `coroutine_clone` derives `Clone` only for movable coroutines. `async fn` futures are immovable and get no impl (checked, E0277). The compiler cannot derive arbitrary traits structurally.

`durable-driver` is rustc plus a pass after analysis, run as `RUSTC_WORKSPACE_WRAPPER`. It needs the `rustc-dev` and `llvm-tools` components (in `rust-toolchain.toml`).

## What the driver reads

- **Roots:** `async fn`s marked `#[durable::workflow]`. In the alpha, the attribute macro adds a hidden `const` naming the function, which the driver finds. The PoC used a tool attribute (`#![register_tool(durable)]`) instead.
- **Frames reachable from the roots**, with concrete generic arguments, from any crate:
  - coroutines;
  - closures (their captures);
  - structs implementing `unsafe trait Structural` (combinators such as `Join2`, `Select2`).
- Per coroutine:
  - `tcx.coroutine_layout(def_id, args)`: per state, the saved locals, their types and debuginfo names.
  - `tcx.layout_of(ty)`: offsets per state, the tag, size and alignment.
  - optimized MIR: which drop flag guards which local (`driver/src/mir.rs`). The pattern is `switchInt(flag) -> [0: .., otherwise: bbX]`, where `bbX` drops a saved local.
- `rustc_const_eval::util::type_name`, which matches `std::any::type_name::<F>()` at run time.

`DURABLE_DUMP=path` writes a readable dump. `driver/decode_snapshot.py` decoded a real raw-copy `snap.bin` using only the dump, which validated the offsets.

## Glue generation

`poc/build-glue.sh [package crate main_file]` runs two builds (in the alpha, `cargo durable` does the same, see [alpha](alpha.md#setup)):

1. Pass 1 builds with an empty glue table. The driver writes the glue.
2. Pass 2 includes the glue (in the PoC, `poc/build.rs` copies it to `$OUT_DIR/durable_glue.rs`; in the alpha, `durable::glue!()` includes it). The driver regenerates it and fails the build if it differs, which confirms that adding the glue does not change any layout.

Generated code, per frame `i`:

```rust
unsafe fn __ser_0(p: *const u8) -> Result<Value> { unsafe {
    match ::core::ptr::read(p.add(160) as *const u8) {       // tag
        5 => {
            let mut m = Map::new();
            m.insert("state".into(), 5u64.into());
            m.insert("sig".into(), "__awaitee:<frame>,a:u64,body:[u8; 64],quick:u64".into());
            m.insert("at".into(), "src/main.rs:38:43: 38:48".into());
            m.insert("body".into(), ser_array::<u8, 64>(p.add(80))?);
            m.insert("quick".into(), ser_field::<u64>(p.add(144))?);
            m.insert("a".into(), ser_field::<u64>(p.add(152))?);
            m.insert("__awaitee".into(), __ser_2(p.add(168))?);   // nested frame
            Ok(m.into())
        }
        ...
}}}
// __de_i checks `sig`, writes each value at its offset, then the tag.
pub static GLUE: &[Entry] = &[Entry { type_name, size, align, ser: __ser_0, de: __de_0 }, ...];
```

Rules:

- A value goes through `T: Serialize + Deserialize` when the driver can prove the impl (trait query against `serde_core`). `[T; N]` goes through a slice, because serde stops at N = 32.
- A value that is itself a frame recurses into that frame's glue:
  - a coroutine: one object per state;
  - a closure: its captures, `{"c0": 7}`;
  - a `Structural` struct: its fields.
- Coroutine states:
  - state 0 (unresumed) stores the upvars;
  - state 1 (returned) stores nothing, e.g. a finished branch of a join;
  - states 3+ store the saved locals, plus the upvars for non-`async fn` coroutines. `async fn` bodies move their arguments into locals on the first resume.
- A value guarded by a drop flag is stored only while initialized: `"note.init": true, "note": ...`.
- A state that holds something else gets an error arm with the reason: a reference, raw pointer, function, trait object, unnameable type, missing serde impl, or an unpaired flag. The lint turns these into compile errors at slow awaits.

Runtime: `glue::serialize(GLUE, &task)` / `glue::deserialize(GLUE, || wf(..), cap, &json)`. Parking serializes the root frame and then drops the task normally, which frees its heap data. Resume marks the root slot uninitialized, writes the live values and the tag, and leaves every other byte uninitialized.

## Compile-time lint

At compile time, the driver rejects any slow await whose live state cannot be serialized.

- **Slow leaves** implement `pub trait SlowLeaf {}` (`Slow`, `SleepUntil`).
- **Slow frames** (fixpoint over the reachable frames). A frame can park if it awaits one of:
  - a slow leaf;
  - a frame that can park (a coroutine, or a `Structural` combinator with such a field);
  - a type the driver cannot see into that has a slow leaf or such a frame nested anywhere inside it (`Timeout<Request<_>>`, `tokio::join!`'s closure, `Ephemeral<{async block}>`).
- **Destructors.** Parking drops the frame, so live values' destructors run at park and again on the resumed copy. At slow awaits the lint also rejects any live value with a user `Drop` impl anywhere in its type (fields, std containers' type arguments, closure captures, an unstarted future's arguments) unless that type implements `durable::DropOnPark`; its destructor can check `durable::is_parking()`, which the runtime sets only while dropping a frame it has stored. std/core/alloc types are trusted; `ManuallyDrop` and `PhantomData` are skipped. Coroutines under the awaitee are checked at their own slow states. Only in the alpha runtime (`durable`); the PoC has no marker trait.
- **Shared pointers.** `Rc`, `Arc` and `Weak` anywhere inside a value are rejected, whatever their serde impls: a snapshot cannot keep the sharing.
- **The check:** at each slow state, every live value must pass the glue rules, and so must every closure or combinator it holds, recursively.
- **Not seen:** type-erased futures (`Pin<Box<dyn Future>>`) count as fast. A slow call hidden there is only caught at run time, when serialization fails.
- **Fast awaits are not checked,** so borrows and tokio I/O across them need no annotation. Code not reachable from a root (the runtime, raw-copy workflows) is not checked.

```
error: `f` cannot be serialized at this slow await
  --> examples/lint_fail.rs:52:14
52 |     slow(id).await + n
   |              ^^^^^ the task can park here
note: `f` is live across the await: `std::fs::File` does not implement Serialize + DeserializeOwned
50 |     let f = std::fs::File::open("/dev/null").unwrap();
   = help: make it serializable, or end it before the await
   = help: values with a destructor stay live until the end of their scope; if `f` is not
           used after this point, `drop` it or close its block before the await
```

`driver/lint-check.sh` builds `poc/examples/lint_fail.rs` and gets 6 errors (the alpha's equivalent is `apps/unsupported/check.sh`):

- a borrow;
- a hidden `for` iterator;
- a type without serde impls;
- a slow call inside `ephemeral`, reported at the outer await and at the borrow inside the section;
- a `File` kept alive only for its destructor.

## Loading snapshots into a changed build

Each serialized state carries a `sig`: its live locals and their types, sorted. Deserialize requires an exact match. Nested frames carry their own. `driver/evolve-demo.sh` (`EVOLVE=1|2|3` selects a version of `evolve`):

- **v1 → v2 (a new slow step after the parked one):** the frame grew from 80 to 88 B and `__awaitee` moved from offset 64 to 80. The v1 snapshot still loads by name, runs v2's new code, parks at the new step, and finishes (55).
- **v1 → v3 (a new local live at the parked await):** refused with a diff of the two signatures.

Limits:

- States are numbered by await order. An await inserted *before* the parked one shifts the numbers. If the shifted state has the same signature, the task resumes at the wrong await. A stable key per await (a user label, or the callee plus its position) would fix this.
- Only the shape of the state is checked, not changes in behavior.

## Miri

`driver/serde-test.sh` runs `poc/tests/serde_roundtrip.rs` natively and under Miri, each with glue generated using the matching flags. For Miri these are its defaults plus `--cfg miri`; the glue turned out identical to the native debug glue.

Covered:

- heap data across two parks;
- a nested frame;
- a 64-byte array after an ephemeral section;
- a join with one branch finished;
- a drop flag set and cleared;
- a generic helper with a closure;
- dropping a rebuilt frame without finishing it.

Because resume leaves non-live bytes uninitialized, any read of a byte the glue did not write is UB under Miri. Results:

- No UB, no leaks.
- Negative control: glue with one field removed gives "reading memory ... but memory is uninitialized".

## Realistic probe on tokio

`probe/` is a tokio binary (current-thread runtime). `steps/` is a small SDK crate (`wait_for_review`, `check`, generic `retry`). `probe/run.sh <id>` drives the workflow through every slow point, one process per step:

```rust
#[durable::workflow]
async fn moderate(req_id: u64, addr: String) -> u64 {
    let quick = fast_score(&addr, req_id).await;          // real TcpStream I/O, borrows
    if quick < 10 { return quick; }
    let note = format!("req {req_id} scored {quick}");
    if quick % 2 == 0 { audit(note); }                    // conditional move: drop flag
    let a = wait_for_review(req_id).await;                // async fn from another crate
    let (b, c) = join2(wait_for_review(req_id + 1), wait_for_review(req_id + 2)).await;
    let d = match select2(wait_for_review(req_id + 3), sleep_until(now() + 2)).await {
        Either::Left(v) => v,
        Either::Right(()) => 0,                           // durable timer won
    };
    let e = retry(3, move |attempt| check(req_id + 4 + attempt as u64)).await;  // generic + closure
    quick + a + b + c + d + e
}
```

What broke first, and what fixed it:

- **Cross-crate slow calls were invisible.** The lint only knew local coroutines, so awaits on `durable_steps` futures counted as fast (unsound). Fix: collect frames reachable from workflow roots, from any crate. The glue serializes the foreign frame's private locals too.
- **False positive on the runtime.** Scanning generic arguments made `host<moderate>` look slow. Fixed by the same change: only frames reachable from roots are checked.
- **Generic coroutines** (`retry::<{closure}, {check}>`): field types instantiated and normalized.
- **Combinators** (`Join2`, `Select2` hold coroutine types, so no serde): `Structural`.
- **Closures:** serialized as their captures. The closure's code comes from its type.
- **Unresumed and returned states**, e.g. a join with one branch done.
- **Drop flags:** paired with their value from MIR. Both cases run (odd and even ids).
- **Slow calls inside opaque wrappers** (`Ephemeral<{async block}>`).

Results: `probe/run.sh 7` → 65 and `probe/run.sh 8` → 66, through 6 parks. These include a join with one branch finished, a deadline expiring between processes, and retry attempt 2 resumed.

Ergonomics finding: values with a destructor stay live until the end of their scope. `addr` is used only in the fast part but appears in every snapshot. A `TcpStream` kept that way would be a lint error. The lint suggests `drop(x)` or a block.

## Open

- Type-erased futures (`Pin<Box<dyn Future>>`) at slow points: invisible to the lint. Serializing them would need a registry keyed by the concrete type.
- Enums holding frames (`Option<{async fn}>`, `MaybeDone`-style combinators): `Structural` covers structs only.
- Combinators must opt in to `Structural`. tokio's own (`tokio::join!`, `select!`, `timeout`) cannot be used at slow points.
- Types are named by path from the glue module, so private types in other modules fail to compile in pass 2. Foreign private types work when they sit inside frames that are recursed into.
- A failed deserialize leaks the values written before the failure.
- Stable state keys (see snapshot versioning).
- `cargo durable build` hides the two passes, but workflow crates still compile twice when the glue changes.
- The driver pins the project to rustc internals, which change on every nightly. Several APIs had already changed on the nightly used here.
