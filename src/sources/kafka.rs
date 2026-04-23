use async_trait::async_trait;
use rdkafka::Message;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use serde_json::Value;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::envelope::Envelope;
use crate::sources::Source;

/// Kafka consumer source. Deserializes each record's payload as JSON and
/// emits it as the envelope payload. Populates `meta.key` from the record
/// key and records `kafka.topic`, `kafka.partition`, `kafka.offset` in
/// `meta.headers` for downstream debugging.
///
/// Auto-commit is disabled; offsets are not committed by this source
/// (at-least-once semantics rely on downstream acking — TODO).
pub struct KafkaSource {
    id: String,
    consumer: StreamConsumer,
}

impl KafkaSource {
    pub fn new(id: impl Into<String>, brokers: &str, group_id: &str, topics: Vec<&str>) -> Self {
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
            id: id.into(),
            consumer,
        }
    }
}

#[async_trait]
impl Source for KafkaSource {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
        log::info!("[{}] starting kafka consumer", self.id);

        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => {
                    log::info!("[{}] cancelled", self.id);
                    return;
                }
                result = self.consumer.recv() => match result {
                    Ok(m) => m,
                    Err(e) => {
                        log::error!("[{}] kafka recv error: {e}", self.id);
                        continue;
                    }
                },
            };

            let offset = msg.offset();
            let partition = msg.partition();
            let topic = msg.topic().to_string();

            let key = msg
                .key()
                .and_then(|k| std::str::from_utf8(k).ok())
                .map(|s| s.trim().to_string());

            let payload_bytes = match msg.payload() {
                Some(p) => p,
                None => {
                    log::error!("[{}] message at offset {offset} has no payload", self.id);
                    continue;
                }
            };

            let payload: Value = match serde_json::from_slice(payload_bytes) {
                Ok(v) => v,
                Err(e) => {
                    log::error!(
                        "[{}] failed to deserialize at offset {offset}: {e}",
                        self.id,
                    );
                    continue;
                }
            };

            let mut env = Envelope::new(&self.id, payload);
            env.meta.key = key;
            env.meta.headers.insert("kafka.topic".into(), topic.clone());
            env.meta
                .headers
                .insert("kafka.partition".into(), partition.to_string());
            env.meta
                .headers
                .insert("kafka.offset".into(), offset.to_string());

            log::debug!(
                "[{}] received topic={topic} partition={partition} offset={offset}",
                self.id,
            );

            tokio::select! {
                _ = cancel.cancelled() => return,
                res = tx.send(env) => {
                    if res.is_err() {
                        log::info!("[{}] downstream closed, stopping", self.id);
                        return;
                    }
                }
            }
        }
    }
}
