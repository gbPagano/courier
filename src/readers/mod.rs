use anyhow::Result;
use std::future::Future;
use tokio_stream::Stream;

pub mod kafka;

pub trait StreamReader {
    type Item;

    fn stream(&self) -> impl Future<Output = impl Stream<Item = Self::Item>> + Send;
}

pub trait Reader {
    type Item;

    fn read(&self) -> impl Future<Output = Result<Self::Item>> + Send;
}
