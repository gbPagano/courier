use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};

use crate::envelope::Envelope;
use crate::sinks::WriteOne;

/// Kafka producer sink. Serializes the envelope payload as JSON and sends
/// it to the configured topic. Uses `meta.key` as the record key when set,
/// falling back to `meta.source_id` otherwise.
pub struct KafkaSink {
    id: String,
    topic: String,
    producer: FutureProducer,
}

impl KafkaSink {
    pub fn new(id: impl Into<String>, brokers: &str, topic: impl Into<String>) -> Self {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "5000")
            .create()
            .expect("Kafka Producer creation failed");

        Self {
            id: id.into(),
            topic: topic.into(),
            producer,
        }
    }
}

#[async_trait]
impl WriteOne for KafkaSink {
    fn id(&self) -> &str {
        &self.id
    }

    async fn write(&self, env: &Envelope) -> Result<()> {
        let key = env
            .meta
            .key
            .clone()
            .unwrap_or_else(|| env.meta.source_id.clone());
        let payload = serde_json::to_string(&env.payload)?;

        let record = FutureRecord::to(&self.topic).key(&key).payload(&payload);
        match self.producer.send(record, Duration::from_secs(0)).await {
            Ok(status) => {
                log::debug!(
                    "[{}] delivered to topic={} partition={} offset={}",
                    self.id,
                    self.topic,
                    status.partition,
                    status.offset,
                );
                Ok(())
            }
            Err((e, _)) => Err(anyhow!("kafka delivery failed: {e:?}")),
        }
    }
}
