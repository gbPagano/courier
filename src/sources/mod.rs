use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::envelope::Envelope;

pub mod api;
pub mod http_webhook;
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

    async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken);
}
