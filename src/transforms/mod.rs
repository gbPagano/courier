use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::config::redact_secret;
use crate::envelope::Envelope;
use crate::observability::NodeCtx;
use crate::observability::trace_context;
use crate::pipeline::ErrorPolicy;

pub mod batch;
pub mod filter;
pub mod mutate;
pub mod script;
pub mod set_key;

/// Full-control transform: owns both channels.
///
/// Implement directly for cardinality changes or stateful work:
/// batching (`N -> 1`), flat-map (`1 -> N`), joins, windowed aggregation.
/// For plain `1 -> 0-or-1` transforms, implement `MapOne` instead.
#[async_trait]
pub trait Transform: Send + Sync {
    fn id(&self) -> &str;

    /// Attach the per-node observability context. Called by
    /// `spawn_pipeline` after the transform is built but before it
    /// runs. Default no-op — full-control transforms that want
    /// metrics override this and store the ctx; `BasicTransform`
    /// already does so for the common path.
    fn set_node_ctx(&mut self, _ctx: NodeCtx) {}

    async fn run(
        self: Box<Self>,
        rx: Receiver<Envelope>,
        tx: Sender<Envelope>,
        cancel: CancellationToken,
    );
}

/// Ergonomic transform: "receive one, return at most one".
///
/// Returning `Ok(None)` filters the envelope out. `BasicTransform` wraps
/// a `MapOne` into a `Transform` with the standard loop and error policy.
#[async_trait]
pub trait MapOne: Send + Sync {
    fn id(&self) -> &str;

    async fn map(&self, env: Envelope) -> Result<Option<Envelope>>;

    /// Hook called by `BasicTransform::set_node_ctx` so wrapped maps can
    /// record per-runtime metrics. Default no-op — script engines use it
    /// to pre-build counter recorders.
    fn set_node_ctx(&mut self, _ctx: NodeCtx) {}
}

/// Adapter that turns any `MapOne` into a `Transform`.
pub struct BasicTransform<M: MapOne> {
    pub inner: M,
    pub on_error: ErrorPolicy,
    node_ctx: NodeCtx,
}

impl<M: MapOne> BasicTransform<M> {
    pub fn new(inner: M) -> Self {
        Self {
            inner,
            on_error: ErrorPolicy::Drop,
            node_ctx: NodeCtx::noop(),
        }
    }

    pub fn with_error_policy(mut self, policy: ErrorPolicy) -> Self {
        self.on_error = policy;
        self
    }
}

#[async_trait]
impl<M: MapOne + 'static> Transform for BasicTransform<M> {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        self.inner.set_node_ctx(ctx.clone());
        self.node_ctx = ctx;
    }

    async fn run(
        self: Box<Self>,
        mut rx: Receiver<Envelope>,
        tx: Sender<Envelope>,
        cancel: CancellationToken,
    ) {
        let id = self.inner.id().to_string();
        let ctx = self.node_ctx;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe = rx.recv() => {
                    let Some(env) = maybe else { break };
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
                    let result = self.inner.map(env).instrument(span.clone()).await;
                    ctx.record_stage_duration_ms(started.elapsed().as_secs_f64() * 1000.0);
                    match result {
                        Ok(Some(mut out)) => {
                            ctx.record_processed();
                            trace_context::inject(&mut out.meta.headers, &span_context);
                            if tx.send(out).await.is_err() {
                                tracing::debug!(node_id = %redact_secret(&id), "downstream closed");
                                return;
                            }
                        }
                        Ok(None) => {
                            ctx.record_filtered();
                        }
                        Err(e) => {
                            ctx.record_failed();
                            match &self.on_error {
                                ErrorPolicy::Drop => {
                                    tracing::error!(node_id = %redact_secret(&id), error = %e, "map failed, dropping");
                                }
                                ErrorPolicy::FailPipeline => {
                                    tracing::error!(node_id = %redact_secret(&id), error = %e, "map failed, failing pipeline");
                                    ctx.mark_pipeline_failed();
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
