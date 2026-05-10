use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{self, WeakSender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::redact_secret;
use crate::envelope::Envelope;
use crate::lifecycle::{CourierState, PipelineState, PipelineStatus};
use crate::observability::{NodeCtx, NodeKind, ObsHandle};
use crate::sinks::Sink;
use crate::sources::Source;
use crate::transforms::Transform;

/// How often the per-edge channel-depth sampler reports the gauge.
/// 300ms is dense enough to catch transient backpressure spikes
/// without burning cycles when the pipeline is idle.
const CHANNEL_DEPTH_SAMPLE_INTERVAL: Duration = Duration::from_millis(300);

/// What to do when a `Transform` or `Sink` returns `Err`.
///
/// - `Drop`: log and continue with the next envelope.
/// - `FailPipeline`: cancel every task in this pipeline via the shared
///   `CancellationToken`. Use when a failure means continuing is pointless
///   (schema drift, unauthorized, etc).
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub enum ErrorPolicy {
    #[default]
    Drop,
    FailPipeline,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub enum FanOutPolicy {
    /// Every sink receives every envelope; backpressure from the slowest sink stalls the pipeline.
    #[default]
    Broadcast,
    /// Every sink receives every envelope; a full channel drops the envelope for that sink only.
    BroadcastWithDrop,
}

/// One source feeding optional transforms into one or more sinks.
///
/// When constructed with more than one sink, `spawn_pipeline` inserts a
/// fan-out splitter that routes each envelope according to `fan_out`.
pub struct Pipeline {
    pub id: String,
    pub source: Box<dyn Source>,
    pub transforms: Vec<Box<dyn Transform>>,
    pub sinks: Vec<Box<dyn Sink>>,
    pub channel_capacity: usize,
    pub fan_out: FanOutPolicy,
    /// Optional shared metrics handle. `Registry::build_courier`
    /// attaches this from the courier-wide `ObsHandle`; tests that
    /// build pipelines manually leave it `None`, which means every
    /// node falls back to `NodeCtx::noop()` (i.e. instrumentation is
    /// silent).
    pub(crate) obs: Option<ObsHandle>,
}

impl Pipeline {
    pub fn new(id: impl Into<String>, source: Box<dyn Source>) -> Self {
        Self {
            id: id.into(),
            source,
            transforms: Vec::new(),
            sinks: Vec::new(),
            channel_capacity: 64,
            fan_out: FanOutPolicy::Broadcast,
            obs: None,
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

    pub fn with_fan_out(mut self, policy: FanOutPolicy) -> Self {
        self.fan_out = policy;
        self
    }

    pub fn with_observability(mut self, obs: Option<ObsHandle>) -> Self {
        self.obs = obs;
        self
    }
}

/// Wires source → transforms → sinks with mpsc channels and spawns each
/// node as its own tokio task. When `sinks.len() > 1`, a fan-out
/// splitter is inserted according to `pipeline.fan_out`. The default
/// `Broadcast` splitter is synchronous per sink: a slow sink applies
/// backpressure to the whole pipeline.
///
/// When the pipeline carries an `ObsHandle`, this function also:
/// - mints a `NodeCtx` per source/transform/sink and attaches it via
///   `set_node_ctx`, so the wrapper runtimes can bump counters in
///   their hot loops without hashmap lookups,
/// - spawns one channel-depth sampler task per mpsc edge that
///   reports `courier_channel_capacity_used` every 300ms until the
///   shared cancel token fires.
pub(crate) fn spawn_pipeline(
    p: Pipeline,
    cancel: CancellationToken,
    state: Arc<CourierState>,
    pipeline_index: usize,
) -> Vec<JoinHandle<()>> {
    let Pipeline {
        id,
        source,
        transforms,
        sinks,
        channel_capacity: cap,
        fan_out,
        obs,
    } = p;

    let status = state.pipeline_status(pipeline_index);

    tracing::info!(pipeline = %redact_secret(&id), "spawning pipeline");
    let mut handles = Vec::new();

    let prev_rx = spawn_source(
        &id,
        source,
        &transforms,
        &sinks,
        cap,
        &obs,
        cancel.clone(),
        &mut handles,
        &status,
    );
    let prev_rx = spawn_transforms(
        &id,
        transforms,
        &sinks,
        prev_rx,
        cap,
        &obs,
        cancel.clone(),
        &mut handles,
        &status,
    );
    spawn_sinks(
        &id,
        sinks,
        prev_rx,
        cap,
        fan_out,
        &obs,
        cancel,
        &mut handles,
        &status,
    );

    status.set_state(PipelineState::Running);

    handles
}

#[allow(clippy::too_many_arguments)]
fn spawn_source(
    id: &str,
    mut source: Box<dyn Source>,
    transforms: &[Box<dyn Transform>],
    sinks: &[Box<dyn Sink>],
    cap: usize,
    obs: &Option<ObsHandle>,
    cancel: CancellationToken,
    handles: &mut Vec<JoinHandle<()>>,
    status: &Arc<PipelineStatus>,
) -> mpsc::Receiver<Envelope> {
    let (src_tx, prev_rx) = mpsc::channel::<Envelope>(cap);
    let node_id = format!("{id}/src");

    {
        let ctx = if let Some(handle) = obs {
            NodeCtx::for_node(id, &node_id, NodeKind::Source, handle.clone())
        } else {
            NodeCtx::noop()
        };
        source.set_node_ctx(ctx.with_pipeline_status(status.clone()));
    }

    if let Some(handle) = obs.as_ref().filter(|h| h.is_enabled()) {
        spawn_edge_sampler(
            id,
            &node_id,
            &next_transform_or_sink_id(id, transforms, sinks),
            cap,
            src_tx.downgrade(),
            handle.clone(),
            cancel.clone(),
            handles,
        );
    }

    let c = cancel;
    handles.push(tokio::spawn(async move { source.run(src_tx, c).await }));
    prev_rx
}

#[allow(clippy::too_many_arguments)]
fn spawn_transforms(
    id: &str,
    transforms: Vec<Box<dyn Transform>>,
    sinks: &[Box<dyn Sink>],
    mut prev_rx: mpsc::Receiver<Envelope>,
    cap: usize,
    obs: &Option<ObsHandle>,
    cancel: CancellationToken,
    handles: &mut Vec<JoinHandle<()>>,
    status: &Arc<PipelineStatus>,
) -> mpsc::Receiver<Envelope> {
    let total = transforms.len();
    for (i, mut t) in transforms.into_iter().enumerate() {
        let node_id = format!("{id}/t{i}");
        {
            let ctx = if let Some(handle) = obs {
                NodeCtx::for_node(id, &node_id, NodeKind::Transform, handle.clone())
            } else {
                NodeCtx::noop()
            };
            t.set_node_ctx(ctx.with_pipeline_status(status.clone()));
        }

        let (next_tx, next_rx) = mpsc::channel::<Envelope>(cap);

        if let Some(handle) = obs.as_ref().filter(|h| h.is_enabled()) {
            spawn_edge_sampler(
                id,
                &node_id,
                &transform_or_sink_id_after(id, i + 1, total, sinks),
                cap,
                next_tx.downgrade(),
                handle.clone(),
                cancel.clone(),
                handles,
            );
        }

        let rx = prev_rx;
        let c = cancel.clone();
        handles.push(tokio::spawn(async move { t.run(rx, next_tx, c).await }));
        prev_rx = next_rx;
    }
    prev_rx
}

#[allow(clippy::too_many_arguments)]
fn spawn_sinks(
    id: &str,
    sinks: Vec<Box<dyn Sink>>,
    prev_rx: mpsc::Receiver<Envelope>,
    cap: usize,
    fan_out: FanOutPolicy,
    obs: &Option<ObsHandle>,
    cancel: CancellationToken,
    handles: &mut Vec<JoinHandle<()>>,
    status: &Arc<PipelineStatus>,
) {
    match sinks.len() {
        0 => {
            tracing::warn!(
                pipeline = %redact_secret(id),
                "pipeline has no sinks; envelopes will be discarded"
            );
            handles.push(tokio::spawn(async move {
                let mut rx = prev_rx;
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        m = rx.recv() => if m.is_none() { break },
                    }
                }
            }));
        }
        1 => spawn_single_sink(
            id,
            sinks.into_iter().next().unwrap(),
            prev_rx,
            obs,
            cancel,
            handles,
            status,
        ),
        _ => spawn_fanout(
            id, sinks, prev_rx, cap, fan_out, obs, cancel, handles, status,
        ),
    }
}

fn spawn_single_sink(
    id: &str,
    mut sink: Box<dyn Sink>,
    prev_rx: mpsc::Receiver<Envelope>,
    obs: &Option<ObsHandle>,
    cancel: CancellationToken,
    handles: &mut Vec<JoinHandle<()>>,
    status: &Arc<PipelineStatus>,
) {
    let node_id = format!("{id}/sink0");
    {
        let ctx = if let Some(handle) = obs {
            NodeCtx::for_node(id, &node_id, NodeKind::Sink, handle.clone())
        } else {
            NodeCtx::noop()
        };
        sink.set_node_ctx(ctx.with_pipeline_status(status.clone()));
    }
    let c = cancel;
    handles.push(tokio::spawn(async move { sink.run(prev_rx, c).await }));
}

#[allow(clippy::too_many_arguments)]
fn spawn_fanout(
    id: &str,
    mut sinks: Vec<Box<dyn Sink>>,
    prev_rx: mpsc::Receiver<Envelope>,
    cap: usize,
    fan_out: FanOutPolicy,
    obs: &Option<ObsHandle>,
    cancel: CancellationToken,
    handles: &mut Vec<JoinHandle<()>>,
    status: &Arc<PipelineStatus>,
) {
    let splitter_id = format!("{id}/fanout");
    let mut sink_txs = Vec::with_capacity(sinks.len());
    for (i, mut sink) in sinks.drain(..).enumerate() {
        let sink_node_id = format!("{id}/sink{i}");
        {
            let ctx = if let Some(handle) = obs {
                NodeCtx::for_node(id, &sink_node_id, NodeKind::Sink, handle.clone())
            } else {
                NodeCtx::noop()
            };
            sink.set_node_ctx(ctx.with_pipeline_status(status.clone()));
        }
        let (tx, rx) = mpsc::channel::<Envelope>(cap);
        if let Some(handle) = obs.as_ref().filter(|h| h.is_enabled()) {
            spawn_edge_sampler(
                id,
                &splitter_id,
                &sink_node_id,
                cap,
                tx.downgrade(),
                handle.clone(),
                cancel.clone(),
                handles,
            );
        }
        sink_txs.push(tx);
        let c = cancel.clone();
        handles.push(tokio::spawn(async move { sink.run(rx, c).await }));
    }
    match fan_out {
        FanOutPolicy::Broadcast => {
            spawn_broadcast_splitter(splitter_id, prev_rx, sink_txs, cancel, handles)
        }
        FanOutPolicy::BroadcastWithDrop => {
            spawn_broadcast_with_drop_splitter(
                id,
                splitter_id,
                prev_rx,
                sink_txs,
                obs,
                cancel,
                handles,
            );
        }
    }
}

fn spawn_broadcast_splitter(
    splitter_id: String,
    mut prev_rx: mpsc::Receiver<Envelope>,
    mut sink_txs: Vec<mpsc::Sender<Envelope>>,
    cancel: CancellationToken,
    handles: &mut Vec<JoinHandle<()>>,
) {
    // No NodeCtx: Broadcast never drops envelopes, so there is
    // nothing to record on the splitter node itself.
    handles.push(tokio::spawn(async move {
        'splitter: loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe = prev_rx.recv() => {
                    let Some(env) = maybe else { break };
                    let mut i = 0;
                    while i < sink_txs.len() {
                        tokio::select! {
                            _ = cancel.cancelled() => break 'splitter,
                            res = sink_txs[i].send(env.clone()) => {
                                if res.is_err() {
                                    tracing::debug!(
                                        node_id = %redact_secret(&splitter_id),
                                        sink_index = i,
                                        "downstream sink closed, removing from fanout"
                                    );
                                    sink_txs.swap_remove(i);
                                    if sink_txs.is_empty() {
                                        break 'splitter;
                                    }
                                    // don't increment — the swapped sink needs a turn
                                } else {
                                    i += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }));
}

fn spawn_broadcast_with_drop_splitter(
    pipeline_id: &str,
    splitter_id: String,
    mut prev_rx: mpsc::Receiver<Envelope>,
    mut sink_txs: Vec<mpsc::Sender<Envelope>>,
    obs: &Option<ObsHandle>,
    cancel: CancellationToken,
    handles: &mut Vec<JoinHandle<()>>,
) {
    let dropped_recorder = obs.as_ref().map(|handle| {
        NodeCtx::for_node(
            pipeline_id,
            &splitter_id,
            NodeKind::Splitter,
            handle.clone(),
        )
        .dropped_recorder("backpressure")
    });
    handles.push(tokio::spawn(async move {
        'splitter: loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe = prev_rx.recv() => {
                    let Some(env) = maybe else { break };
                    let mut i = 0;
                    while i < sink_txs.len() {
                        if cancel.is_cancelled() {
                            break 'splitter;
                        }
                        match sink_txs[i].try_send(env.clone()) {
                            Ok(()) => i += 1,
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                if let Some(ref r) = dropped_recorder {
                                    r.record();
                                }
                                tracing::debug!(
                                    node_id = %redact_secret(&splitter_id),
                                    sink_index = i,
                                    "downstream sink channel full, envelope dropped"
                                );
                                i += 1;
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                tracing::debug!(
                                    node_id = %redact_secret(&splitter_id),
                                    sink_index = i,
                                    "downstream sink closed, removing from fanout"
                                );
                                sink_txs.swap_remove(i);
                                if sink_txs.is_empty() {
                                    break 'splitter;
                                }
                                // don't increment — the swapped sink needs a turn
                            }
                        }
                    }
                }
            }
        }
    }));
}

/// First downstream node id from the source's perspective: either the
/// first transform (if any) or `sink0`/`fanout` depending on the
/// sink count. Used purely to label the source-out edge gauge.
fn next_transform_or_sink_id(
    id: &str,
    transforms: &[Box<dyn Transform>],
    sinks: &[Box<dyn Sink>],
) -> String {
    if !transforms.is_empty() {
        format!("{id}/t0")
    } else if sinks.len() > 1 {
        format!("{id}/fanout")
    } else {
        format!("{id}/sink0")
    }
}

/// Edge destination for the `i`-th transform's downstream edge:
/// either the next transform or the first sink/splitter.
fn transform_or_sink_id_after(
    id: &str,
    next_index: usize,
    total_transforms: usize,
    sinks: &[Box<dyn Sink>],
) -> String {
    if next_index < total_transforms {
        format!("{id}/t{next_index}")
    } else if sinks.len() > 1 {
        format!("{id}/fanout")
    } else {
        format!("{id}/sink0")
    }
}

/// Spawn a periodic task that samples how many slots are currently
/// in use on `tx`'s buffer and records the result on
/// `courier_channel_capacity_used`. Holds a clone of `tx` purely as a
/// capacity probe — never sends — so it doesn't perturb the pipeline.
#[allow(clippy::too_many_arguments)]
fn spawn_edge_sampler(
    pipeline: &str,
    src_node_id: &str,
    dest_node_id: &str,
    capacity: usize,
    tx: WeakSender<Envelope>,
    handle: ObsHandle,
    cancel: CancellationToken,
    handles: &mut Vec<JoinHandle<()>>,
) {
    let edge_id = format!(
        "{pipeline}/edge/{}->{}",
        short_node_id(pipeline, src_node_id),
        short_node_id(pipeline, dest_node_id)
    );
    let ctx = NodeCtx::for_node(pipeline, &edge_id, NodeKind::Edge, handle);
    handles.push(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(CHANNEL_DEPTH_SAMPLE_INTERVAL);
        // Skip the immediate first tick — first sample after one full
        // interval avoids reporting "0 used" before the source even
        // emits.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let Some(tx) = tx.upgrade() else {
                        break;
                    };
                    let used = capacity.saturating_sub(tx.capacity()) as u64;
                    ctx.record_channel_capacity_used(used);
                    // Quit once the channel is closed downstream — at
                    // that point the gauge is meaningless.
                    if tx.is_closed() {
                        break;
                    }
                }
            }
        }
    }));
}

fn short_node_id<'a>(pipeline: &str, node_id: &'a str) -> &'a str {
    node_id
        .strip_prefix(pipeline)
        .and_then(|s| s.strip_prefix('/'))
        .unwrap_or(node_id)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use async_trait::async_trait;
    use futures::future::join_all;
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use serde_json::json;
    use std::sync::{Arc, Mutex, OnceLock};
    use tokio::sync::{
        Notify,
        mpsc::{self, Receiver, Sender},
    };
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;
    use crate::lifecycle::CourierState;
    use crate::observability::metrics::testing::{
        counter_sum, histogram_count, obs_handle_in_memory,
    };
    use crate::observability::trace_context::TRACEPARENT;
    use crate::observability::{SendStopped, SourceCtx};
    use crate::sinks::{ManagedSink, WriteOne};
    use crate::transforms::{BasicTransform, MapOne};

    fn test_spawn_pipeline(pipeline: Pipeline, cancel: CancellationToken) -> Vec<JoinHandle<()>> {
        let state = Arc::new(CourierState::new(vec![pipeline.id.clone()]));
        let idx = 0;
        spawn_pipeline(pipeline, cancel, state, idx)
    }

    static TEST_TRACING_GLOBAL: OnceLock<()> = OnceLock::new();

    fn install_test_tracing_global() {
        TEST_TRACING_GLOBAL.get_or_init(|| {
            let subscriber =
                tracing_subscriber::registry().with(tracing_subscriber::filter::LevelFilter::TRACE);
            let _ = tracing::subscriber::set_global_default(subscriber);
        });
        tracing::callsite::rebuild_interest_cache();
    }

    struct HundredSource {
        source_ctx: SourceCtx,
    }

    impl HundredSource {
        fn new() -> Self {
            Self {
                source_ctx: SourceCtx::new("src"),
            }
        }
    }

    #[async_trait]
    impl Source for HundredSource {
        fn id(&self) -> &str {
            "src"
        }

        fn set_node_ctx(&mut self, ctx: NodeCtx) {
            self.source_ctx = SourceCtx::from_node_ctx(ctx);
        }

        async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
            for i in 0..100 {
                let env = Envelope::new("src", json!({ "n": i }));
                match self.source_ctx.send(&tx, env, &cancel).await {
                    Ok(()) => {}
                    Err(SendStopped::Cancelled) | Err(SendStopped::DownstreamClosed) => break,
                }
            }
        }
    }

    struct EvenOnly;

    #[async_trait]
    impl MapOne for EvenOnly {
        fn id(&self) -> &str {
            "even_only"
        }

        async fn map(&self, env: Envelope) -> Result<Option<Envelope>> {
            let n = env.payload["n"].as_i64().unwrap();
            Ok((n % 2 == 0).then_some(env))
        }
    }

    struct AcceptSink;

    #[async_trait]
    impl WriteOne for AcceptSink {
        fn id(&self) -> &str {
            "accept"
        }

        async fn write(&self, _env: &Envelope) -> Result<()> {
            Ok(())
        }
    }

    struct BurstSource {
        count: usize,
    }

    #[async_trait]
    impl Source for BurstSource {
        fn id(&self) -> &str {
            "burst"
        }

        async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
            for i in 0..self.count {
                let env = Envelope::new("burst", json!({ "n": i }));
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    res = tx.send(env) => {
                        if res.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }

    struct StallAfterFirstReceiveSink {
        first_received: Arc<Notify>,
    }

    #[async_trait]
    impl Sink for StallAfterFirstReceiveSink {
        fn id(&self) -> &str {
            "stall"
        }

        async fn run(self: Box<Self>, mut rx: Receiver<Envelope>, _cancel: CancellationToken) {
            if rx.recv().await.is_some() {
                self.first_received.notify_one();
                futures::future::pending::<()>().await;
            }
        }
    }

    /// Sink that receives one envelope and then stalls forever. Used by
    /// fan-out tests that need a permanently-blocked downstream channel.
    struct StallForeverAfterFirstSink;

    #[async_trait]
    impl Sink for StallForeverAfterFirstSink {
        fn id(&self) -> &str {
            "stall-forever"
        }

        async fn run(self: Box<Self>, mut rx: Receiver<Envelope>, _cancel: CancellationToken) {
            if rx.recv().await.is_some() {
                futures::future::pending::<()>().await;
            }
        }
    }

    struct CloseAfterOneSink;

    #[async_trait]
    impl Sink for CloseAfterOneSink {
        fn id(&self) -> &str {
            "close-after-one"
        }

        async fn run(self: Box<Self>, mut rx: Receiver<Envelope>, _cancel: CancellationToken) {
            rx.recv().await;
            // dropping rx closes the channel from this side
        }
    }

    struct DrainSink;

    #[async_trait]
    impl Sink for DrainSink {
        fn id(&self) -> &str {
            "drain"
        }

        async fn run(self: Box<Self>, mut rx: Receiver<Envelope>, cancel: CancellationToken) {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    maybe = rx.recv() => {
                        if maybe.is_none() {
                            break;
                        }
                    }
                }
            }
        }
    }

    struct TraceSource {
        source_ctx: SourceCtx,
    }

    impl TraceSource {
        fn new() -> Self {
            Self {
                source_ctx: SourceCtx::new("trace/src"),
            }
        }
    }

    #[async_trait]
    impl Source for TraceSource {
        fn id(&self) -> &str {
            "src"
        }

        fn set_node_ctx(&mut self, ctx: NodeCtx) {
            self.source_ctx = SourceCtx::from_node_ctx(ctx);
        }

        async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
            let source = self.source_ctx.clone();
            let mut env = Envelope::new("src", json!({ "n": 1 }));
            env.meta.headers.insert(
                TRACEPARENT.to_string(),
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string(),
            );
            match source.send(&tx, env, &cancel).await {
                Ok(()) | Err(SendStopped::Cancelled) | Err(SendStopped::DownstreamClosed) => {}
            }
        }
    }

    struct PassThrough;

    #[async_trait]
    impl MapOne for PassThrough {
        fn id(&self) -> &str {
            "pass"
        }

        async fn map(&self, env: Envelope) -> Result<Option<Envelope>> {
            Ok(Some(env))
        }
    }

    struct CaptureSink {
        seen: Arc<Mutex<Vec<Envelope>>>,
    }

    #[async_trait]
    impl WriteOne for CaptureSink {
        fn id(&self) -> &str {
            "capture"
        }

        async fn write(&self, env: &Envelope) -> Result<()> {
            self.seen.lock().unwrap().push(env.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn node_ctx_records_pipeline_metrics() {
        let (handle, exporter) = obs_handle_in_memory();
        let pipeline = Pipeline::new("metrics", Box::new(HundredSource::new()))
            .with_observability(Some(handle.clone()))
            .with_transform(Box::new(BasicTransform::new(EvenOnly)))
            .with_sink(Box::new(ManagedSink::new(AcceptSink)));

        let handles = test_spawn_pipeline(pipeline, CancellationToken::new());
        join_all(handles).await;
        handle.shutdown();

        assert_eq!(
            counter_sum(
                &exporter,
                "courier_envelopes_processed_total",
                &[("pipeline", "metrics"), ("node_id", "metrics/src")]
            ),
            100
        );
        assert_eq!(
            counter_sum(
                &exporter,
                "courier_envelopes_processed_total",
                &[("pipeline", "metrics"), ("node_id", "metrics/t0")]
            ),
            50
        );
        assert_eq!(
            counter_sum(
                &exporter,
                "courier_envelopes_filtered_total",
                &[("pipeline", "metrics"), ("node_id", "metrics/t0")]
            ),
            50
        );
        assert_eq!(
            counter_sum(
                &exporter,
                "courier_envelopes_processed_total",
                &[("pipeline", "metrics"), ("node_id", "metrics/sink0")]
            ),
            50
        );
        assert_eq!(
            histogram_count(
                &exporter,
                "courier_stage_duration_milliseconds",
                &[("pipeline", "metrics"), ("node_id", "metrics/src")]
            ),
            100
        );
        assert_eq!(
            histogram_count(
                &exporter,
                "courier_stage_duration_milliseconds",
                &[("pipeline", "metrics"), ("node_id", "metrics/t0")]
            ),
            100
        );
        assert_eq!(
            histogram_count(
                &exporter,
                "courier_stage_duration_milliseconds",
                &[("pipeline", "metrics"), ("node_id", "metrics/sink0")]
            ),
            50
        );
    }

    #[tokio::test]
    async fn broadcast_splitter_observes_cancel_while_blocked_on_sink_send() {
        let first_received = Arc::new(Notify::new());
        let pipeline = Pipeline::new("broadcast-cancel", Box::new(BurstSource { count: 32 }))
            .with_channel_capacity(1)
            .with_sink(Box::new(StallAfterFirstReceiveSink {
                first_received: first_received.clone(),
            }))
            .with_sink(Box::new(DrainSink));

        let cancel = CancellationToken::new();
        let mut handles = test_spawn_pipeline(pipeline, cancel.clone());
        let splitter = handles
            .pop()
            .expect("broadcast splitter should be the final spawned task");

        first_received.notified().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_millis(250), splitter).await;
        for handle in handles {
            handle.abort();
        }

        assert!(
            result.is_ok(),
            "broadcast splitter did not exit promptly after cancellation"
        );
    }

    #[test]
    fn trace_context_propagates_across_pipeline() {
        install_test_tracing_global();

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("courier_test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
        let dispatch = tracing::Dispatch::new(subscriber);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (metrics, _metric_exporter) = obs_handle_in_memory();
        let seen = Arc::new(Mutex::new(Vec::new()));

        tracing::dispatcher::with_default(&dispatch, || {
            tracing::callsite::rebuild_interest_cache();
            runtime.block_on(async {
                let cancel = CancellationToken::new();
                let (source_tx, transform_rx) = mpsc::channel(8);
                let mut source = TraceSource::new();
                source.set_node_ctx(NodeCtx::for_node(
                    "trace",
                    "trace/src",
                    NodeKind::Source,
                    metrics.clone(),
                ));
                Box::new(source).run(source_tx, cancel.clone()).await;

                let (sink_tx, sink_rx) = mpsc::channel(8);
                let mut transform = BasicTransform::new(PassThrough);
                transform.set_node_ctx(NodeCtx::for_node(
                    "trace",
                    "trace/t0",
                    NodeKind::Transform,
                    metrics.clone(),
                ));
                Box::new(transform)
                    .run(transform_rx, sink_tx, cancel.clone())
                    .await;

                let mut sink = ManagedSink::new(CaptureSink { seen: seen.clone() });
                sink.set_node_ctx(NodeCtx::for_node(
                    "trace",
                    "trace/sink0",
                    NodeKind::Sink,
                    metrics,
                ));
                Box::new(sink).run(sink_rx, cancel).await;
            });
            tracing::callsite::rebuild_interest_cache();
        });
        provider.force_flush().unwrap();

        let captured = seen.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        assert!(
            captured[0].meta.headers.contains_key(TRACEPARENT),
            "sink should see refreshed trace context"
        );

        let spans = exporter.get_finished_spans().unwrap();
        let source_span = spans
            .iter()
            .find(|s| s.name == "courier.source")
            .unwrap_or_else(|| panic!("missing source span: {spans:?}"));
        assert!(
            source_span.attributes.iter().any(|attr| {
                attr.key.as_str() == "pipeline"
                    && matches!(&attr.value, opentelemetry::Value::String(value) if value.as_ref() == "trace")
            }),
            "source span missing pipeline attribute: {source_span:?}"
        );
        assert!(
            spans.iter().any(|s| s.name == "courier.transform"),
            "missing transform span: {spans:?}"
        );
        assert!(
            spans.iter().any(|s| s.name == "courier.sink"),
            "missing sink span: {spans:?}"
        );
        let incoming_trace_id = "4bf92f3577b34da6a3ce929d0e0e4736";
        assert!(
            spans
                .iter()
                .all(|s| s.span_context.trace_id().to_string() == incoming_trace_id),
            "spans did not share incoming trace id: {spans:?}"
        );
    }

    #[tokio::test]
    async fn broadcast_with_drop_records_dropped_metrics() {
        let (handle, exporter) = obs_handle_in_memory();
        let pipeline = Pipeline::new(
            "broadcast-drop-metrics",
            Box::new(BurstSource { count: 10 }),
        )
        .with_channel_capacity(1)
        .with_fan_out(FanOutPolicy::BroadcastWithDrop)
        .with_observability(Some(handle.clone()))
        .with_sink(Box::new(StallForeverAfterFirstSink))
        .with_sink(Box::new(DrainSink));

        let cancel = CancellationToken::new();
        let handles = test_spawn_pipeline(pipeline, cancel.clone());
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_millis(500), join_all(handles)).await;

        handle.force_flush();
        let dropped = counter_sum(
            &exporter,
            "courier_envelopes_dropped_total",
            &[
                ("pipeline", "broadcast-drop-metrics"),
                ("node_id", "broadcast-drop-metrics/fanout"),
                ("reason", "backpressure"),
            ],
        );
        assert!(
            dropped > 0,
            "expected some dropped envelopes due to backpressure"
        );
    }

    #[tokio::test]
    async fn broadcast_with_drop_all_sinks_close_terminates_cleanly() {
        // When every sink closes before the source finishes, the splitter
        // should drain prev_rx and exit without hanging.
        let pipeline = Pipeline::new("all-closed", Box::new(BurstSource { count: 20 }))
            .with_channel_capacity(4)
            .with_fan_out(FanOutPolicy::BroadcastWithDrop)
            .with_sink(Box::new(CloseAfterOneSink))
            .with_sink(Box::new(CloseAfterOneSink));

        let cancel = CancellationToken::new();
        let handles = test_spawn_pipeline(pipeline, cancel);
        let result = tokio::time::timeout(Duration::from_secs(1), join_all(handles)).await;
        assert!(
            result.is_ok(),
            "pipeline should terminate when all sinks close"
        );
    }
}
