#![feature(auto_traits, negative_impls)]
//! v0 PoC of durable native futures. See docs/design.md.

use std::cell::Cell;
use std::future::Future;
use std::hash::Hasher;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::{ptr, slice};

/// Safety: the type holds no absolute addresses, and nothing outside
/// the task arena holds the address of a value of this type.
pub unsafe auto trait Durable {}
impl<T: ?Sized> !Durable for &T {}
impl<T: ?Sized> !Durable for &mut T {}
impl<T: ?Sized> !Durable for *const T {}
impl<T: ?Sized> !Durable for *mut T {}
// NonNull, Box, Vec, String, Arc, AtomicPtr... are excluded structurally:
// they all bottom out in a raw pointer field.

thread_local! {
    /// Set by a slow leaf that returned Pending during the current poll.
    static PARKED: Cell<Option<u64>> = const { Cell::new(None) };
    /// Number of live ephemeral sections. Must be per task in a real runtime.
    static EPHEMERAL: Cell<usize> = const { Cell::new(0) };
}

// ---------- leaves ----------

/// Slow leaf: ready once `results/<id>` exists, otherwise parks.
pub struct Slow {
    id: u64,
}
pub fn slow(id: u64) -> Slow {
    Slow { id }
}
impl Future for Slow {
    type Output = u64;
    fn poll(self: Pin<&mut Self>, _: &mut Context) -> Poll<u64> {
        assert_eq!(EPHEMERAL.get(), 0, "slow call inside ephemeral section");
        match std::fs::read_to_string(format!("results/{}", self.id)) {
            Ok(s) => Poll::Ready(s.trim().parse().unwrap()),
            Err(_) => {
                PARKED.set(Some(self.id));
                Poll::Pending
            }
        }
    }
}

/// A non-slow await: Pending once, then Ready.
pub struct YieldOnce(bool);
pub fn yield_once() -> YieldOnce {
    YieldOnce(false)
}
impl Future for YieldOnce {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _: &mut Context) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            Poll::Pending
        }
    }
}

// ---------- Option A: ephemeral sections ----------

/// Asserts Durable for a fast section; enforced at run time by the counter.
pub struct Ephemeral<F> {
    inner: F,
    entered: bool,
}
unsafe impl<F> Durable for Ephemeral<F> {}
pub fn ephemeral<F: Future>(inner: F) -> Ephemeral<F> {
    Ephemeral { inner, entered: false }
}
impl<F: Future> Future for Ephemeral<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<F::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        if !this.entered {
            this.entered = true;
            EPHEMERAL.set(EPHEMERAL.get() + 1);
        }
        let r = unsafe { Pin::new_unchecked(&mut this.inner) }.poll(cx);
        if r.is_ready() {
            this.entered = false;
            // saturating only so the unsound demo shows its garbage instead of
            // an underflow panic; a real runtime resets this per task.
            EPHEMERAL.set(EPHEMERAL.get().saturating_sub(1));
        }
        r
    }
}
impl<F> Drop for Ephemeral<F> {
    fn drop(&mut self) {
        if self.entered {
            EPHEMERAL.set(EPHEMERAL.get().saturating_sub(1));
        }
    }
}

/// Minimal join of two futures (polls `a` first). Durable iff both are.
pub struct Join2<A: Future, B: Future> {
    a: A,
    b: B,
    ra: Option<A::Output>,
    rb: Option<B::Output>,
}
pub fn join2<A: Future, B: Future>(a: A, b: B) -> Join2<A, B> {
    Join2 { a, b, ra: None, rb: None }
}
impl<A: Future, B: Future> Future for Join2<A, B> {
    type Output = (A::Output, B::Output);
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        if this.ra.is_none() {
            if let Poll::Ready(v) = unsafe { Pin::new_unchecked(&mut this.a) }.poll(cx) {
                this.ra = Some(v);
            }
        }
        if this.rb.is_none() {
            if let Poll::Ready(v) = unsafe { Pin::new_unchecked(&mut this.b) }.poll(cx) {
                this.rb = Some(v);
            }
        }
        if this.ra.is_some() && this.rb.is_some() {
            Poll::Ready((this.ra.take().unwrap(), this.rb.take().unwrap()))
        } else {
            Poll::Pending
        }
    }
}

/// Escape hatch used only by the negative-control demos.
pub struct ForceDurable<F>(pub F);
unsafe impl<F> Durable for ForceDurable<F> {}
impl<F: Future> Future for ForceDurable<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<F::Output> {
        unsafe { self.map_unchecked_mut(|s| &mut s.0) }.poll(cx)
    }
}

/// Compile-time check helper.
pub fn require_durable<F: Future + Durable>(f: F) -> F {
    f
}

// ---------- driver ----------

pub enum Outcome<T> {
    Done(T),
    Parked { event: u64 },
}

/// Poll until Ready or parked on a slow call. On park, write the frame to
/// `path` and forget the in-memory future.
pub fn run<F: Future + Durable>(mut fut: Pin<Box<F>>, tag: u32, path: &str) -> Outcome<F::Output> {
    println!("frame @ {:p}, size {} bytes", &*fut, size_of::<F>());
    let mut cx = Context::from_waker(Waker::noop());
    let skip_check = std::env::var_os("POC_SKIP_EPHEMERAL_CHECK").is_some();
    loop {
        PARKED.set(None);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return Outcome::Done(v),
            Poll::Pending => {
                let Some(event) = PARKED.get() else { continue };
                // The leaf's own assert misses an ephemeral section that starts
                // *after* the slow leaf parks in the same poll (e.g. a join).
                if EPHEMERAL.get() != 0 && !skip_check {
                    panic!("parked on slow event {event} while an ephemeral section is live; refusing to evict");
                }
                let p = &*fut as *const F as *const u8;
                let bytes = unsafe { slice::from_raw_parts(p, size_of::<F>()) };
                write_snapshot(path, tag, bytes);
                std::mem::forget(fut); // ownership is now on disk
                return Outcome::Parked { event };
            }
        }
    }
}

/// `_ctor` is never called; it only names the unnameable `F`.
pub fn resume<F: Future + Durable>(_ctor: impl FnOnce() -> F, bytes: &[u8]) -> Pin<Box<F>> {
    assert_eq!(bytes.len(), size_of::<F>(), "frame size mismatch");
    let mut b = Box::<F>::new_uninit();
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), b.as_mut_ptr() as *mut u8, bytes.len());
        Box::into_pin(b.assume_init())
    }
}

// ---------- snapshot file ----------

const MAGIC: &[u8; 4] = b"DUR0";

pub fn build_id() -> u64 {
    let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    let mut h = std::hash::DefaultHasher::new();
    h.write(&exe);
    h.finish()
}

/// Layout: magic, tag u32, build_id u64, len u64, frame bytes.
pub fn write_snapshot(path: &str, tag: u32, frame: &[u8]) {
    let mut out = Vec::with_capacity(24 + frame.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&build_id().to_le_bytes());
    out.extend_from_slice(&(frame.len() as u64).to_le_bytes());
    out.extend_from_slice(frame); // includes uninit padding: ignored in v0
    std::fs::write(path, out).unwrap();
}

/// Returns (tag, frame bytes). Panics if the snapshot came from another build.
pub fn read_snapshot(path: &str) -> (u32, Vec<u8>) {
    let d = std::fs::read(path).unwrap();
    assert_eq!(&d[0..4], MAGIC, "bad magic");
    let tag = u32::from_le_bytes(d[4..8].try_into().unwrap());
    let bid = u64::from_le_bytes(d[8..16].try_into().unwrap());
    assert_eq!(bid, build_id(), "snapshot written by a different build");
    let len = u64::from_le_bytes(d[16..24].try_into().unwrap()) as usize;
    (tag, d[24..24 + len].to_vec())
}

pub mod compile_checks;
