use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use self::retry::WriteOutcome;
use crate::config::redact_secret;
use crate::envelope::Envelope;
use crate::observability::NodeCtx;
use crate::observability::trace_context;
use crate::pipeline::ErrorPolicy;
use crate::retry::RetryPolicy;

pub mod api;
pub mod file;
pub mod kafka;
mod retry;
pub mod sql;

/// Full-control sink: owns the receiver loop.
///
/// Implement directly when the sink needs buffering, background work, or
/// a custom shutdown drain. Most sinks should implement `WriteOne` instead
/// and wrap themselves in `ManagedSink`.
#[async_trait]
pub trait Sink: Send + Sync {
    fn id(&self) -> &str;

    /// Attach the per-node observability context. Called by
    /// `spawn_pipeline` after the sink is built but before it runs.
    /// Default no-op — full-control sinks that want metrics override
    /// this and store the ctx; `ManagedSink` already does so.
    fn set_node_ctx(&mut self, _ctx: NodeCtx) {}

    async fn run(self: Box<Self>, rx: Receiver<Envelope>, cancel: CancellationToken);
}

/// Ergonomic sink: "write one envelope, report result".
///
/// `ManagedSink` turns a `WriteOne` into a `Sink` with the standard recv
/// loop, cancellation, error policy, and optional retry with exponential
/// back-off. Cross-cutting wrappers (rate limiting, batching) compose over
/// `WriteOne` the same way and plug into `ManagedSink` unchanged.
#[async_trait]
pub trait WriteOne: Send + Sync {
    fn id(&self) -> &str;

    async fn write(&self, env: &Envelope) -> Result<()>;
}

/// Adapter that turns any [`WriteOne`] into a [`Sink`].
///
/// Manages the recv loop, graceful cancellation, error policy, and optional
/// retry with exponential back-off and a configurable exhaustion policy.
pub struct ManagedSink<W: WriteOne> {
    pub inner: W,
    pub on_error: ErrorPolicy,
    pub retry: Option<RetryPolicy>,
    node_ctx: NodeCtx,
}

impl<W: WriteOne> ManagedSink<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            on_error: ErrorPolicy::Drop,
            retry: None,
            node_ctx: NodeCtx::noop(),
        }
    }

    pub fn with_error_policy(mut self, policy: ErrorPolicy) -> Self {
        self.on_error = policy;
        self
    }

    pub fn with_retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = Some(policy);
        self
    }
}

#[async_trait]
impl<W: WriteOne + 'static> Sink for ManagedSink<W> {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        self.node_ctx = ctx;
    }

    async fn run(self: Box<Self>, mut rx: Receiver<Envelope>, cancel: CancellationToken) {
        let id = self.inner.id().to_string();
        let ctx = self.node_ctx;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!(node_id = %redact_secret(&id), reason = "cancel", "sink drained on cancellation");
                    break;
                }
                maybe = rx.recv() => {
                    let Some(mut env) = maybe else {
                        tracing::debug!(node_id = %redact_secret(&id), reason = "upstream_closed", "sink loop ending");
                        break;
                    };
                    let span = tracing::info_span!(
                        "courier.sink",
                        pipeline = %redact_secret(ctx.pipeline()),
                        node_id = %redact_secret(ctx.node_id()),
                        node_kind = %ctx.node_kind_str(),
                        envelope.source_id = %env.meta.source_id,
                        envelope.key = if ctx.log_keys() { env.meta.key.as_deref().unwrap_or("") } else { "" },
                    );
                    if let Some(parent) = trace_context::extract(&env.meta.headers) {
                        let _ = span.set_parent(parent);
                    }
                    trace_context::inject(&mut env.meta.headers, &span.context());
                    let started = Instant::now();
                    let result = async {
                        match &self.retry {
                            Some(policy) => retry::write_with_retry(&self.inner, &env, policy, &ctx).await,
                            None => self.inner.write(&env).await.map(|()| WriteOutcome::Written),
                        }
                    }
                    .instrument(span)
                    .await;
                    ctx.record_stage_duration_ms(started.elapsed().as_secs_f64() * 1000.0);

                    match result {
                        Ok(WriteOutcome::Written) => {
                            ctx.record_processed();
                            if let Some(latency_ms) = end_to_end_latency_ms(&env) {
                                ctx.record_end_to_end_latency_ms(latency_ms);
                            }
                        }
                        Ok(WriteOutcome::DeadLettered) => {
                            ctx.record_failed();
                        }
                        Err(e) => {
                            ctx.record_failed();
                            match &self.on_error {
                                ErrorPolicy::Drop => {
                                    tracing::error!(node_id = %redact_secret(&id), error = %e, "write failed, dropping");
                                }
                                ErrorPolicy::FailPipeline => {
                                    tracing::error!(node_id = %redact_secret(&id), error = %e, "write failed, failing pipeline");
                                    cancel.cancel();
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Best-effort `now - env.meta.timestamp_ms`. `Envelope::new` always
/// stamps a timestamp, but a custom source could build an envelope
/// directly from `Meta::default()` (where `timestamp_ms` is 0) — that
/// case and any future-skewed clock are skipped.
fn end_to_end_latency_ms(env: &Envelope) -> Option<f64> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    let started_ms = env.meta.timestamp_ms;
    if started_ms == 0 || started_ms > now_ms {
        return None;
    }
    Some((now_ms - started_ms) as f64)
}

#[cfg(test)]
mod tests {
    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::observability::NodeKind;
    use crate::observability::metrics::testing::{
        counter_sum, histogram_count, obs_handle_in_memory,
    };
    use crate::retry::{ExhaustedPolicy, RetryPolicy};

    struct AlwaysFailSink;

    #[async_trait]
    impl WriteOne for AlwaysFailSink {
        fn id(&self) -> &str {
            "fail"
        }

        async fn write(&self, _env: &Envelope) -> Result<()> {
            Err(anyhow!("simulated failure"))
        }
    }

    fn no_delay_dead_letter_policy(path: std::path::PathBuf) -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            initial_delay_ms: 1,
            backoff_multiplier: 1.0,
            max_delay_ms: 1,
            on_exhausted: ExhaustedPolicy::DeadLetter { path },
        }
    }

    #[tokio::test]
    async fn dead_lettered_sink_write_is_not_recorded_as_processed() {
        let (handle, exporter) = obs_handle_in_memory();
        let ctx = NodeCtx::for_node("p", "p/sink0", NodeKind::Sink, handle.clone());
        let dir = tempfile::tempdir().unwrap();
        let policy = no_delay_dead_letter_policy(dir.path().join("dead.jsonl"));
        let mut sink = ManagedSink::new(AlwaysFailSink).with_retry(policy);
        sink.set_node_ctx(ctx);

        let (tx, rx) = mpsc::channel(1);
        tx.send(Envelope::new("src", json!({"x": 1})))
            .await
            .unwrap();
        drop(tx);

        Box::new(sink).run(rx, CancellationToken::new()).await;
        handle.shutdown();

        let attrs = &[("pipeline", "p"), ("node_id", "p/sink0")];
        assert_eq!(
            counter_sum(&exporter, "courier_envelopes_processed_total", attrs),
            0
        );
        assert_eq!(
            counter_sum(&exporter, "courier_envelopes_failed_total", attrs),
            1
        );
        assert_eq!(
            counter_sum(&exporter, "courier_dead_lettered_total", attrs),
            1
        );
        assert_eq!(
            histogram_count(&exporter, "courier_end_to_end_latency_milliseconds", attrs),
            0
        );
        assert_eq!(
            histogram_count(&exporter, "courier_stage_duration_milliseconds", attrs),
            1
        );
    }
}
