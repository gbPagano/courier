use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::config::{parse_config, redact_secret};
use crate::envelope::Envelope;
use crate::observability::NodeCtx;
use crate::observability::trace_context;
use crate::pipeline::ErrorPolicy;
use crate::transforms::Transform;

const RESERVED_PAYLOAD_KEYS: &[&str] = &["_batch_count", "_batch_first_timestamp_ms"];

/// Groups envelopes into batches by maximum count and/or maximum time window.
///
/// When either limit is reached, the batch is emitted as a single envelope
/// whose payload contains an array of the original payloads under the
/// configured `payload_key`.
///
/// # Meta derivation
/// The emitted envelope inherits `meta` from the first envelope in the batch.
/// Its `key` is set to `batch-{first_timestamp_ms}` so downstream sinks
/// that require a key have a deterministic value.
///
/// # Acknowledgement
/// Because channels are the acknowledgement boundary, the batched envelope
/// is acknowledged only when downstream work for it completes. This in turn
/// acknowledges every constituent envelope, since none are released until
/// the batch envelope is delivered to the next stage.
///
/// # Cancellation
/// By default (`flush_on_cancel = true`), any partial batch is flushed
/// immediately when the cancellation token fires. When set to `false`,
/// partial batches are dropped on cancellation.
pub struct BatchTransform {
    id: String,
    max_size: usize,
    max_delay: Option<Duration>,
    payload_key: String,
    flush_on_cancel: bool,
    node_ctx: NodeCtx,
}

impl BatchTransform {
    pub fn new(
        id: impl Into<String>,
        max_size: usize,
        max_delay_ms: Option<u64>,
        payload_key: impl Into<String>,
        flush_on_cancel: bool,
    ) -> Self {
        Self {
            id: id.into(),
            max_size,
            max_delay: max_delay_ms.map(Duration::from_millis),
            payload_key: payload_key.into(),
            flush_on_cancel,
            node_ctx: NodeCtx::noop(),
        }
    }

    async fn emit_batch(&self, batch: Vec<Envelope>, tx: &Sender<Envelope>) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        if is_reserved_payload_key(&self.payload_key) {
            self.record_failed(batch.len());
            anyhow::bail!(
                "batch: payload_key '{}' is reserved for batch metadata",
                self.payload_key
            );
        }

        let first_timestamp_ms = batch[0].meta.timestamp_ms;
        let mut meta = batch[0].meta.clone();
        meta.key = Some(format!("batch-{}", first_timestamp_ms));

        // Propagate trace context from the first envelope in the batch.
        if let Some(parent) = trace_context::extract(&batch[0].meta.headers) {
            trace_context::inject(&mut meta.headers, &parent);
        }

        let count = batch.len();
        let payloads: Vec<Value> = batch.into_iter().map(|e| e.payload).collect();
        let mut payload = serde_json::Map::with_capacity(3);
        payload.insert(self.payload_key.clone(), Value::Array(payloads));
        payload.insert("_batch_count".into(), serde_json::json!(count));
        payload.insert(
            "_batch_first_timestamp_ms".into(),
            serde_json::json!(first_timestamp_ms),
        );
        let payload = Value::Object(payload);

        let env = Envelope { meta, payload };

        // Propagate downstream closure so the batcher exits instead of
        // silently dropping every subsequent batch.
        tx.send(env).await.map_err(|_| {
            self.record_failed(count);
            anyhow::anyhow!("downstream closed")
        })?;
        self.record_processed(count);
        Ok(())
    }

    fn record_processed(&self, count: usize) {
        for _ in 0..count {
            self.node_ctx.record_processed();
        }
    }

    fn record_filtered(&self, count: usize) {
        for _ in 0..count {
            self.node_ctx.record_filtered();
        }
    }

    fn record_failed(&self, count: usize) {
        for _ in 0..count {
            self.node_ctx.record_failed();
        }
    }
}

#[async_trait]
impl Transform for BatchTransform {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        self.node_ctx = ctx;
    }

    async fn run(
        self: Box<Self>,
        mut rx: Receiver<Envelope>,
        tx: Sender<Envelope>,
        cancel: CancellationToken,
    ) {
        let id = self.id.clone();
        let ctx = self.node_ctx.clone();
        let mut batch: Vec<Envelope> = Vec::with_capacity(self.max_size);
        let mut deadline: Option<tokio::time::Instant> = None;

        loop {
            let timeout = if let Some(d) = deadline {
                let remaining = d.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    // Deadline already passed — flush now.
                    if let Err(e) = self.emit_batch(std::mem::take(&mut batch), &tx).await {
                        tracing::error!(node_id = %redact_secret(&id), error = %e, "batch emit failed");
                        break;
                    }
                    deadline = None;
                    continue;
                }
                Some(tokio::time::sleep(remaining))
            } else {
                None
            };

            tokio::select! {
                _ = cancel.cancelled() => {
                    if self.flush_on_cancel
                        && !batch.is_empty()
                        && let Err(e) = self.emit_batch(std::mem::replace(&mut batch, Vec::with_capacity(self.max_size)), &tx).await
                    {
                        tracing::error!(node_id = %redact_secret(&id), error = %e, "batch flush on cancel failed");
                    } else if !self.flush_on_cancel {
                        self.record_filtered(batch.len());
                    }
                    break;
                }

                maybe = rx.recv() => {
                    let Some(env) = maybe else {
                        // Upstream closed; flush any remaining batch.
                        if !batch.is_empty()
                            && let Err(e) = self.emit_batch(std::mem::replace(&mut batch, Vec::with_capacity(self.max_size)), &tx).await
                        {
                            tracing::error!(node_id = %redact_secret(&id), error = %e, "batch flush on upstream close failed");
                            break;
                        }
                        break;
                    };

                    let span = tracing::info_span!(
                        "courier.transform",
                        pipeline = %redact_secret(ctx.pipeline()),
                        node_id = %redact_secret(ctx.node_id()),
                        node_kind = %ctx.node_kind_str(),
                        envelope.source_id = %env.meta.source_id,
                        envelope.key = if ctx.log_keys() { env.meta.key.as_deref().unwrap_or("") } else { "" },
                    );
                    if let Some(parent) = trace_context::extract(&env.meta.headers) {
                        let _ = span.set_parent(parent);
                    }
                    let span_context = span.context();
                    let started = Instant::now();

                    batch.push(env);
                    ctx.record_stage_duration_ms(started.elapsed().as_secs_f64() * 1000.0);
                    if batch.len() >= self.max_size {
                        if let Err(e) = self.emit_batch(std::mem::replace(&mut batch, Vec::with_capacity(self.max_size)), &tx).await {
                            tracing::error!(node_id = %redact_secret(&id), error = %e, "batch emit failed");
                            break;
                        }
                        deadline = None;
                    } else if deadline.is_none() && self.max_delay.is_some() {
                        deadline = Some(tokio::time::Instant::now() + self.max_delay.unwrap());
                    }

                    // Continue the span context for the emitted batch envelope
                    // (handled inside emit_batch via trace_context::inject on headers).
                    let _ = span_context;
                }

                _ = async { timeout.unwrap().await }, if timeout.is_some() => {
                    if !batch.is_empty()
                        && let Err(e) = self.emit_batch(std::mem::replace(&mut batch, Vec::with_capacity(self.max_size)), &tx).await
                    {
                        tracing::error!(node_id = %redact_secret(&id), error = %e, "batch emit on timeout failed");
                        break;
                    }
                    deadline = None;
                }
            }
        }
    }
}

// ------------------------------------------------------------------
// Config + Factory
// ------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct BatchTransformConfig {
    max_size: usize,
    #[serde(default)]
    max_delay_ms: Option<u64>,
    #[serde(default = "default_payload_key")]
    payload_key: String,
    #[serde(default = "default_flush_on_cancel")]
    flush_on_cancel: bool,
}

fn default_payload_key() -> String {
    "items".into()
}

fn default_flush_on_cancel() -> bool {
    true
}

fn is_reserved_payload_key(key: &str) -> bool {
    RESERVED_PAYLOAD_KEYS.contains(&key)
}

/// Registry factory for [`BatchTransform`]. Registered by
/// `courier::registry::register_builtin` under kind `"batch"`.
pub fn batch_transform_factory(
    id: &str,
    config: Value,
    _on_error: ErrorPolicy,
) -> Result<Box<dyn Transform>> {
    let config: BatchTransformConfig = parse_config("batch", config)?;
    if config.max_size == 0 {
        anyhow::bail!("batch: max_size must be greater than 0");
    }
    if is_reserved_payload_key(&config.payload_key) {
        anyhow::bail!(
            "batch: payload_key '{}' is reserved for batch metadata",
            config.payload_key
        );
    }
    Ok(Box::new(BatchTransform::new(
        id,
        config.max_size,
        config.max_delay_ms,
        config.payload_key,
        config.flush_on_cancel,
    )))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::Registry;
    use crate::config::{ErrorPolicyConfig, TransformSpec};
    use crate::envelope::Envelope;
    use crate::observability::metrics::testing::{
        counter_sum, histogram_count, obs_handle_in_memory,
    };
    use crate::observability::{NodeCtx, NodeKind};

    #[tokio::test]
    async fn emits_batch_when_max_size_reached() {
        let (in_tx, in_rx) = mpsc::channel(10);
        let t = BatchTransform::new("t", 3, None, "items", true);
        let (out_tx, mut out_rx) = mpsc::channel(10);
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let h = tokio::spawn(async move {
            Box::new(t).run(in_rx, out_tx, cancel2).await;
        });

        for i in 0..3 {
            in_tx
                .send(Envelope::new("src", json!({ "i": i })))
                .await
                .unwrap();
        }

        let out = out_rx.recv().await.unwrap();
        assert_eq!(out.payload["items"].as_array().unwrap().len(), 3);
        assert_eq!(out.payload["_batch_count"], 3);

        drop(in_tx);
        let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
    }

    #[tokio::test]
    async fn emits_batch_on_timeout() {
        let t = BatchTransform::new("t", 10, Some(50), "items", true);
        let (in_tx, in_rx) = mpsc::channel(10);
        let (out_tx, mut out_rx) = mpsc::channel(10);
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let h = tokio::spawn(async move {
            Box::new(t).run(in_rx, out_tx, cancel2).await;
        });

        in_tx
            .send(Envelope::new("src", json!({ "i": 1 })))
            .await
            .unwrap();

        let out = tokio::time::timeout(Duration::from_millis(200), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.payload["items"].as_array().unwrap().len(), 1);

        drop(in_tx);
        let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
    }

    #[tokio::test]
    async fn flushes_partial_batch_on_upstream_close() {
        let t = BatchTransform::new("t", 10, None, "items", true);
        let (in_tx, in_rx) = mpsc::channel(10);
        let (out_tx, mut out_rx) = mpsc::channel(10);
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let h = tokio::spawn(async move {
            Box::new(t).run(in_rx, out_tx, cancel2).await;
        });

        in_tx
            .send(Envelope::new("src", json!({ "i": 1 })))
            .await
            .unwrap();
        in_tx
            .send(Envelope::new("src", json!({ "i": 2 })))
            .await
            .unwrap();
        drop(in_tx);

        let out = tokio::time::timeout(Duration::from_secs(1), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.payload["items"].as_array().unwrap().len(), 2);

        let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
    }

    #[tokio::test]
    async fn flushes_partial_batch_on_cancel() {
        let t = BatchTransform::new("t", 10, None, "items", true);
        let (in_tx, in_rx) = mpsc::channel(10);
        let (out_tx, mut out_rx) = mpsc::channel(10);
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let h = tokio::spawn(async move {
            Box::new(t).run(in_rx, out_tx, cancel2).await;
        });

        in_tx
            .send(Envelope::new("src", json!({ "i": 1 })))
            .await
            .unwrap();
        // Give the batch transform a chance to receive the item before cancelling.
        tokio::task::yield_now().await;
        cancel.cancel();

        let out = tokio::time::timeout(Duration::from_secs(1), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.payload["items"].as_array().unwrap().len(), 1);

        let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
    }

    #[tokio::test]
    async fn drops_partial_batch_on_cancel_when_flush_disabled() {
        let t = BatchTransform::new("t", 10, None, "items", false);
        let (in_tx, in_rx) = mpsc::channel(10);
        let (out_tx, mut out_rx) = mpsc::channel(10);
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let h = tokio::spawn(async move {
            Box::new(t).run(in_rx, out_tx, cancel2).await;
        });

        in_tx
            .send(Envelope::new("src", json!({ "i": 1 })))
            .await
            .unwrap();
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;
        assert!(result.is_err() || result.unwrap().is_none());

        let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
    }

    #[tokio::test]
    async fn empty_input_produces_nothing() {
        let t = BatchTransform::new("t", 3, None, "items", true);
        let (in_tx, in_rx) = mpsc::channel(10);
        let (out_tx, mut out_rx) = mpsc::channel(10);
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let h = tokio::spawn(async move {
            Box::new(t).run(in_rx, out_tx, cancel2).await;
        });

        drop(in_tx);

        let result = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;
        assert!(result.is_err() || result.unwrap().is_none());

        let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
    }

    #[test]
    fn factory_resolves_through_registry() {
        let registry = Registry::with_builtins();
        registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "batch".into(),
                    config: json!({ "max_size": 10 }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                },
            )
            .unwrap();
    }

    #[test]
    fn factory_rejects_zero_max_size() {
        let registry = Registry::with_builtins();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "batch".into(),
                    config: json!({ "max_size": 0 }),
                    on_error: None,
                },
            )
            .err()
            .expect("expected validation error");
        let msg = format!("{err:#}");
        assert!(msg.contains("max_size must be greater than 0"), "{msg}");
    }

    #[test]
    fn factory_rejects_reserved_payload_keys() {
        let registry = Registry::with_builtins();
        for payload_key in RESERVED_PAYLOAD_KEYS {
            let err = registry
                .build_transform(
                    "p/t0",
                    TransformSpec {
                        kind: "batch".into(),
                        config: json!({ "max_size": 10, "payload_key": payload_key }),
                        on_error: None,
                    },
                )
                .err()
                .expect("expected validation error");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("reserved for batch metadata"),
                "payload_key={payload_key}: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn records_metrics_for_emitted_batches() {
        let (handle, exporter) = obs_handle_in_memory();
        let mut t = BatchTransform::new("t", 3, None, "items", true);
        t.set_node_ctx(NodeCtx::for_node(
            "metrics",
            "metrics/t0",
            NodeKind::Transform,
            handle.clone(),
        ));
        let (in_tx, in_rx) = mpsc::channel(10);
        let (out_tx, mut out_rx) = mpsc::channel(10);
        let cancel = CancellationToken::new();
        let h = tokio::spawn(async move {
            Box::new(t).run(in_rx, out_tx, cancel).await;
        });

        for i in 0..3 {
            in_tx
                .send(Envelope::new("src", json!({ "i": i })))
                .await
                .unwrap();
        }

        let _ = out_rx.recv().await.unwrap();
        drop(in_tx);
        let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
        handle.shutdown();

        let attrs = &[("pipeline", "metrics"), ("node_id", "metrics/t0")];
        assert_eq!(
            counter_sum(&exporter, "courier_envelopes_processed_total", attrs),
            3
        );
        assert_eq!(
            histogram_count(&exporter, "courier_stage_duration_milliseconds", attrs),
            3
        );
    }

    #[tokio::test]
    async fn records_filtered_metrics_for_dropped_cancel_batch() {
        let (handle, exporter) = obs_handle_in_memory();
        let mut t = BatchTransform::new("t", 10, None, "items", false);
        t.set_node_ctx(NodeCtx::for_node(
            "metrics",
            "metrics/t0",
            NodeKind::Transform,
            handle.clone(),
        ));
        let (in_tx, in_rx) = mpsc::channel(10);
        let (out_tx, mut out_rx) = mpsc::channel(10);
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let h = tokio::spawn(async move {
            Box::new(t).run(in_rx, out_tx, cancel2).await;
        });

        in_tx
            .send(Envelope::new("src", json!({ "i": 1 })))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;
        assert!(result.is_err() || result.unwrap().is_none());
        drop(in_tx);
        let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
        handle.shutdown();

        let attrs = &[("pipeline", "metrics"), ("node_id", "metrics/t0")];
        assert_eq!(
            counter_sum(&exporter, "courier_envelopes_filtered_total", attrs),
            1
        );
        assert_eq!(
            counter_sum(&exporter, "courier_envelopes_processed_total", attrs),
            0
        );
    }

    #[tokio::test]
    async fn records_failed_metrics_when_emit_fails() {
        let (handle, exporter) = obs_handle_in_memory();
        let mut t = BatchTransform::new("t", 2, None, "items", true);
        t.set_node_ctx(NodeCtx::for_node(
            "metrics",
            "metrics/t0",
            NodeKind::Transform,
            handle.clone(),
        ));
        let (in_tx, in_rx) = mpsc::channel(10);
        let (out_tx, out_rx) = mpsc::channel(10);
        let cancel = CancellationToken::new();
        let h = tokio::spawn(async move {
            Box::new(t).run(in_rx, out_tx, cancel).await;
        });

        drop(out_rx);
        for i in 0..2 {
            in_tx
                .send(Envelope::new("src", json!({ "i": i })))
                .await
                .unwrap();
        }

        let _ = tokio::time::timeout(Duration::from_secs(1), h)
            .await
            .unwrap();
        handle.shutdown();

        let attrs = &[("pipeline", "metrics"), ("node_id", "metrics/t0")];
        assert_eq!(
            counter_sum(&exporter, "courier_envelopes_failed_total", attrs),
            2
        );
        assert_eq!(
            counter_sum(&exporter, "courier_envelopes_processed_total", attrs),
            0
        );
    }

    #[tokio::test]
    async fn stops_when_downstream_closes() {
        let t = BatchTransform::new("t", 2, None, "items", true);
        let (in_tx, in_rx) = mpsc::channel(10);
        let (out_tx, out_rx) = mpsc::channel(10);
        let cancel = CancellationToken::new();
        let h = tokio::spawn(async move {
            Box::new(t).run(in_rx, out_tx, cancel.clone()).await;
        });

        // Send one item — not enough to trigger a batch yet.
        in_tx
            .send(Envelope::new("src", json!({ "i": 1 })))
            .await
            .unwrap();

        // Drop the downstream receiver to simulate sink exit.
        drop(out_rx);

        // Send a second item — now the batcher tries to emit and sees
        // downstream is closed. It should stop instead of looping forever.
        in_tx
            .send(Envelope::new("src", json!({ "i": 2 })))
            .await
            .unwrap();

        // The batcher task should exit promptly.
        let _ = tokio::time::timeout(Duration::from_secs(1), h)
            .await
            .unwrap();
    }
}
