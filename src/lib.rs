//! Courier — async pipelines composed as `Source → Transform* → Sink[]`.
//!
//! Nodes communicate via `tokio::mpsc` channels of `Envelope`. Each node
//! runs as its own task; the shared `CancellationToken` triggers a
//! graceful drain on SIGINT/Ctrl+C.
//!
//! # Runtime lifecycle
//!
//! Courier exposes liveness and readiness health probes suitable for
//! container orchestrators. The health server (enabled via `[health]`
//! config) serves:
//!
//! - **GET /health/live** → 200 when the process is alive.
//! - **GET /health/ready** → 200 when all pipelines are running; 503
//!   otherwise (starting, failed, draining, or stopped pipelines).
//!
//! Each pipeline transitions through: `Starting → Running → Draining → Stopped`
//! (or `Starting → Running → Failed` for unrecoverable errors).
//!
//! ## Shutdown and in-flight envelopes
//!
//! On SIGINT/Ctrl+C, Courier transitions all pipelines to `Draining`,
//! cancels the shared `CancellationToken`, and waits up to
//! `shutdown.timeout_secs` (default 30) for in-flight envelopes to
//! drain through sinks. Sources stop pulling, transforms finish their
//! current item, and sinks drain their channel receivers until upstream
//! closes. If the timeout expires, remaining tasks are orphaned — they
//! keep running until the process exits, because dropping a `JoinHandle`
//! does not abort the underlying tokio task.
//!
//! ## Process exit codes
//!
//! - **0** — clean shutdown (SIGINT, all pipelines drained).
//! - **1** — configuration or startup error, or one or more pipelines
//!   failed with `FailPipeline`.

use std::sync::Arc;
use std::time::Duration;

use futures::future;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::ObservabilityConfig;
use crate::lifecycle::CourierState;
use crate::observability::ObsHandle;

pub mod cli;
pub mod config;
pub mod envelope;
pub(crate) mod health;
pub(crate) mod lifecycle;
pub mod observability;
pub mod pipeline;
pub mod registry;
pub mod retry;
pub mod sinks;
pub mod sources;
pub mod transforms;

pub use envelope::{DeadLetterEntry, Envelope, Meta};
pub use lifecycle::{PipelineState, PipelineStatus};
pub use registry::{Registry, register_builtin};
pub use retry::{ExhaustedPolicy, RetryPolicy};
pub use sinks::ManagedSink;

use pipeline::{Pipeline, spawn_pipeline};

/// Outcome of `Courier::run()`, used to set the process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// All pipelines shut down cleanly.
    Success,
    /// One or more pipelines failed with `FailPipeline`.
    Failed,
}

/// Top-level runtime. Owns every pipeline; `run` blocks until all of them
/// exit (cancellation, upstream closure, or unrecoverable error).
pub struct Courier {
    pipelines: Vec<Pipeline>,
    observability: Option<ObservabilityConfig>,
    metrics: ObsHandle,
    shutdown_timeout: Duration,
    health_address: Option<std::net::SocketAddr>,
}

impl Courier {
    pub fn new(pipelines: Vec<Pipeline>) -> Self {
        Self {
            pipelines,
            observability: None,
            metrics: ObsHandle::noop(),
            shutdown_timeout: Duration::from_secs(30),
            health_address: None,
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

    /// Set the maximum time to wait for in-flight envelopes to drain
    /// after a shutdown signal. Defaults to 30 seconds.
    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Set the bind address for the health probe HTTP server.
    /// When set, `/health/live` and `/health/ready` are served on this
    /// address. When `None`, no health server is started.
    pub fn with_health_address(mut self, addr: Option<std::net::SocketAddr>) -> Self {
        self.health_address = addr;
        self
    }

    /// Spawn every pipeline as tokio tasks under the given cancel token.
    /// Caller is responsible for awaiting the returned handles and firing
    /// the token on shutdown. `run` wraps this with a SIGINT handler and
    /// a graceful drain timeout.
    ///
    /// Returns the handles plus the shared `CourierState`. After all handles
    /// complete, call `state.has_failures()` to check whether any pipeline
    /// hit `FailPipeline`. Use `run` when you want this wired up automatically
    /// with a `RunOutcome` return value.
    pub fn spawn(self, cancel: CancellationToken) -> (Vec<JoinHandle<()>>, Arc<CourierState>) {
        let pipeline_names: Vec<String> = self.pipelines.iter().map(|p| p.id.clone()).collect();
        let state = Arc::new(CourierState::new(pipeline_names));
        let handles = self.spawn_with_state_and_cancel(cancel, state.clone());
        (handles, state)
    }

    /// Internal: spawn with a pre-built `CourierState` so `run` can
    /// share the state with the health server.
    fn spawn_with_state_and_cancel(
        self,
        cancel: CancellationToken,
        state: Arc<CourierState>,
    ) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        for (i, p) in self.pipelines.into_iter().enumerate() {
            handles.extend(spawn_pipeline(p, cancel.clone(), state.clone(), i));
        }
        handles
    }

    /// Run the courier runtime: spawn all pipelines, start the health
    /// server (if configured), and block until shutdown.
    ///
    /// On SIGINT/Ctrl+C, transitions pipelines to `Draining`, waits up
    /// to `shutdown_timeout` for in-flight work to drain, then shuts down.
    /// Returns `RunOutcome::Failed` if any pipeline triggered
    /// `FailPipeline`; `RunOutcome::Success` otherwise.
    pub async fn run(self) -> RunOutcome {
        let pipeline_names: Vec<String> = self.pipelines.iter().map(|p| p.id.clone()).collect();
        let cancel = CancellationToken::new();
        let metrics = self.metrics.clone();
        let shutdown_timeout = self.shutdown_timeout;
        let health_address = self.health_address;

        let state = Arc::new(CourierState::new(pipeline_names));

        let health_task = health_address.map(|addr| {
            let state_clone = state.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::health::serve(addr, state_clone).await {
                    tracing::error!(address = %addr, "health server stopped unexpectedly: {e:#}");
                }
            })
        });

        let handles = self.spawn_with_state_and_cancel(cancel.clone(), state.clone());

        let signal_state = state.clone();
        let signal_metrics = metrics.clone();
        let signal_cancel = cancel.clone();
        tokio::spawn(async move {
            match tokio::signal::ctrl_c().await {
                Ok(()) => {
                    log::info!("received shutdown signal, draining pipelines");
                    signal_state.mark_shutdown_requested();
                    signal_state.transition_pipelines_to_draining();
                    signal_cancel.cancel();
                    signal_metrics.force_flush();
                    crate::observability::force_flush_traces();
                    crate::observability::force_flush_logs();
                }
                Err(e) => log::error!("failed to listen for shutdown signal: {e}"),
            }
        });

        let all_done = future::join_all(handles);
        tokio::pin!(all_done);

        tokio::select! {
            _ = &mut all_done => {
                tracing::info!("all pipelines completed");
            }
            _ = cancel.cancelled() => {
                match tokio::time::timeout(shutdown_timeout, &mut all_done).await {
                    Ok(_) => tracing::info!("all pipelines shut down cleanly"),
                    Err(_) => tracing::warn!(
                        timeout_secs = shutdown_timeout.as_secs(),
                        "shutdown timeout exceeded, orphaning remaining tasks"
                    ),
                }
            }
        }

        state.finalize_pipeline_states();

        metrics.shutdown();
        crate::observability::shutdown_traces();
        crate::observability::shutdown_logs();

        if let Some(task) = health_task {
            task.abort();
        }

        if state.has_failures() {
            RunOutcome::Failed
        } else {
            RunOutcome::Success
        }
    }
}
