use std::fmt::Debug;

use crate::schemas::{Json, Named};

#[derive(Debug)]
pub struct KafkaMessage<T: Json> {
    pub key: String,
    pub value: T,
}

impl<T: Json> KafkaMessage<T> {
    pub fn new(key: &str, value: T) -> Self {
        Self {
            key: key.into(),
            value,
        }
    }
}
impl<T: Json> From<T> for KafkaMessage<T> {
    fn from(value: T) -> Self {
        KafkaMessage::new(value.get_id(), value)
    }
}
