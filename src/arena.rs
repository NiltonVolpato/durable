//! Task arena: one contiguous allocation holding a header, the root future's
//! frame, and task data. Everything inside refers to other things inside by
//! offset, so the used prefix can be memcpy'd to a new address.
//!
//! Two kinds of in-arena pointers:
//! - `ABox` / `DVec`: offsets from the arena base. Plain values that the
//!   frame can hold and move freely. Deref needs the current task's arena,
//!   which the runtime sets for the duration of each poll.
//! - `RelRef`: offset from its own address. Deref needs no context, but it
//!   must sit at a stable address (arena data or a pinned slot), so it is
//!   only ever set through `Pin<&mut Self>`.

use std::alloc::{self, Layout};
use std::cell::Cell;
use std::future::Future;
use std::marker::{PhantomData, PhantomPinned};
use std::ops::Deref;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::task::{Context, Poll};

use crate::Durable;

pub(crate) const NONE: u64 = u64::MAX;

/// Per-task runtime state. Lives at offset 0 of the arena, so it moves with it.
#[repr(C)]
pub(crate) struct Header {
    pub task_id: u64,
    /// Event id of the slow leaf that parked during the current poll, or NONE.
    pub parked: u64,
    pub cap: u32,
    pub align: u32,
    pub used: u32,
    pub root_off: u32,
    pub root_size: u32,
    /// Number of live ephemeral sections in this task.
    pub ephemeral: u32,
}

// ---------- current-task context ----------

thread_local! {
    /// Base of the arena of the task being polled. Set only for one poll at a
    /// time (like tokio's current-task context); nothing persists across polls.
    static CURRENT: Cell<*mut u8> = const { Cell::new(ptr::null_mut()) };
}

fn enter<R>(base: *mut u8, f: impl FnOnce() -> R) -> R {
    struct Restore(*mut u8);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.set(self.0);
        }
    }
    let _restore = Restore(CURRENT.replace(base));
    f()
}

fn current() -> *mut u8 {
    let b = CURRENT.get();
    assert!(!b.is_null(), "no current task arena (called outside a task poll?)");
    b
}

/// Do not nest: each call makes a fresh `&mut Header`.
pub(crate) fn with_header<R>(f: impl FnOnce(&mut Header) -> R) -> R {
    unsafe { f(&mut *current().cast::<Header>()) }
}

/// Bump-allocate in the current arena. Returns (offset, task id).
fn alloc_in_current(layout: Layout) -> (u32, u64) {
    with_header(|h| {
        assert!(layout.align() <= h.align as usize, "alignment above arena alignment");
        let off = (h.used as usize).next_multiple_of(layout.align());
        let end = off + layout.size();
        assert!(end <= h.cap as usize, "task arena full");
        h.used = end as u32;
        (off as u32, h.task_id)
    })
}

fn at<T>(off: u32) -> *mut T {
    unsafe { current().add(off as usize).cast() }
}

fn check_task(task: u64) {
    with_header(|h| assert_eq!(h.task_id, task, "arena handle used outside its task"));
}

// ---------- arena memory + task ----------

struct Arena {
    base: NonNull<u8>,
    layout: Layout,
}
impl Arena {
    fn zeroed(cap: usize, align: usize) -> Self {
        let layout = Layout::from_size_align(cap, align).unwrap();
        let p = unsafe { alloc::alloc_zeroed(layout) };
        let base = NonNull::new(p).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        // RelRef derefs rebuild pointers from addresses.
        let _ = base.as_ptr().expose_provenance();
        Arena { base, layout }
    }
    fn header(&self) -> *mut Header {
        self.base.as_ptr().cast()
    }
}
impl Drop for Arena {
    fn drop(&mut self) {
        unsafe { alloc::dealloc(self.base.as_ptr(), self.layout) }
    }
}

/// The used prefix of an arena, plus what is needed to rebuild it.
pub struct Snapshot {
    pub cap: u32,
    pub align: u32,
    pub bytes: Vec<u8>,
}

/// A root future living in its own arena.
pub struct Task<F: Future> {
    arena: Arena,
    /// False once ownership moved to a snapshot: then Drop frees memory only.
    live: bool,
    _f: PhantomData<F>,
}

impl<F: Future + Durable> Task<F> {
    pub fn new(task_id: u64, cap: usize, fut: F) -> Self {
        let align = align_of::<F>().max(64);
        let arena = Arena::zeroed(cap, align);
        let h = arena.header();
        unsafe {
            h.write(Header {
                task_id,
                parked: NONE,
                cap: cap as u32,
                align: align as u32,
                used: size_of::<Header>() as u32,
                root_off: 0,
                root_size: size_of::<F>() as u32,
                ephemeral: 0,
            });
        }
        let (off, _) = enter(arena.base.as_ptr(), || alloc_in_current(Layout::new::<F>()));
        unsafe {
            (*h).root_off = off;
            arena.base.as_ptr().add(off as usize).cast::<F>().write(fut);
        }
        Task { arena, live: true, _f: PhantomData }
    }

    /// `_ctor` is never called; it only names the unnameable `F`.
    pub fn resume(_ctor: impl FnOnce() -> F, snap: &Snapshot) -> Self {
        assert!(align_of::<F>() <= snap.align as usize, "alignment mismatch");
        let arena = Arena::zeroed(snap.cap as usize, snap.align as usize);
        unsafe {
            ptr::copy_nonoverlapping(snap.bytes.as_ptr(), arena.base.as_ptr(), snap.bytes.len());
        }
        let h = unsafe { &*arena.header() };
        assert_eq!(h.root_size as usize, size_of::<F>(), "frame size mismatch");
        assert_eq!(h.used as usize, snap.bytes.len(), "snapshot length mismatch");
        Task { arena, live: true, _f: PhantomData }
    }
}

impl<F: Future> Task<F> {
    fn root_ptr(&self) -> *mut F {
        unsafe { self.arena.base.as_ptr().add((*self.arena.header()).root_off as usize).cast() }
    }
    pub fn base(&self) -> *const u8 {
        self.arena.base.as_ptr()
    }
    pub fn frame(&self) -> *const u8 {
        self.root_ptr() as *const u8
    }
    pub fn used(&self) -> usize {
        unsafe { (*self.arena.header()).used as usize }
    }
    pub fn parked(&self) -> Option<u64> {
        let p = unsafe { (*self.arena.header()).parked };
        (p != NONE).then_some(p)
    }
    pub fn ephemeral(&self) -> u32 {
        unsafe { (*self.arena.header()).ephemeral }
    }

    pub fn poll(&mut self, cx: &mut Context) -> Poll<F::Output> {
        unsafe { (*self.arena.header()).parked = NONE };
        let root = unsafe { Pin::new_unchecked(&mut *self.root_ptr()) };
        enter(self.arena.base.as_ptr(), || root.poll(cx))
    }

    /// Copy out the used prefix and give up ownership without running drop.
    pub fn evict(mut self) -> Snapshot {
        self.live = false;
        let h = unsafe { &*self.arena.header() };
        let bytes = unsafe { std::slice::from_raw_parts(self.arena.base.as_ptr(), h.used as usize) };
        // Includes uninit padding: ignored in the PoC.
        Snapshot { cap: h.cap, align: h.align, bytes: bytes.to_vec() }
    }
}

impl<F: Future> Drop for Task<F> {
    fn drop(&mut self) {
        if self.live {
            let root = self.root_ptr();
            enter(self.arena.base.as_ptr(), || unsafe { ptr::drop_in_place(root) });
        }
    }
}

// ---------- base-relative handles ----------

/// One value in the task arena. Element destructors are not run (bump arena).
pub struct ABox<T> {
    off: u32,
    task: u64,
    _t: PhantomData<T>,
}
impl<T> ABox<T> {
    pub fn new(v: T) -> Self {
        let (off, task) = alloc_in_current(Layout::new::<T>());
        unsafe { at::<T>(off).write(v) };
        ABox { off, task, _t: PhantomData }
    }
    fn ptr(&self) -> *mut T {
        check_task(self.task);
        at(self.off)
    }
    pub fn get(&self) -> &T {
        unsafe { &*self.ptr() }
    }
    pub fn get_pin(&mut self) -> Pin<&mut T> {
        unsafe { Pin::new_unchecked(&mut *self.ptr()) }
    }
}
impl<T> Deref for ABox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.get()
    }
}

/// Vector in the task arena. Grows only if `T: Unpin`: moving `!Unpin`
/// elements (e.g. ones holding a `RelRef`) would break their offsets.
pub struct DVec<T> {
    off: u32,
    len: u32,
    cap: u32,
    task: u64, // NONE until the first allocation
    _t: PhantomData<T>,
}
impl<T> Default for DVec<T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T> DVec<T> {
    pub const fn new() -> Self {
        DVec { off: 0, len: 0, cap: 0, task: NONE, _t: PhantomData }
    }
    pub fn with_capacity(n: usize) -> Self {
        let mut v = Self::new();
        v.realloc(n);
        v
    }
    pub fn len(&self) -> usize {
        self.len as usize
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    fn data(&self) -> *mut T {
        check_task(self.task);
        at(self.off)
    }
    pub fn as_slice(&self) -> &[T] {
        if self.cap == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.data(), self.len()) }
    }
    pub fn get(&self, i: usize) -> &T {
        &self.as_slice()[i]
    }
    pub fn get_pin(&mut self, i: usize) -> Pin<&mut T> {
        assert!(i < self.len());
        unsafe { Pin::new_unchecked(&mut *self.data().add(i)) }
    }
    /// Push without growing. Works for any `T`; panics when full.
    pub fn push_pinned(&mut self, v: T) {
        assert!(self.len < self.cap, "DVec of !Unpin elements cannot grow; size it with with_capacity");
        unsafe { self.data().add(self.len()).write(v) };
        self.len += 1;
    }
    /// Move elements to a new buffer. The old one is abandoned (bump arena).
    fn realloc(&mut self, new_cap: usize) {
        let (off, task) = alloc_in_current(Layout::array::<T>(new_cap).unwrap());
        if self.len > 0 {
            unsafe { ptr::copy_nonoverlapping(self.data(), at::<T>(off), self.len()) };
        }
        self.off = off;
        self.cap = new_cap as u32;
        self.task = task;
    }
}
impl<T: Unpin> DVec<T> {
    pub fn push(&mut self, v: T) {
        if self.len == self.cap {
            self.realloc((self.cap as usize * 2).max(4));
        }
        self.push_pinned(v);
    }
}

// ---------- self-relative pointer ----------

const NULL_OFF: isize = isize::MIN;

/// Offset from `&self` to the target. Both must be in the same task arena.
/// Not Copy/Clone and `!Unpin`: a moved copy would point somewhere else.
/// Aliasing with `&mut` access to the target is not tracked (needs a brand
/// token; see design doc).
pub struct RelRef<T> {
    off: isize,
    _pin: PhantomPinned,
    _t: PhantomData<T>,
}
impl<T> RelRef<T> {
    pub const fn null() -> Self {
        RelRef { off: NULL_OFF, _pin: PhantomPinned, _t: PhantomData }
    }
    /// Point at `target`. Runtime check stands in for the arena brand.
    pub fn set(self: Pin<&mut Self>, target: &T) {
        let me = unsafe { self.get_unchecked_mut() };
        let s = (me as *mut Self).addr();
        let t = (target as *const T).addr();
        let base = current().addr();
        let cap = with_header(|h| h.cap as usize);
        let inside = |a: usize| a >= base && a < base + cap;
        assert!(inside(s) && inside(t), "RelRef endpoints must both be in the current task arena");
        me.off = t.wrapping_sub(s) as isize;
    }
    pub fn get(&self) -> Option<&T> {
        if self.off == NULL_OFF {
            return None;
        }
        let addr = (self as *const Self).addr().wrapping_add_signed(self.off);
        Some(unsafe { &*ptr::with_exposed_provenance::<T>(addr) })
    }
}
