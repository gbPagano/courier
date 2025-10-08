use anyhow::Result;
use std::future::Future;

pub mod kafka;

pub trait Writer {
    type Item;

    fn setup(&mut self) -> impl Future<Output = Result<()>> + Send;

    fn write(&self, data: Self::Item) -> impl Future<Output = Result<()>> + Send;
}
