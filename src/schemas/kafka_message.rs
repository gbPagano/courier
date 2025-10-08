use rdkafka::message::{OwnedHeaders, ToBytes};
use std::fmt::Debug;

#[derive(Debug)]
pub struct KafkaMessage<K: ToBytes + Send, V: ToBytes + Send> {
    pub key: K,
    pub value: V,
    pub headers: OwnedHeaders,
}
impl<K, V> KafkaMessage<K, V>
where
    K: ToBytes + Send,
    V: ToBytes + Send,
{
    pub fn new(key: K, value: V, headers: OwnedHeaders) -> Self {
        Self {
            key,
            value,
            headers,
        }
    }
}
