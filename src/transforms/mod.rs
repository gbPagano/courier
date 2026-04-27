use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

use crate::envelope::Envelope;
use crate::observability::NodeCtx;
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

    /// Attach the per-node observability context. Called by
    /// `spawn_pipeline` after the transform is built but before it
    /// runs. Default no-op — full-control transforms that want
    /// metrics override this and store the ctx; `BasicTransform`
    /// already does so for the common path.
    fn set_node_ctx(&mut self, _ctx: NodeCtx) {}

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
    node_ctx: NodeCtx,
}

impl<M: MapOne> BasicTransform<M> {
    pub fn new(inner: M) -> Self {
        Self {
            inner,
            on_error: ErrorPolicy::Drop,
            node_ctx: NodeCtx::noop(),
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

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        self.node_ctx = ctx;
    }

    async fn run(
        self: Box<Self>,
        mut rx: Receiver<Envelope>,
        tx: Sender<Envelope>,
        cancel: CancellationToken,
    ) {
        let id = self.inner.id().to_string();
        let ctx = self.node_ctx;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe = rx.recv() => {
                    let Some(env) = maybe else { break };
                    let started = Instant::now();
                    let result = self.inner.map(env).await;
                    ctx.record_stage_duration_ms(started.elapsed().as_secs_f64() * 1000.0);
                    match result {
                        Ok(Some(out)) => {
                            ctx.record_processed();
                            if tx.send(out).await.is_err() {
                                tracing::debug!(node_id = %id, "downstream closed");
                                return;
                            }
                        }
                        Ok(None) => {
                            ctx.record_filtered();
                        }
                        Err(e) => {
                            ctx.record_failed();
                            match &self.on_error {
                                ErrorPolicy::Drop => {
                                    tracing::error!(node_id = %id, error = %e, "map failed, dropping");
                                }
                                ErrorPolicy::FailPipeline => {
                                    tracing::error!(node_id = %id, error = %e, "map failed, failing pipeline");
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
}
