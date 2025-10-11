use async_trait::async_trait;

mod interval;
mod interval_fanout;
mod stream;

pub use interval::IntervalOperation;
pub use interval_fanout::IntervalFanoutOperation;
pub use stream::StreamOperation;

#[async_trait]
pub trait Operation: Send + Sync {
    async fn run(&self);
}
