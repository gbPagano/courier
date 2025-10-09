use std::any::type_name;
use std::future::Future;

use anyhow::Result;
use tokio_stream::Stream;

pub mod api;
pub mod kafka;

pub trait StreamReader {
    type Item;

    fn stream(&self) -> impl Future<Output = impl Stream<Item = Self::Item>> + Send;

    fn set_id(&mut self, _id: &'static str) {}

    fn get_id(&self) -> &'static str {
        type_name::<Self>()
    }
}

pub trait Reader {
    type Item;

    fn read(&self) -> impl Future<Output = Result<Self::Item>> + Send;

    fn set_id(&mut self, _id: &str) {}

    fn get_id(&self) -> &'static str {
        type_name::<Self>()
    }
}
