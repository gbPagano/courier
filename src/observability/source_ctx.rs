//! Source-side helpers for creating root spans and stamping trace context.

use std::sync::Arc;

use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::envelope::Envelope;
use crate::observability::NodeCtx;
use crate::observability::trace_context;

#[derive(Clone)]
pub struct SourceCtx {
    pipeline: Arc<str>,
    node_id: Arc<str>,
    log_keys: bool,
}

impl SourceCtx {
    pub fn new(node_id: impl Into<String>) -> Self {
        let node_id = node_id.into();
        Self {
            pipeline: Arc::from(""),
            node_id: Arc::from(node_id),
            log_keys: false,
        }
    }

    pub fn from_node_ctx(ctx: NodeCtx) -> Self {
        Self {
            pipeline: Arc::from(ctx.pipeline()),
            node_id: Arc::from(ctx.node_id()),
            log_keys: ctx.log_keys(),
        }
    }

    pub async fn send(
        &self,
        tx: &Sender<Envelope>,
        mut env: Envelope,
        cancel: &CancellationToken,
    ) -> Result<(), SendStopped> {
        let span = tracing::info_span!(
            "courier.source",
            pipeline = %self.pipeline,
            node_id = %self.node_id,
            node_kind = "source",
            envelope.source_id = %env.meta.source_id,
            envelope.key = if self.log_keys { env.meta.key.as_deref().unwrap_or("") } else { "" },
        );
        if let Some(parent) = trace_context::extract(&env.meta.headers) {
            let _ = span.set_parent(parent);
        }

        let span_for_context = span.clone();
        async move {
            trace_context::inject(&mut env.meta.headers, &span_for_context.context());
            tokio::select! {
                _ = cancel.cancelled() => Err(SendStopped::Cancelled),
                res = tx.send(env) => res.map_err(|_| SendStopped::DownstreamClosed),
            }
        }
        .instrument(span)
        .await
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SendStopped {
    Cancelled,
    DownstreamClosed,
}
