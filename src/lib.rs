//! Courier — async pipelines composed as `Source → Transform* → Sink[]`.
//!
//! Nodes communicate via `tokio::mpsc` channels of `Envelope`. Each node
//! runs as its own task; the shared `CancellationToken` triggers a
//! graceful drain on SIGINT/Ctrl+C.

use futures::future;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::ObservabilityConfig;
use crate::observability::ObsHandle;

pub mod cli;
pub mod config;
pub mod envelope;
pub mod observability;
pub mod pipeline;
pub mod registry;
pub mod retry;
pub mod sinks;
pub mod sources;
pub mod transforms;

pub use registry::{Registry, register_builtin};
pub use retry::{ExhaustedPolicy, RetryPolicy};
pub use sinks::ManagedSink;

use pipeline::{Pipeline, spawn_pipeline};

/// Top-level runtime. Owns every pipeline; `run` blocks until all of them
/// exit (cancellation, upstream closure, or unrecoverable error).
pub struct Courier {
    pipelines: Vec<Pipeline>,
    /// Parsed observability config carried through from `Config`. Stored
    /// here so subsequent PRs can wire OTLP exporters and force-flush
    /// providers on shutdown without changing this signature again.
    /// `None` means "use built-in defaults".
    observability: Option<ObservabilityConfig>,
    metrics: ObsHandle,
}

impl Courier {
    pub fn new(pipelines: Vec<Pipeline>) -> Self {
        Self {
            pipelines,
            observability: None,
            metrics: ObsHandle::noop(),
        }
    }

    /// Attach the observability config parsed from `[observability]`.
    /// Builder shape so tests and `Registry::build_courier` keep using
    /// `Courier::new(...)` without a forced extra argument.
    pub fn with_observability(mut self, observability: Option<ObservabilityConfig>) -> Self {
        self.observability = observability;
        self
    }

    pub fn observability(&self) -> Option<&ObservabilityConfig> {
        self.observability.as_ref()
    }

    pub(crate) fn with_metrics(mut self, metrics: ObsHandle) -> Self {
        self.metrics = metrics;
        self
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
        let metrics = self.metrics.clone();

        let signal_cancel = cancel.clone();
        let signal_metrics = metrics.clone();
        tokio::spawn(async move {
            match tokio::signal::ctrl_c().await {
                Ok(_) => {
                    log::info!("received shutdown signal, cancelling pipelines");
                    signal_cancel.cancel();
                    signal_metrics.force_flush();
                    crate::observability::force_flush_traces();
                }
                Err(e) => log::error!("failed to listen for shutdown signal: {e}"),
            }
        });

        let handles = self.spawn(cancel);
        future::join_all(handles).await;
        metrics.shutdown();
        crate::observability::shutdown_traces();
    }
}
