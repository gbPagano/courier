use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

use crate::envelope::Envelope;
use crate::pipeline::ErrorPolicy;

pub mod script;
pub mod set_key;

/// Full-control transform: owns both channels.
///
/// Implement directly for cardinality changes or stateful work:
/// batching (`N -> 1`), flat-map (`1 -> N`), joins, windowed aggregation.
/// For plain `1 -> 0-or-1` transforms, implement `MapOne` instead.
#[async_trait]
pub trait Transform: Send + Sync {
    fn id(&self) -> &str;

    async fn run(
        self: Box<Self>,
        rx: Receiver<Envelope>,
        tx: Sender<Envelope>,
        cancel: CancellationToken,
    );
}

/// Ergonomic transform: "receive one, return at most one".
///
/// Returning `Ok(None)` filters the envelope out. `BasicTransform` wraps
/// a `MapOne` into a `Transform` with the standard loop and error policy.
#[async_trait]
pub trait MapOne: Send + Sync {
    fn id(&self) -> &str;

    async fn map(&self, env: Envelope) -> Result<Option<Envelope>>;
}

/// Adapter that turns any `MapOne` into a `Transform`.
pub struct BasicTransform<M: MapOne> {
    pub inner: M,
    pub on_error: ErrorPolicy,
}

impl<M: MapOne> BasicTransform<M> {
    pub fn new(inner: M) -> Self {
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
impl<M: MapOne + 'static> Transform for BasicTransform<M> {
    fn id(&self) -> &str {
        self.inner.id()
    }

    async fn run(
        self: Box<Self>,
        mut rx: Receiver<Envelope>,
        tx: Sender<Envelope>,
        cancel: CancellationToken,
    ) {
        let id = self.inner.id().to_string();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe = rx.recv() => {
                    let Some(env) = maybe else { break };
                    match self.inner.map(env).await {
                        Ok(Some(out)) => {
                            if tx.send(out).await.is_err() {
                                log::debug!("[{id}] downstream closed");
                                return;
                            }
                        }
                        Ok(None) => { /* filtered */ }
                        Err(e) => match &self.on_error {
                            ErrorPolicy::Drop => {
                                log::error!("[{id}] map failed, dropping: {e}");
                            }
                            ErrorPolicy::FailPipeline => {
                                log::error!("[{id}] map failed, failing pipeline: {e}");
                                cancel.cancel();
                                break;
                            }
                        },
                    }
                }
            }
        }
    }
}
