//! Compile-time checks of the `Durable` auto trait through `async fn`.
//! Run with `cargo test --doc`.
//!
//! ## Accepted
//!
//! Plain data across a slow await, borrows confined to an ephemeral section:
//! ```
//! use durable_poc::*;
//! async fn ok(id: u64, body: [u8; 64]) -> u64 {
//!     let quick = ephemeral(async {
//!         let s = &body[..];
//!         yield_once().await;
//!         s.iter().map(|&b| b as u64).sum::<u64>()
//!     }).await;
//!     quick + slow(id).await
//! }
//! require_durable(ok(1, [0; 64]));
//! ```
//!
//! A borrow that is dead before the slow await (MIR liveness is precise):
//! ```
//! use durable_poc::*;
//! async fn ok(id: u64, body: [u8; 64]) -> u64 {
//!     let s = &body;
//!     let n = s.len() as u64;
//!     n + slow(id).await
//! }
//! require_durable(ok(1, [0; 64]));
//! ```
//!
//! Nested async fns compose:
//! ```
//! use durable_poc::*;
//! async fn inner(id: u64) -> u64 { let k = [3u64; 4]; slow(id).await + k[0] }
//! async fn outer(id: u64) -> u64 { inner(id).await + inner(id + 1).await }
//! require_durable(outer(1));
//! ```
//!
//! HOLE: fn pointers (code addresses under ASLR) pass:
//! ```
//! use durable_poc::*;
//! fn double(x: u64) -> u64 { x * 2 }
//! async fn hole(id: u64) -> u64 {
//!     let f: fn(u64) -> u64 = double;
//!     f(slow(id).await)
//! }
//! require_durable(hole(1));
//! ```
//!
//! Arena data across slow awaits:
//! ```
//! use durable_poc::*;
//! struct Note { weight: u64 }
//! async fn ok(id: u64) -> u64 {
//!     let mut v: DVec<u64> = DVec::new();
//!     v.push(1);
//!     let note = ABox::new(Note { weight: 3 });
//!     let mut refs: DVec<RelRef<Note>> = DVec::with_capacity(1);
//!     refs.push_pinned(RelRef::null());
//!     refs.get_pin(0).set(note.get());
//!     let a = slow(id).await;
//!     v.get(0) + a + refs.get(0).get().unwrap().weight
//! }
//! require_durable(ok(1));
//! ```
//!
//! ## Rejected
//!
//! OS handles (explicit negative impls):
//! ```compile_fail,E0277
//! use durable_poc::*;
//! async fn bad(id: u64) -> u64 {
//!     let f = std::fs::File::open("/dev/null").unwrap();
//!     let r = slow(id).await;
//!     drop(f);
//!     r
//! }
//! require_durable(bad(1));
//! ```
//! ```compile_fail,E0277
//! use durable_poc::*;
//! async fn bad(id: u64) -> u64 {
//!     let s = std::net::TcpStream::connect("127.0.0.1:1").unwrap();
//!     let r = slow(id).await;
//!     drop(s);
//!     r
//! }
//! require_durable(bad(1));
//! ```
//! ```compile_fail,E0277
//! use durable_poc::*;
//! async fn bad(id: u64) -> u64 {
//!     let t = std::time::Instant::now();
//!     let r = slow(id).await;
//!     r + t.elapsed().as_secs()
//! }
//! require_durable(bad(1));
//! ```
//!
//! A `&T` into the arena held across a slow await (the handle is fine, the
//! borrow is not):
//! ```compile_fail,E0277
//! use durable_poc::*;
//! async fn bad(id: u64) -> u64 {
//!     let b = ABox::new(5u64);
//!     let r: &u64 = b.get();
//!     slow(id).await + *r
//! }
//! require_durable(bad(1));
//! ```
//!
//! A growable push of `!Unpin` elements does not exist:
//! ```compile_fail,E0599
//! use durable_poc::*;
//! let mut refs: DVec<RelRef<u64>> = DVec::new();
//! refs.push(RelRef::null());
//! ```
//!
//! A `RelRef` cannot be copied out of the arena:
//! ```compile_fail,E0507
//! use durable_poc::*;
//! fn f(refs: &DVec<RelRef<u64>>) -> RelRef<u64> { *refs.get(0) }
//! ```
//!
//!
//! Borrow of the frame held across a slow await (demo step 5):
//! ```compile_fail,E0277
//! use durable_poc::*;
//! async fn bad(id: u64, body: [u8; 64]) -> u64 {
//!     let s = &body;
//!     let a = slow(id).await;
//!     s[0] as u64 + a
//! }
//! require_durable(bad(1, [0; 64]));
//! ```
//!
//! Hidden temporary: the slice iterator in a `for` loop:
//! ```compile_fail,E0277
//! use durable_poc::*;
//! async fn bad(ids: [u64; 4]) -> u64 {
//!     let mut t = 0;
//!     for id in ids.iter() { t += slow(*id).await; }
//!     t
//! }
//! require_durable(bad([1; 4]));
//! ```
//!
//! Hidden temporary: `match &x` scrutinee:
//! ```compile_fail,E0277
//! use durable_poc::*;
//! async fn bad(x: Option<u64>) -> u64 {
//!     match &x {
//!         Some(v) => slow(1).await + *v,
//!         None => 0,
//!     }
//! }
//! require_durable(bad(None));
//! ```
//!
//! Hidden temporary: `&x` argument evaluated before the await:
//! ```compile_fail,E0277
//! use durable_poc::*;
//! fn f(a: &u64, b: u64) -> u64 { *a + b }
//! async fn bad(x: u64) -> u64 { f(&x, slow(1).await) }
//! require_durable(bad(1));
//! ```
//!
//! Heap data across a slow await:
//! ```compile_fail,E0277
//! use durable_poc::*;
//! async fn bad(id: u64) -> u64 {
//!     let s = String::from("x");
//!     slow(id).await + s.len() as u64
//! }
//! require_durable(bad(1));
//! ```
//!
//! `&'static str` across a slow await:
//! ```compile_fail,E0277
//! use durable_poc::*;
//! async fn bad(id: u64) -> u64 {
//!     let s: &'static str = "x";
//!     slow(id).await + s.len() as u64
//! }
//! require_durable(bad(1));
//! ```
//!
//! Over-approximation: a borrow across a *fast* await outside `ephemeral`
//! poisons the whole future, even though no slow call sees it:
//! ```compile_fail,E0277
//! use durable_poc::*;
//! async fn bad(id: u64, body: [u8; 64]) -> u64 {
//!     let s = &body;
//!     yield_once().await;
//!     let n = s[0] as u64;
//!     n + slow(id).await
//! }
//! require_durable(bad(1, [0; 64]));
//! ```
//!
//! False positive: `PhantomData<&T>` holds no pointer but is rejected:
//! ```compile_fail,E0277
//! use durable_poc::*;
//! use std::marker::PhantomData;
//! async fn bad(id: u64) -> u64 {
//!     let p: PhantomData<&u8> = PhantomData;
//!     let r = slow(id).await;
//!     let _ = p;
//!     r
//! }
//! require_durable(bad(1));
//! ```
