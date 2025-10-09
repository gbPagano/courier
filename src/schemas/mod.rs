use std::any::type_name;
use std::fmt::Debug;

use serde::{Deserialize, Serialize};

pub mod bytes;
pub mod kafka;

pub trait Json: Serialize + for<'de> Deserialize<'de> + Send + Sync + Debug {}

impl<T> Json for T where T: Serialize + for<'de> Deserialize<'de> + Send + Sync + Debug {}

pub trait Named {
    fn get_id(&self) -> &'static str {
        type_name::<Self>()
    }

    fn set_id(&mut self, _id: &'static str) {}
}

impl<T> Named for T {}
