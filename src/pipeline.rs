use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::envelope::Envelope;
use crate::sinks::Sink;
use crate::sources::Source;
use crate::transforms::Transform;

/// What to do when a `Transform` or `Sink` returns `Err`.
///
/// - `Drop`: log and continue with the next envelope.
/// - `FailPipeline`: cancel every task in this pipeline via the shared
///   `CancellationToken`. Use when a failure means continuing is pointless
///   (schema drift, unauthorized, etc).
#[derive(Debug, Clone, Default)]
pub enum ErrorPolicy {
    #[default]
    Drop,
    FailPipeline,
}

/// One source feeding optional transforms into one or more sinks.
///
/// When constructed with more than one sink, `spawn_pipeline` inserts a
/// broadcast splitter that clones each envelope to every sink.
pub struct Pipeline {
    pub id: String,
    pub source: Box<dyn Source>,
    pub transforms: Vec<Box<dyn Transform>>,
    pub sinks: Vec<Box<dyn Sink>>,
    pub channel_capacity: usize,
}

impl Pipeline {
    pub fn new(id: impl Into<String>, source: Box<dyn Source>) -> Self {
        Self {
            id: id.into(),
            source,
            transforms: Vec::new(),
            sinks: Vec::new(),
            channel_capacity: 64,
        }
    }

    pub fn with_transform(mut self, t: Box<dyn Transform>) -> Self {
        self.transforms.push(t);
        self
    }

    pub fn with_sink(mut self, s: Box<dyn Sink>) -> Self {
        self.sinks.push(s);
        self
    }

    pub fn with_channel_capacity(mut self, cap: usize) -> Self {
        self.channel_capacity = cap;
        self
    }
}

/// Wires source → transforms → sinks with mpsc channels and spawns each
/// node as its own tokio task. When `sinks.len() > 1`, an implicit
/// broadcast splitter is inserted. The splitter is synchronous per sink:
/// a slow sink applies backpressure to the whole pipeline.
pub(crate) fn spawn_pipeline(p: Pipeline, cancel: CancellationToken) -> Vec<JoinHandle<()>> {
    let Pipeline {
        id,
        source,
        transforms,
        sinks,
        channel_capacity: cap,
    } = p;

    log::info!("[{id}] spawning pipeline");
    let mut handles = Vec::new();

    let (src_tx, mut prev_rx) = mpsc::channel::<Envelope>(cap);
    let c = cancel.clone();
    handles.push(tokio::spawn(async move { source.run(src_tx, c).await }));

    for t in transforms {
        let (next_tx, next_rx) = mpsc::channel::<Envelope>(cap);
        let rx = prev_rx;
        let c = cancel.clone();
        handles.push(tokio::spawn(async move { t.run(rx, next_tx, c).await }));
        prev_rx = next_rx;
    }

    match sinks.len() {
        0 => {
            log::warn!("[{id}] pipeline has no sinks, envelopes will be discarded");
            let c = cancel.clone();
            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        m = prev_rx.recv() => if m.is_none() { break },
                    }
                }
            }));
        }
        1 => {
            let sink = sinks.into_iter().next().unwrap();
            let c = cancel.clone();
            handles.push(tokio::spawn(async move { sink.run(prev_rx, c).await }));
        }
        _ => {
            let mut sink_txs = Vec::with_capacity(sinks.len());
            for sink in sinks {
                let (tx, rx) = mpsc::channel::<Envelope>(cap);
                sink_txs.push(tx);
                let c = cancel.clone();
                handles.push(tokio::spawn(async move { sink.run(rx, c).await }));
            }
            let c = cancel.clone();
            let splitter_id = format!("{id}/broadcast");
            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        maybe = prev_rx.recv() => {
                            let Some(env) = maybe else { break };
                            for tx in &sink_txs {
                                if tx.send(env.clone()).await.is_err() {
                                    log::debug!("[{splitter_id}] downstream sink closed");
                                }
                            }
                        }
                    }
                }
            }));
        }
    }

    handles
}
