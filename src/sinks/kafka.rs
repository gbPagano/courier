use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use rdkafka::config::ClientConfig;
use rdkafka::message::{Header, OwnedHeaders};
use rdkafka::producer::{FutureProducer, FutureRecord};
use serde::Deserialize;
use serde_json::Value;

use crate::config::parse_config;
use crate::envelope::Envelope;
use crate::observability::trace_context::{TRACEPARENT, TRACESTATE};
use crate::pipeline::ErrorPolicy;
use crate::retry::RetryPolicy;
use crate::sinks::{ManagedSink, Sink, WriteOne};

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

        let mut record = FutureRecord::to(&self.topic).key(&key).payload(&payload);
        let mut headers = OwnedHeaders::new();
        let mut has_trace_headers = false;
        for header_key in [TRACEPARENT, TRACESTATE] {
            if let Some(value) = env.meta.headers.get(header_key) {
                headers = headers.insert(Header {
                    key: header_key,
                    value: Some(value.as_str()),
                });
                has_trace_headers = true;
            }
        }
        if has_trace_headers {
            record = record.headers(headers);
        }
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

#[derive(Debug, Deserialize)]
struct KafkaSinkConfig {
    brokers: String,
    topic: String,
}

/// Registry factory for [`KafkaSink`]. Registered by
/// `courier::registry::register_builtin` under kind `"kafka"`.
///
/// Retry and error policy are managed centrally by the registry and applied
/// to every sink uniformly — no per-sink config needed.
pub fn kafka_sink_factory(
    id: &str,
    config: Value,
    on_error: ErrorPolicy,
    retry: Option<RetryPolicy>,
) -> Result<Box<dyn Sink>> {
    let config: KafkaSinkConfig = parse_config("kafka", config)?;
    if config.brokers.trim().is_empty() {
        bail!("invalid config for component type 'kafka': brokers must not be empty");
    }
    if config.topic.trim().is_empty() {
        bail!("invalid config for component type 'kafka': topic must not be empty");
    }
    let kafka = KafkaSink::new(id, &config.brokers, config.topic);
    let mut sink = ManagedSink::new(kafka).with_error_policy(on_error);
    if let Some(policy) = retry {
        sink = sink.with_retry(policy);
    }
    Ok(Box::new(sink))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdkafka::Message;
    use rdkafka::consumer::{Consumer, StreamConsumer};
    use serde_json::json;
    use testcontainers_modules::kafka::apache::{self, KAFKA_PORT};
    use testcontainers_modules::testcontainers::runners::AsyncRunner;

    use crate::sinks::WriteOne;

    #[test]
    fn factory_rejects_empty_topic() {
        let err = kafka_sink_factory(
            "kafka",
            serde_json::json!({
                "brokers": "localhost:9092",
                "topic": ""
            }),
            ErrorPolicy::Drop,
            None,
        )
        .err()
        .expect("expected empty topic to fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("topic must not be empty"), "{msg}");
    }

    #[tokio::test]
    async fn delivers_payload_and_key_with_source_id_fallback() -> anyhow::Result<()> {
        let node = apache::Kafka::default().start().await?;
        let host_port = node.get_host_port_ipv4(KAFKA_PORT).await?;
        let brokers = format!("127.0.0.1:{host_port}");

        let topic = "courier-sink-test";
        let sink = KafkaSink::new("kafka-sink", &brokers, topic);

        // Explicit meta.key -> used as record key.
        let mut e1 = Envelope::new("src-1", json!({ "hello": "world" }));
        e1.meta.key = Some("k-1".into());
        sink.write(&e1).await?;

        // No meta.key -> falls back to meta.source_id.
        let e2 = Envelope::new("src-2", json!({ "n": 42 }));
        assert!(e2.meta.key.is_none());
        sink.write(&e2).await?;

        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            .set("group.id", "courier-sink-test-consumer")
            .set("auto.offset.reset", "earliest")
            .set("enable.auto.commit", "false")
            .create()?;
        consumer.subscribe(&[topic])?;

        let mut received = Vec::new();
        for _ in 0..2 {
            let msg = tokio::time::timeout(Duration::from_secs(30), consumer.recv()).await??;
            let key = msg.key().map(|k| String::from_utf8_lossy(k).into_owned());
            let payload: serde_json::Value = serde_json::from_slice(msg.payload().unwrap())?;
            received.push((key, payload));
        }

        assert!(
            received
                .iter()
                .any(|(k, p)| k.as_deref() == Some("k-1") && p == &json!({ "hello": "world" })),
            "missing explicit-key message in {received:?}",
        );
        assert!(
            received
                .iter()
                .any(|(k, p)| k.as_deref() == Some("src-2") && p == &json!({ "n": 42 })),
            "missing source-id-fallback message in {received:?}",
        );
        Ok(())
    }
}
