//! Courier — async pipelines composed as `Source → Transform* → Sink[]`.
//!
//! Nodes communicate via `tokio::mpsc` channels of `Envelope`. Each node
//! runs as its own task; the shared `CancellationToken` triggers a
//! graceful drain on SIGINT/Ctrl+C.

use futures::future;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub mod envelope;
pub mod pipeline;
pub mod sinks;
pub mod sources;
pub mod transforms;

use pipeline::{Pipeline, spawn_pipeline};

/// Top-level runtime. Owns every pipeline; `run` blocks until all of them
/// exit (cancellation, upstream closure, or unrecoverable error).
pub struct Courier {
    pipelines: Vec<Pipeline>,
}

impl Courier {
    pub fn new(pipelines: Vec<Pipeline>) -> Self {
        Self { pipelines }
    }

    /// Spawn every pipeline as tokio tasks under the given cancel token.
    /// Caller is responsible for awaiting the returned handles and firing
    /// the token on shutdown. `run` wraps this with a SIGINT handler.
    pub fn spawn(self, cancel: CancellationToken) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        for p in self.pipelines {
            handles.extend(spawn_pipeline(p, cancel.clone()));
        }
        handles
    }

    pub async fn run(self) {
        let cancel = CancellationToken::new();

        let signal_cancel = cancel.clone();
        tokio::spawn(async move {
            match tokio::signal::ctrl_c().await {
                Ok(_) => {
                    log::info!("received shutdown signal, cancelling pipelines");
                    signal_cancel.cancel();
                }
                Err(e) => log::error!("failed to listen for shutdown signal: {e}"),
            }
        });

        let handles = self.spawn(cancel);
        future::join_all(handles).await;
    }
}
