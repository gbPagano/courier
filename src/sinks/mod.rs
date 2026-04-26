use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

use crate::envelope::Envelope;
use crate::pipeline::ErrorPolicy;
use crate::retry::RetryPolicy;

pub mod api;
pub mod file;
pub mod kafka;
mod retry;

/// Full-control sink: owns the receiver loop.
///
/// Implement directly when the sink needs buffering, background work, or
/// a custom shutdown drain. Most sinks should implement `WriteOne` instead
/// and wrap themselves in `ManagedSink`.
#[async_trait]
pub trait Sink: Send + Sync {
    fn id(&self) -> &str;

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
}

impl<W: WriteOne> ManagedSink<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            on_error: ErrorPolicy::Drop,
            retry: None,
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

    async fn run(self: Box<Self>, mut rx: Receiver<Envelope>, cancel: CancellationToken) {
        let id = self.inner.id().to_string();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    log::debug!("[{id}] cancelled, draining remaining items");
                    break;
                }
                maybe = rx.recv() => {
                    let Some(env) = maybe else { break };
                    let result = match &self.retry {
                        Some(policy) => retry::write_with_retry(&self.inner, &env, policy).await,
                        None => self.inner.write(&env).await,
                    };
                    if let Err(e) = result {
                        match &self.on_error {
                            ErrorPolicy::Drop => {
                                log::error!("[{id}] write failed, dropping: {e}");
                            }
                            ErrorPolicy::FailPipeline => {
                                log::error!("[{id}] write failed, failing pipeline: {e}");
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
