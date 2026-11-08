#![feature(auto_traits, negative_impls)]
//! PoC of durable native futures. See docs/design.md.

use std::future::Future;
use std::hash::Hasher;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

pub mod arena;
pub mod compile_checks;
pub mod glue;
pub use serde_json;
pub use arena::{ABox, DVec, RelRef, Snapshot, Task};
use arena::with_header;

/// Safety: the type holds no absolute addresses or other process-local
/// identity, and nothing outside the task arena holds its address.
pub unsafe auto trait Durable {}
impl<T: ?Sized> !Durable for &T {}
impl<T: ?Sized> !Durable for &mut T {}
impl<T: ?Sized> !Durable for *const T {}
impl<T: ?Sized> !Durable for *mut T {}
// NonNull, Box, Vec, String, Arc, AtomicPtr... are excluded structurally:
// they all bottom out in a raw pointer field.

// Process-local handles: plain integers inside, meaningless in another
// process. Not exhaustive; add more as found.
#[cfg(unix)]
impl !Durable for std::os::fd::OwnedFd {}
impl !Durable for std::fs::File {}
impl !Durable for std::net::TcpStream {}
impl !Durable for std::net::TcpListener {}
impl !Durable for std::net::UdpSocket {}
#[cfg(unix)]
impl !Durable for std::os::unix::net::UnixStream {}
#[cfg(unix)]
impl !Durable for std::os::unix::net::UnixListener {}
#[cfg(unix)]
impl !Durable for std::os::unix::net::UnixDatagram {}
impl !Durable for std::process::Child {}
impl !Durable for std::process::ChildStdin {}
impl !Durable for std::process::ChildStdout {}
impl !Durable for std::process::ChildStderr {}
impl !Durable for std::thread::ThreadId {}
/// Monotonic clock: not comparable across reboots or machines. Use SystemTime.
impl !Durable for std::time::Instant {}

// ---------- leaves ----------

/// Marks leaf futures that can park a task for a long time. durable-driver
/// treats any await that can reach one as a slow point.
pub trait SlowLeaf {}
impl SlowLeaf for Slow {}

/// Slow leaf: ready once `results/<id>` exists, otherwise parks.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Slow {
    id: u64,
}
pub fn slow(id: u64) -> Slow {
    Slow { id }
}
impl Future for Slow {
    type Output = u64;
    fn poll(self: Pin<&mut Self>, _: &mut Context) -> Poll<u64> {
        with_header(|h| assert_eq!(h.ephemeral, 0, "slow call inside ephemeral section"));
        match std::fs::read_to_string(format!("results/{}", self.id)) {
            Ok(s) => Poll::Ready(s.trim().parse().unwrap()),
            Err(_) => {
                with_header(|h| h.parked = self.id);
                Poll::Pending
            }
        }
    }
}

/// A non-slow await: Pending once, then Ready.
#[derive(serde::Serialize, serde::Deserialize)]
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

/// Asserts Durable for a fast section; enforced at run time by the task's
/// ephemeral counter (in the arena header).
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
            with_header(|h| h.ephemeral += 1);
        }
        let r = unsafe { Pin::new_unchecked(&mut this.inner) }.poll(cx);
        if r.is_ready() {
            this.entered = false;
            with_header(|h| h.ephemeral -= 1);
        }
        r
    }
}
impl<F> Drop for Ephemeral<F> {
    fn drop(&mut self) {
        if self.entered {
            with_header(|h| h.ephemeral -= 1);
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

/// Durable timer: ready once the wall clock reaches `deadline` (unix secs).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SleepUntil {
    deadline: u64,
}
pub fn sleep_until(deadline: u64) -> SleepUntil {
    SleepUntil { deadline }
}
impl SlowLeaf for SleepUntil {}
impl Future for SleepUntil {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _: &mut Context) -> Poll<()> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        if now >= self.deadline {
            return Poll::Ready(());
        }
        with_header(|h| h.parked = self.deadline);
        Poll::Pending
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum Either<A, B> {
    Left(A),
    Right(B),
}

/// First of two futures to finish (polls `a` first); the other is dropped.
pub struct Select2<A, B> {
    a: A,
    b: B,
}
pub fn select2<A: Future, B: Future>(a: A, b: B) -> Select2<A, B> {
    Select2 { a, b }
}
impl<A: Future, B: Future> Future for Select2<A, B> {
    type Output = Either<A::Output, B::Output>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        if let Poll::Ready(v) = unsafe { Pin::new_unchecked(&mut this.a) }.poll(cx) {
            return Poll::Ready(Either::Left(v));
        }
        if let Poll::Ready(v) = unsafe { Pin::new_unchecked(&mut this.b) }.poll(cx) {
            return Poll::Ready(Either::Right(v));
        }
        Poll::Pending
    }
}

/// Safety: durable-driver may serialize and rebuild this type field by field,
/// bypassing its constructors. Its fields must carry all of its state.
pub unsafe trait Structural {}
unsafe impl<A: Future, B: Future> Structural for Join2<A, B> {}
unsafe impl<A, B> Structural for Select2<A, B> {}

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

/// Poll until Ready or parked on a slow call. On park, write the arena to
/// `path` and give up the in-memory task without dropping it.
pub fn run<F: Future + Durable>(mut task: Task<F>, tag: u32, path: &str) -> Outcome<F::Output> {
    println!(
        "arena @ {:p}, frame @ {:p} ({} B), used {} B",
        task.base(),
        task.frame(),
        size_of::<F>(),
        task.used()
    );
    let mut cx = Context::from_waker(Waker::noop());
    let skip_check = std::env::var_os("POC_SKIP_EPHEMERAL_CHECK").is_some();
    loop {
        match task.poll(&mut cx) {
            Poll::Ready(v) => return Outcome::Done(v),
            Poll::Pending => {
                let Some(event) = task.parked() else { continue };
                // The leaf's own assert misses an ephemeral section that starts
                // *after* the slow leaf parks in the same poll (e.g. a join).
                if task.ephemeral() != 0 && !skip_check {
                    panic!("parked on slow event {event} while an ephemeral section is live; refusing to evict");
                }
                let snap = task.evict();
                println!("evicted {} B", snap.bytes.len());
                write_snapshot(path, tag, &snap);
                return Outcome::Parked { event };
            }
        }
    }
}

// ---------- snapshot file ----------

const MAGIC: &[u8; 4] = b"DUR1";

pub fn build_id() -> u64 {
    let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    let mut h = std::hash::DefaultHasher::new();
    h.write(&exe);
    h.finish()
}

/// Layout: magic, tag u32, build_id u64, cap u32, align u32, len u64, bytes.
pub fn write_snapshot(path: &str, tag: u32, snap: &Snapshot) {
    let mut out = Vec::with_capacity(32 + snap.bytes.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&build_id().to_le_bytes());
    out.extend_from_slice(&snap.cap.to_le_bytes());
    out.extend_from_slice(&snap.align.to_le_bytes());
    out.extend_from_slice(&(snap.bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(&snap.bytes);
    std::fs::write(path, out).unwrap();
}

/// Returns (tag, snapshot). Panics if the snapshot came from another build.
pub fn read_snapshot(path: &str) -> (u32, Snapshot) {
    let d = std::fs::read(path).unwrap();
    let u32_at = |i: usize| u32::from_le_bytes(d[i..i + 4].try_into().unwrap());
    let u64_at = |i: usize| u64::from_le_bytes(d[i..i + 8].try_into().unwrap());
    assert_eq!(&d[0..4], MAGIC, "bad magic");
    assert_eq!(u64_at(8), build_id(), "snapshot written by a different build");
    let len = u64_at(24) as usize;
    let snap = Snapshot { cap: u32_at(16), align: u32_at(20), bytes: d[32..32 + len].to_vec() };
    (u32_at(4), snap)
}
