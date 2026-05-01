use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use rdkafka::Message;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::Headers;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::config::{parse_config, redact_secret};
use crate::envelope::Envelope;
use crate::observability::trace_context::{TRACEPARENT, TRACESTATE};
use crate::observability::{NodeCtx, SendStopped, SourceCtx};
use crate::retry::RetryPolicy;
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
    source_ctx: SourceCtx,
}

impl KafkaSource {
    pub fn new(
        id: impl Into<String>,
        brokers: &str,
        group_id: &str,
        topics: Vec<&str>,
    ) -> Result<Self> {
        let id = id.into();
        let consumer: StreamConsumer = ClientConfig::new()
            .set("group.id", group_id)
            .set("bootstrap.servers", brokers)
            .set("enable.partition.eof", "false")
            .set("session.timeout.ms", "6000")
            .set("enable.auto.commit", "false")
            .create()
            .context("failed to create kafka consumer")?;

        consumer
            .subscribe(&topics)
            .context("failed to subscribe kafka consumer to specified topics")?;

        Ok(Self {
            source_ctx: SourceCtx::new(&id),
            id,
            consumer,
        })
    }
}

#[async_trait]
impl Source for KafkaSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        self.source_ctx = SourceCtx::from_node_ctx(ctx);
    }

    async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
        log::info!("[{}] starting kafka consumer", redact_secret(&self.id));
        let source_ctx = self.source_ctx.clone();

        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => {
                    log::info!("[{}] cancelled", redact_secret(&self.id));
                    return;
                }
                result = self.consumer.recv() => match result {
                    Ok(m) => m,
                    Err(e) => {
                        log::error!("[{}] kafka recv error: {e}", redact_secret(&self.id));
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
                .map(str::to_owned);

            let payload_bytes = match msg.payload() {
                Some(p) => p,
                None => {
                    log::error!(
                        "[{}] message at offset {offset} has no payload",
                        redact_secret(&self.id)
                    );
                    continue;
                }
            };

            let payload: Value = match serde_json::from_slice(payload_bytes) {
                Ok(v) => v,
                Err(e) => {
                    log::error!(
                        "[{}] failed to deserialize at offset {offset}: {e}",
                        redact_secret(&self.id),
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
            if let Some(headers) = msg.headers() {
                for header in headers.iter() {
                    if matches!(header.key, TRACEPARENT | TRACESTATE)
                        && let Some(value) = header.value.and_then(|v| std::str::from_utf8(v).ok())
                    {
                        env.meta
                            .headers
                            .insert(header.key.to_string(), value.to_string());
                    }
                }
            }

            log::debug!(
                "[{}] received topic={topic} partition={partition} offset={offset}",
                redact_secret(&self.id),
                topic = redact_secret(&topic),
            );

            match source_ctx.send(&tx, env, &cancel).await {
                Ok(()) => {}
                Err(SendStopped::Cancelled) => return,
                Err(SendStopped::DownstreamClosed) => {
                    log::info!("[{}] downstream closed, stopping", redact_secret(&self.id));
                    return;
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct KafkaSourceConfig {
    brokers: String,
    group_id: String,
    topics: Vec<String>,
}

/// Registry factory for [`KafkaSource`]. Registered by
/// `courier::registry::register_builtin` under kind `"kafka"`.
///
/// `retry` is rejected at config time: kafka consumers are push-based
/// (the broker drives delivery), so a retry/backoff knob has no role here.
pub fn kafka_source_factory(
    id: &str,
    config: Value,
    retry: Option<RetryPolicy>,
) -> Result<Box<dyn Source>> {
    if retry.is_some() {
        bail!(
            "invalid config for component type 'kafka': retry has no effect on push-based sources"
        );
    }
    let config: KafkaSourceConfig = parse_config("kafka", config)?;
    if config.brokers.trim().is_empty() {
        bail!("invalid config for component type 'kafka': brokers must not be empty");
    }
    if config.group_id.trim().is_empty() {
        bail!("invalid config for component type 'kafka': group_id must not be empty");
    }
    if config.topics.is_empty() {
        bail!("invalid config for component type 'kafka': topics must not be empty");
    }
    if let Some(index) = config
        .topics
        .iter()
        .position(|topic| topic.trim().is_empty())
    {
        bail!("invalid config for component type 'kafka': topics[{index}] must not be empty");
    }
    let topics: Vec<_> = config.topics.iter().map(String::as_str).collect();
    Ok(Box::new(KafkaSource::new(
        id,
        &config.brokers,
        &config.group_id,
        topics,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use rdkafka::message::{Header, OwnedHeaders};
    use rdkafka::producer::{FutureProducer, FutureRecord};
    use serde_json::json;
    use testcontainers_modules::kafka::apache::{self, KAFKA_PORT};
    use testcontainers_modules::testcontainers::runners::AsyncRunner;
    use tokio::sync::mpsc;

    #[test]
    fn factory_rejects_empty_topics() {
        let err = kafka_source_factory(
            "kafka",
            serde_json::json!({
                "brokers": "localhost:9092",
                "group_id": "courier",
                "topics": []
            }),
            None,
        )
        .err()
        .expect("expected empty topics to fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("topics must not be empty"), "{msg}");
    }

    #[test]
    fn factory_rejects_retry_policy() {
        use crate::retry::{ExhaustedPolicy, RetryPolicy};

        let err = kafka_source_factory(
            "kafka",
            serde_json::json!({
                "brokers": "localhost:9092",
                "group_id": "courier",
                "topics": ["t"]
            }),
            Some(RetryPolicy {
                max_attempts: 3,
                initial_delay_ms: 100,
                backoff_multiplier: 2.0,
                max_delay_ms: 1000,
                on_exhausted: ExhaustedPolicy::Propagate,
            }),
        )
        .err()
        .expect("expected retry rejection");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("retry has no effect on push-based sources"),
            "{msg}"
        );
    }

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

        let source = KafkaSource::new("src", &brokers, "courier-source-group", vec![topic])?;
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
                        FutureRecord::to(topic).key("k-1").payload(payload).headers(
                            OwnedHeaders::new().insert(Header {
                                key: TRACEPARENT,
                                value: Some(
                                    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                                ),
                            }),
                        ),
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
        assert_eq!(
            env.meta.headers.get(TRACEPARENT).map(String::as_str),
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        );
        Ok(())
    }
}
