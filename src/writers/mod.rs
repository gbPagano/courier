use std::any::type_name;

use anyhow::Result;
use async_trait::async_trait;

pub mod kafka;

#[async_trait]
pub trait Writer: Sync + Send {
    type Item: Send;

    async fn write(&self, data: Self::Item) -> Result<()>;

    fn set_id(&mut self, _id: &str) {}

    fn get_id(&self) -> &'static str {
        type_name::<Self>()
    }
}
