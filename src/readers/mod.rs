use std::any::type_name;
use std::future::Future;

use anyhow::Result;
use async_trait::async_trait;
use tokio_stream::Stream;

pub mod api;
pub mod kafka;

pub trait StreamReader: Sync + Send {
    type Item: Send;

    fn stream(&self) -> impl Future<Output = impl Stream<Item = Self::Item> + Send> + Send;

    fn set_id(&mut self, _id: &'static str) {}

    fn get_id(&self) -> &'static str {
        type_name::<Self>()
    }
}

#[async_trait]
pub trait Reader: Sync + Send {
    type Item: Send + Sync;

    async fn read(&self) -> Result<Self::Item>;

    fn set_id(&mut self, _id: &str) {}

    fn get_id(&self) -> &'static str {
        type_name::<Self>()
    }
}
