use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

use crate::envelope::Envelope;
use crate::pipeline::ErrorPolicy;

pub mod kafka;

/// Full-control sink: owns the receiver loop.
///
/// Implement directly when the sink needs buffering, background work, or
/// a custom shutdown drain. Most sinks should implement `WriteOne` instead
/// and wrap themselves in `BasicSink`.
#[async_trait]
pub trait Sink: Send + Sync {
    fn id(&self) -> &str;

    async fn run(self: Box<Self>, rx: Receiver<Envelope>, cancel: CancellationToken);
}

/// Ergonomic sink: "write one envelope, report result".
///
/// `BasicSink` turns a `WriteOne` into a `Sink` with the standard recv
/// loop, cancellation, and error policy. Cross-cutting wrappers
/// (retry, batching, rate limit) will compose over `WriteOne` so they
/// are written once and reused across every concrete sink.
#[async_trait]
pub trait WriteOne: Send + Sync {
    fn id(&self) -> &str;

    async fn write(&self, env: &Envelope) -> Result<()>;
}

/// Adapter that turns any `WriteOne` into a `Sink`.
pub struct BasicSink<W: WriteOne> {
    pub inner: W,
    pub on_error: ErrorPolicy,
}

impl<W: WriteOne> BasicSink<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            on_error: ErrorPolicy::Drop,
        }
    }

    pub fn with_error_policy(mut self, policy: ErrorPolicy) -> Self {
        self.on_error = policy;
        self
    }
}

#[async_trait]
impl<W: WriteOne + 'static> Sink for BasicSink<W> {
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
                    if let Err(e) = self.inner.write(&env).await {
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
