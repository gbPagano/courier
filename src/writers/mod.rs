use std::any::type_name;
use std::future::Future;

use anyhow::Result;

pub mod kafka;

pub trait Writer: Sync + Send {
    type Item: Send;

    fn write(&self, data: Self::Item) -> impl Future<Output = Result<()>> + Send;

    fn set_id(&mut self, _id: &str) {}

    fn get_id(&self) -> &'static str {
        type_name::<Self>()
    }
}
