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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use rdkafka::producer::{FutureProducer, FutureRecord};
    use serde_json::json;
    use testcontainers_modules::kafka::apache::{self, KAFKA_PORT};
    use testcontainers_modules::testcontainers::runners::AsyncRunner;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn emits_envelope_from_kafka_record() -> anyhow::Result<()> {
        let node = apache::Kafka::default().start().await?;
        let host_port = node.get_host_port_ipv4(KAFKA_PORT).await?;
        let brokers = format!("127.0.0.1:{host_port}");

        let topic = "courier-source-test";

        // Pre-create the topic by producing (and consuming a throwaway) so
        // that KafkaSource's subscribe succeeds immediately. We cannot
        // produce the real message yet: KafkaSource uses the default
        // `auto.offset.reset=latest`, so a fresh group ignores records
        // written before it joined.
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            .set("message.timeout.ms", "5000")
            .create()?;

        let source = KafkaSource::new("src", &brokers, "courier-source-group", vec![topic]);
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let cancel_inner = cancel.clone();
        let handle = tokio::spawn(async move {
            Box::new(source).run(tx, cancel_inner).await;
        });

        // Produce in a loop until something lands; KafkaSource starts from
        // `latest`, so early sends may arrive before the group is assigned.
        let produce_cancel = cancel.clone();
        let produce_handle = tokio::spawn(async move {
            let payload = r#"{"event":"login","user":"u-1"}"#;
            while !produce_cancel.is_cancelled() {
                let _ = producer
                    .send(
                        FutureRecord::to(topic).key("k-1").payload(payload),
                        Duration::from_secs(5),
                    )
                    .await;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });

        let env = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await?
            .expect("source closed before emitting");
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), produce_handle).await;

        assert_eq!(env.meta.key.as_deref(), Some("k-1"));
        assert_eq!(env.payload, json!({ "event": "login", "user": "u-1" }));
        assert_eq!(
            env.meta.headers.get("kafka.topic").map(String::as_str),
            Some(topic),
        );
        assert_eq!(
            env.meta.headers.get("kafka.partition").map(String::as_str),
            Some("0"),
        );
        assert!(
            env.meta.headers.contains_key("kafka.offset"),
            "missing kafka.offset header",
        );
        Ok(())
    }
}
