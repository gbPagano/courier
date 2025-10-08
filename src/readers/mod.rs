use anyhow::Result;
use async_stream::stream;
use std::future::Future;
use std::time::Duration;
use tokio_stream::{Stream, StreamExt};

pub mod kafka;

pub trait Reader {
    type Item;

    fn setup(&mut self) -> impl Future<Output = Result<()>> + Send;

    fn read(&self) -> impl Future<Output = impl Stream<Item = Self::Item>> + Send;
}
