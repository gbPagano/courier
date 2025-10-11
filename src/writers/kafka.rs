use std::time::Duration;

use anyhow::Result;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};

use crate::schemas::Json;
use crate::schemas::kafka::KafkaMessage;
use crate::writers::Writer;

pub struct KafkaWriter<T: Json> {
    producer: FutureProducer,
    topic: String,
    _marker: std::marker::PhantomData<T>,
}

impl<T: Json> KafkaWriter<T> {
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

impl<T: Json> Writer for KafkaWriter<T> {
    type Item = KafkaMessage<T>;

    async fn write(&self, data: KafkaMessage<T>) -> Result<()> {
        log::debug!("Sending message to topic: {}", self.topic);

        let payload = serde_json::to_string(&data.value).map_err(|e| {
            log::error!(
                "Failed to serialize message for topic '{}': {e}",
                self.topic,
            );
            e
        })?;
        log::trace!("Serialized payload for topic '{}': {payload:?}", self.topic);

        let delivery_status = self
            .producer
            .send(
                FutureRecord::to(&self.topic)
                    .key(&data.key)
                    .payload(&payload),
                Duration::from_secs(0),
            )
            .await;

        match delivery_status {
            Ok(status) => {
                log::debug!(
                    "Delivered to topic '{}': (partition: {}, offset: {})",
                    self.topic,
                    status.partition,
                    status.offset
                );
                Ok(())
            }
            Err((e, _)) => {
                log::error!("Failed to deliver message to topic '{}': {e:?}", self.topic,);
                Err(anyhow::anyhow!("KafkaError: {e:?}"))
            }
        }
    }
}
