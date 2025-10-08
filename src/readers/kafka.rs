use std::fmt::Debug;
use std::time::Duration;

use crate::readers::Reader;
use anyhow::Result;
use async_stream::stream;
use futures::stream::FuturesUnordered;
use futures::{StreamExt, TryStreamExt};
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::{OwnedHeaders, ToBytes};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::{Message, message};
use tokio_stream::Stream;

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

pub struct KafkaReader<K, V> {
    consumer: StreamConsumer,
    _marker: std::marker::PhantomData<(K, V)>,
    // topic: String,
}
impl<K, V> KafkaReader<K, V>
where
    K: ToBytes + Send,
    V: ToBytes + Send,
{
    pub fn new(brokers: &str, group_id: &str, topics: Vec<&str>) -> Self {
        let consumer: StreamConsumer = ClientConfig::new()
            .set("group.id", group_id)
            .set("bootstrap.servers", brokers)
            .set("enable.partition.eof", "false")
            .set("session.timeout.ms", "6000")
            .set("enable.auto.commit", "false")
            .create()
            .expect("Kafka Consumer creation failed");

        consumer
            .subscribe(&topics)
            .expect("Can't subscribe to specified topics");

        Self {
            consumer,
            _marker: std::marker::PhantomData,
        }
    }
}

pub trait FromBytes: Sized {
    /// The error type that will be returned if the conversion fails.
    // type Error;
    /// Tries to convert the provided byte slice into a different type.
    fn from_bytes(_: &[u8]) -> Result<Self>;
}

impl FromBytes for String {
    // type Error = str::Utf8Error;
    fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(String::from_utf8(bytes.to_vec())?)
    }
}

impl<K, V> Reader for KafkaReader<K, V>
where
    K: ToBytes + Send + Sync + FromBytes + Clone + Debug,
    V: ToBytes + Send + Sync + FromBytes + Clone + Debug,
{
    type Item = KafkaMessage<K, V>;
    async fn setup(&mut self) -> Result<()> {
        Ok(())
    }

    async fn read(&self) -> impl Stream<Item = KafkaMessage<K, V>> {
        stream! {
            loop {
                if let Ok(m) = self.consumer.recv().await {
                    let m = m.detach();

                    let key = K::from_bytes(m.key().unwrap()).ok().unwrap().to_owned();
                    let value = V::from_bytes(m.payload().unwrap()).ok().unwrap().to_owned();
                    let message = KafkaMessage::new(key, value, OwnedHeaders::new());
                    log::info!("Message: {:?}", message);

                    yield message;
                }
            }
        }
    }
}
