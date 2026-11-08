//! Combinators that can sit at a slow point.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Safety: durable-driver may serialize and rebuild this type field by field,
/// bypassing its constructors. Its fields must carry all of its state.
pub unsafe trait Structural {}

/// Both futures, polled concurrently (`a` first).
pub struct Join2<A: Future, B: Future> {
    a: A,
    b: B,
    ra: Option<A::Output>,
    rb: Option<B::Output>,
}

pub fn join2<A: Future, B: Future>(a: A, b: B) -> Join2<A, B> {
    Join2 { a, b, ra: None, rb: None }
}

unsafe impl<A: Future, B: Future> Structural for Join2<A, B> {}

impl<A: Future, B: Future> Future for Join2<A, B> {
    type Output = (A::Output, B::Output);
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        if this.ra.is_none()
            && let Poll::Ready(v) = unsafe { Pin::new_unchecked(&mut this.a) }.poll(cx)
        {
            this.ra = Some(v);
        }
        if this.rb.is_none()
            && let Poll::Ready(v) = unsafe { Pin::new_unchecked(&mut this.b) }.poll(cx)
        {
            this.rb = Some(v);
        }
        if this.ra.is_some() && this.rb.is_some() {
            Poll::Ready((this.ra.take().unwrap(), this.rb.take().unwrap()))
        } else {
            Poll::Pending
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Either<A, B> {
    Left(A),
    Right(B),
}

/// The first of two futures to finish (`a` is polled first); the other is dropped.
pub struct Select2<A, B> {
    a: A,
    b: B,
}

pub fn select2<A: Future, B: Future>(a: A, b: B) -> Select2<A, B> {
    Select2 { a, b }
}

unsafe impl<A, B> Structural for Select2<A, B> {}

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
