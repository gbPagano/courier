use async_trait::async_trait;

mod interval;
mod stream;

pub use interval::IntervalOperation;
pub use stream::StreamOperation;

#[async_trait]
pub trait Operation: Send + Sync {
    async fn run(&self);
}
