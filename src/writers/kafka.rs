use std::fmt::Debug;
use std::time::Duration;

use anyhow::Result;
use rdkafka::config::ClientConfig;
use rdkafka::message::ToBytes;
use rdkafka::producer::{FutureProducer, FutureRecord};

use crate::readers::kafka::KafkaMessage;
use crate::writers::Writer;

pub struct KafkaWriter<K: ToBytes + Send, V: ToBytes + Send> {
    producer: FutureProducer,
    topic: String,
    _marker: std::marker::PhantomData<(K, V)>,
}
impl<K, V> KafkaWriter<K, V>
where
    K: ToBytes + Send,
    V: ToBytes + Send,
{
    pub fn new(brokers: &str, topic: &str) -> Self {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "5000")
            .create()
            .unwrap();

        Self {
            producer,
            topic: topic.into(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<K, V> Writer for KafkaWriter<K, V>
where
    K: ToBytes + Send + Sync + Debug,
    V: ToBytes + Send + Sync + Debug,
{
    type Item = KafkaMessage<K, V>;
    async fn setup(&mut self) -> Result<()> {
        Ok(())
    }

    async fn write(&self, data: KafkaMessage<K, V>) -> Result<()> {
        log::debug!("Sending message to topic: {}", self.topic);
        let delivery_status = self
            .producer
            .send(
                FutureRecord::to(&self.topic)
                    .key(&data.key)
                    .payload(&data.value)
                    .headers(data.headers.clone()),
                Duration::from_secs(0),
            )
            .await;

        match delivery_status {
            Ok(status) => {
                log::debug!("Delivered to topic: {} - {:?}", self.topic, status);
                Ok(())
            }
            Err((e, _)) => {
                log::error!("{:?}", e);
                Err(anyhow::anyhow!("KafkaError"))
            }
        }
    }
}
