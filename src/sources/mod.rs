use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::envelope::Envelope;
use crate::observability::NodeCtx;

pub mod api;
pub mod http_webhook;
pub mod jsonl_file;
pub mod kafka;
mod retry;
pub mod sql;

/// A pipeline source.
///
/// Drives its own cadence (polling, streaming, event-driven) and pushes
/// envelopes into `tx`. When `cancel` fires, the implementation must exit
/// promptly; dropping `tx` on exit signals downstream stages to drain.
#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> &str;

    /// Attach the per-node observability context. Called by
    /// `spawn_pipeline` after the source is built but before it runs.
    /// Default no-op for custom sources that do not use `SourceCtx`.
    fn set_node_ctx(&mut self, _ctx: NodeCtx) {}

    async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken);
}
