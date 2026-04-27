//! Source-side helpers for creating root spans and stamping trace context.

use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::envelope::Envelope;
use crate::observability::trace_context;

#[derive(Clone)]
pub struct SourceCtx {
    node_id: String,
}

impl SourceCtx {
    pub fn new(node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
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
            node_id = %self.node_id,
            node_kind = "source",
            envelope.source_id = %env.meta.source_id,
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
