//! Source-side helpers for creating root spans and stamping trace context.

use std::time::Instant;

use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::envelope::Envelope;
use crate::observability::NodeCtx;
use crate::observability::trace_context;

#[derive(Clone)]
pub struct SourceCtx {
    node_ctx: NodeCtx,
}

impl SourceCtx {
    pub fn new(node_id: impl Into<String>) -> Self {
        let node_id = node_id.into();
        Self {
            node_ctx: NodeCtx::for_node(
                "",
                &node_id,
                crate::observability::NodeKind::Source,
                crate::observability::ObsHandle::noop(),
            ),
        }
    }

    pub fn from_node_ctx(ctx: NodeCtx) -> Self {
        Self { node_ctx: ctx }
    }

    pub async fn send(
        &self,
        tx: &Sender<Envelope>,
        mut env: Envelope,
        cancel: &CancellationToken,
    ) -> Result<(), SendStopped> {
        let span = tracing::info_span!(
            "courier.source",
            pipeline = %self.node_ctx.pipeline(),
            node_id = %self.node_ctx.node_id(),
            node_kind = "source",
            envelope.source_id = %env.meta.source_id,
            envelope.key = if self.node_ctx.log_keys() { env.meta.key.as_deref().unwrap_or("") } else { "" },
        );
        if let Some(parent) = trace_context::extract(&env.meta.headers) {
            let _ = span.set_parent(parent);
        }

        let span_for_context = span.clone();
        let started = Instant::now();
        let result = async move {
            trace_context::inject(&mut env.meta.headers, &span_for_context.context());
            tokio::select! {
                _ = cancel.cancelled() => Err(SendStopped::Cancelled),
                res = tx.send(env) => res.map_err(|_| SendStopped::DownstreamClosed),
            }
        }
        .instrument(span)
        .await;

        self.node_ctx
            .record_stage_duration_ms(started.elapsed().as_secs_f64() * 1000.0);
        if result.is_ok() {
            self.node_ctx.record_processed();
        }
        result
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SendStopped {
    Cancelled,
    DownstreamClosed,
}
