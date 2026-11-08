//! Rejected by `#[durable::workflow]` itself.

#[durable::workflow]
async fn generic<T: Clone>(x: T) -> T {
    x
}

#[durable::workflow]
async fn impl_trait_arg(x: impl Clone) -> u64 {
    let _ = x;
    0
}

#[durable::workflow]
fn not_async() -> u64 {
    0
}

struct S;
impl S {
    #[durable::workflow]
    async fn method(&self) -> u64 {
        0
    }
}

durable::glue!();
fn main() {}
