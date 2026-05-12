//! Metrics core for Courier.
//!
//! Single-stop wiring for the OpenTelemetry metrics SDK:
//! - [`init_metrics`] builds an [`ObsHandle`] from the parsed
//!   `[observability]` config (or returns a no-op handle when no
//!   exporter is configured).
//! - [`NodeCtx`] holds a pre-built attribute set and counter/histogram
//!   handles bound to a single node id. Hot loops in `BasicTransform`
//!   and `ManagedSink` record metrics without rebuilding attributes.
//! - [`ObsHandle::shutdown`] force-flushes any installed providers so
//!   the last batch survives a graceful drain.
//!
//! The OTLP push path (PeriodicReader + grpc-tonic exporter) is
//! activated when `[observability.metrics].otlp_endpoint` is set.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use opentelemetry::KeyValue;

use crate::lifecycle::PipelineStatus;
use opentelemetry::metrics::{Counter, Histogram, InstrumentProvider, Meter, MeterProvider};
use opentelemetry_otlp::{MetricExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

use crate::config::{ObservabilityConfig, redact_secret};

/// Coarse-grained classification of a runtime node, used as a metric
/// attribute so dashboards can slice "all sinks" or "all transforms"
/// without per-name aggregation.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NodeKind {
    Source,
    Transform,
    Sink,
    /// The implicit broadcast splitter inserted by `spawn_pipeline`
    /// when a pipeline has more than one sink.
    Splitter,
    /// One mpsc edge between two adjacent nodes. Reported by the
    /// channel-depth sampler; not used by transforms or sinks.
    Edge,
}

impl NodeKind {
    fn as_str(self) -> &'static str {
        match self {
            NodeKind::Source => "source",
            NodeKind::Transform => "transform",
            NodeKind::Sink => "sink",
            NodeKind::Splitter => "splitter",
            NodeKind::Edge => "edge",
        }
    }
}

/// Shared metrics provider plus pre-built instrument handles.
///
/// One `ObsHandle` is constructed per `Courier` and cloned into every
/// `NodeCtx`. Cloning is cheap — instruments are `Arc` internally.
#[derive(Clone)]
pub struct ObsHandle {
    inner: Arc<ObsHandleInner>,
}

struct ObsHandleInner {
    /// `Some` when the SDK provider is owned (real or in-memory test
    /// reader); `None` for the global noop fallback.
    provider: Option<SdkMeterProvider>,
    instruments: Instruments,
    log_keys: bool,
}

struct Instruments {
    processed: Counter<u64>,
    failed: Counter<u64>,
    filtered: Counter<u64>,
    retries: Counter<u64>,
    dead_lettered: Counter<u64>,
    dropped: Counter<u64>,
    script_timeouts: Counter<u64>,
    script_payload_too_large: Counter<u64>,
    stage_duration: Histogram<f64>,
    end_to_end_latency: Histogram<f64>,
    channel_capacity_used: Histogram<u64>,
}

#[derive(Debug)]
struct NoopInstrumentProvider;

impl InstrumentProvider for NoopInstrumentProvider {}

impl ObsHandle {
    /// Build an `ObsHandle` with no exporter installed. Counters and
    /// histograms still work (so callers don't branch on `Option`),
    /// but observations are dropped. This uses a private no-op meter,
    /// not `opentelemetry::global`, so embedded hosts with a global
    /// provider do not receive Courier metrics when Courier metrics
    /// are disabled.
    pub fn noop() -> Self {
        Self::noop_with_log_keys(false)
    }

    fn noop_with_log_keys(log_keys: bool) -> Self {
        let meter = Meter::new(Arc::new(NoopInstrumentProvider));
        Self::from_meter(meter, None, log_keys)
    }

    /// Whether this handle owns a concrete SDK provider. False means
    /// observations go to a private no-op meter and callers can skip
    /// auxiliary sampling tasks.
    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.provider.is_some()
    }

    fn from_meter(meter: Meter, provider: Option<SdkMeterProvider>, log_keys: bool) -> Self {
        let instruments = Instruments {
            processed: meter
                .u64_counter("courier_envelopes_processed_total")
                .with_description("Envelopes successfully processed by a node.")
                .build(),
            failed: meter
                .u64_counter("courier_envelopes_failed_total")
                .with_description(
                    "Envelopes that triggered an error in a node, after retries are exhausted.",
                )
                .build(),
            filtered: meter
                .u64_counter("courier_envelopes_filtered_total")
                .with_description("Envelopes intentionally dropped by a transform (MapOne returned None).")
                .build(),
            retries: meter
                .u64_counter("courier_retries_total")
                .with_description("Retry attempts performed by a sink.")
                .build(),
            dead_lettered: meter
                .u64_counter("courier_dead_lettered_total")
                .with_description("Envelopes routed to a dead-letter sink after retries were exhausted.")
                .build(),
            dropped: meter
                .u64_counter("courier_envelopes_dropped_total")
                .with_description("Envelopes dropped by the fan-out splitter because the downstream channel was full or closed.")
                .build(),
            script_timeouts: meter
                .u64_counter("courier_script_timeouts_total")
                .with_description("Script transform invocations aborted because they exceeded the configured per-envelope timeout.")
                .build(),
            script_payload_too_large: meter
                .u64_counter("courier_script_payload_too_large_total")
                .with_description("Envelopes rejected by a script transform because their serialized size exceeded a configured size guardrail.")
                .build(),
            stage_duration: meter
                .f64_histogram("courier_stage_duration_milliseconds")
                .with_description("Wall-clock time a node spent processing one envelope.")
                .with_unit("ms")
                .build(),
            end_to_end_latency: meter
                .f64_histogram("courier_end_to_end_latency_milliseconds")
                .with_description(
                    "Time from envelope creation (meta.timestamp_ms) to sink write completion.",
                )
                .with_unit("ms")
                .build(),
            channel_capacity_used: meter
                .u64_histogram("courier_channel_capacity_used")
                .with_description(
                    "In-flight items on a pipeline edge, sampled periodically (capacity - sender.capacity()).",
                )
                .build(),
        };
        Self {
            inner: Arc::new(ObsHandleInner {
                provider,
                instruments,
                log_keys,
            }),
        }
    }

    /// Force-flush pending observations without tearing down the provider.
    pub fn force_flush(&self) {
        if let Some(provider) = &self.inner.provider {
            let _ = provider.force_flush();
        }
    }

    /// Drain observations and tear down the installed provider.
    pub fn shutdown(&self) {
        if let Some(provider) = &self.inner.provider {
            let _ = provider.shutdown();
        }
    }
}

/// Per-node bundle of metric attributes and instrument handles.
///
/// Constructed once per node by `spawn_pipeline` (or by tests). The
/// hot-path code in `BasicTransform` / `ManagedSink` stores the
/// `NodeCtx` alongside its other state and calls `processed_add(1)`
/// without hashmap lookups.
#[derive(Clone)]
pub struct NodeCtx {
    handle: ObsHandle,
    attrs: Arc<[KeyValue]>,
    pipeline: Arc<str>,
    node_id: Arc<str>,
    node_kind: NodeKind,
    log_keys: bool,
    /// Attached by `spawn_pipeline` via `with_pipeline_status`; always `Some`
    /// at runtime. `None` only in unit tests that construct `NodeCtx::noop()`
    /// directly without going through `spawn_pipeline`, in which case
    /// `mark_pipeline_failed()` is a no-op.
    pipeline_status: Option<Arc<PipelineStatus>>,
}

impl NodeCtx {
    /// Build a `NodeCtx` for a single node. The attribute set is
    /// `pipeline`, `node_id`, `node_kind` — kept tight on purpose to
    /// avoid cardinality blow-ups (no `meta.key`, no payload labels).
    pub fn for_node(pipeline: &str, node_id: &str, node_kind: NodeKind, handle: ObsHandle) -> Self {
        let attrs: Arc<[KeyValue]> = Arc::from(
            [
                KeyValue::new("pipeline", pipeline.to_string()),
                KeyValue::new("node_id", node_id.to_string()),
                KeyValue::new("node_kind", node_kind.as_str()),
            ]
            .as_slice(),
        );
        let log_keys = handle.inner.log_keys;
        Self {
            handle,
            attrs,
            pipeline: Arc::from(pipeline),
            node_id: Arc::from(node_id),
            node_kind,
            log_keys,
            pipeline_status: None,
        }
    }

    /// Attach the pipeline status so `mark_pipeline_failed` can transition
    /// both the failure flag and the visible state to `Failed`.
    pub fn with_pipeline_status(mut self, status: Arc<PipelineStatus>) -> Self {
        self.pipeline_status = Some(status);
        self
    }

    /// Mark this pipeline as failed due to an unrecoverable error
    /// (`FailPipeline` policy). Sets state to `Failed` and raises the failure
    /// flag so the health probe and exit code both reflect the error.
    /// No-op when no status is attached (e.g. tests or no-op contexts).
    pub fn mark_pipeline_failed(&self) {
        if let Some(status) = &self.pipeline_status {
            status.mark_failed();
        }
    }

    /// No-op context with empty attributes, backed by a private
    /// no-op meter. Used by tests that build pipelines manually and
    /// by the default state of `BasicTransform` / `ManagedSink` until
    /// `spawn_pipeline` attaches a real ctx.
    pub fn noop() -> Self {
        Self {
            handle: ObsHandle::noop(),
            attrs: Arc::from([] as [KeyValue; 0]),
            pipeline: Arc::from(""),
            node_id: Arc::from(""),
            node_kind: NodeKind::Transform,
            log_keys: false,
            pipeline_status: None,
        }
    }

    pub fn attrs(&self) -> &[KeyValue] {
        &self.attrs
    }

    pub fn handle(&self) -> &ObsHandle {
        &self.handle
    }

    pub fn pipeline(&self) -> &str {
        &self.pipeline
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn node_kind(&self) -> NodeKind {
        self.node_kind
    }

    pub fn node_kind_str(&self) -> &'static str {
        self.node_kind.as_str()
    }

    pub fn log_keys(&self) -> bool {
        self.log_keys
    }

    pub fn record_processed(&self) {
        self.handle.inner.instruments.processed.add(1, &self.attrs);
    }

    pub fn record_filtered(&self) {
        self.handle.inner.instruments.filtered.add(1, &self.attrs);
    }

    pub fn record_failed(&self) {
        self.handle.inner.instruments.failed.add(1, &self.attrs);
    }

    pub fn record_retry(&self) {
        self.handle.inner.instruments.retries.add(1, &self.attrs);
    }

    pub fn record_dead_letter(&self) {
        self.handle
            .inner
            .instruments
            .dead_lettered
            .add(1, &self.attrs);
    }

    /// Precompute a recorder for `courier_envelopes_dropped_total` bound
    /// to a fixed `reason`. Call this once outside the hot loop and reuse
    /// the returned `DroppedRecorder` per drop to avoid rebuilding the
    /// attribute set on every event.
    pub fn dropped_recorder(&self, reason: &'static str) -> DroppedRecorder {
        let mut attrs: Vec<KeyValue> = self.attrs.iter().cloned().collect();
        attrs.push(KeyValue::new("reason", reason));
        DroppedRecorder {
            handle: self.handle.clone(),
            attrs: Arc::from(attrs.as_slice()),
        }
    }

    /// Precompute a recorder for `courier_script_timeouts_total` bound to
    /// a fixed script `runtime` ("rhai", "lua", "python"). Built once by
    /// the engine in `set_node_ctx` so the hot path doesn't rebuild the
    /// attribute slice.
    pub fn script_timeout_recorder(&self, runtime: &'static str) -> ScriptTimeoutRecorder {
        let mut attrs: Vec<KeyValue> = self.attrs.iter().cloned().collect();
        attrs.push(KeyValue::new("runtime", runtime));
        ScriptTimeoutRecorder {
            handle: self.handle.clone(),
            attrs: Arc::from(attrs.as_slice()),
        }
    }

    /// Precompute a recorder for `courier_script_payload_too_large_total`
    /// bound to a fixed script `runtime` and a fixed `direction` ("in" or
    /// "out"). Built once by `ScriptMapOne` in `set_node_ctx` per enabled
    /// side so the hot path doesn't rebuild the attribute slice.
    pub fn script_payload_too_large_recorder(
        &self,
        runtime: &'static str,
        direction: &'static str,
    ) -> ScriptPayloadTooLargeRecorder {
        let mut attrs: Vec<KeyValue> = self.attrs.iter().cloned().collect();
        attrs.push(KeyValue::new("runtime", runtime));
        attrs.push(KeyValue::new("direction", direction));
        ScriptPayloadTooLargeRecorder {
            handle: self.handle.clone(),
            attrs: Arc::from(attrs.as_slice()),
        }
    }

    pub fn record_stage_duration_ms(&self, ms: f64) {
        self.handle
            .inner
            .instruments
            .stage_duration
            .record(ms, &self.attrs);
    }

    pub fn record_end_to_end_latency_ms(&self, ms: f64) {
        self.handle
            .inner
            .instruments
            .end_to_end_latency
            .record(ms, &self.attrs);
    }

    pub fn record_channel_capacity_used(&self, used: u64) {
        self.handle
            .inner
            .instruments
            .channel_capacity_used
            .record(used, &self.attrs);
    }
}

/// Reusable handle for counting dropped envelopes at a fixed `reason`.
/// Built once via [`NodeCtx::dropped_recorder`] so the attribute slice
/// is allocated up front and not rebuilt per event.
#[derive(Clone)]
pub struct DroppedRecorder {
    handle: ObsHandle,
    attrs: Arc<[KeyValue]>,
}

impl DroppedRecorder {
    pub fn record(&self) {
        self.handle.inner.instruments.dropped.add(1, &self.attrs);
    }
}

/// Reusable handle for counting script transform timeouts at a fixed
/// `runtime`. Built once via [`NodeCtx::script_timeout_recorder`].
#[derive(Clone)]
pub struct ScriptTimeoutRecorder {
    handle: ObsHandle,
    attrs: Arc<[KeyValue]>,
}

impl ScriptTimeoutRecorder {
    pub fn record(&self) {
        self.handle
            .inner
            .instruments
            .script_timeouts
            .add(1, &self.attrs);
    }
}

/// Reusable handle for counting script transform size violations at a
/// fixed `runtime` and `direction`. Built once via
/// [`NodeCtx::script_payload_too_large_recorder`].
#[derive(Clone)]
pub struct ScriptPayloadTooLargeRecorder {
    handle: ObsHandle,
    attrs: Arc<[KeyValue]>,
}

impl ScriptPayloadTooLargeRecorder {
    pub fn record(&self) {
        self.handle
            .inner
            .instruments
            .script_payload_too_large
            .add(1, &self.attrs);
    }
}

/// Build an `ObsHandle` from the parsed `[observability]` config.
///
/// When `config.metrics.otlp_endpoint` is set, installs a
/// `PeriodicReader` over the OTLP/gRPC exporter pushing every
/// `export_interval_ms`. When unset, returns a no-op handle so the
/// rest of the runtime is unaware of the difference.
pub fn init_metrics(config: Option<&ObservabilityConfig>) -> Result<ObsHandle> {
    let Some(obs) = config else {
        return Ok(ObsHandle::noop());
    };
    let Some(endpoint) = super::configured_endpoint(obs.metrics.otlp_endpoint.as_deref()) else {
        return Ok(ObsHandle::noop_with_log_keys(obs.log_keys));
    };

    let exporter = MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .with_context(|| {
            format!(
                "failed to build OTLP metric exporter for {}",
                redact_secret(endpoint)
            )
        })?;

    let reader = PeriodicReader::builder(exporter)
        .with_interval(Duration::from_millis(obs.metrics.export_interval_ms))
        .build();

    let resource = Resource::builder()
        .with_service_name(obs.service_name.clone())
        .build();

    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build();

    let meter = provider.meter("courier");
    Ok(ObsHandle::from_meter(meter, Some(provider), obs.log_keys))
}

#[cfg(test)]
pub(crate) mod testing {
    //! Test helpers — an `InMemoryMetricExporter` paired with a
    //! `PeriodicReader` so unit tests can drive a small pipeline,
    //! force a flush, and assert exact counter / histogram values
    //! without touching a real OTLP collector.

    use std::time::Duration;

    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::ObsHandle;

    /// Build an `ObsHandle` backed by an in-memory exporter and return
    /// the exporter so tests can pull collected metrics out of it.
    pub fn obs_handle_in_memory() -> (ObsHandle, InMemoryMetricExporter) {
        let exporter = InMemoryMetricExporter::default();
        // 1-hour interval — tests must call `provider.force_flush()`
        // explicitly so the timing is deterministic. The reader still
        // exists so the SDK has somewhere to push.
        let reader = PeriodicReader::builder(exporter.clone())
            .with_interval(Duration::from_secs(3600))
            .build();
        let provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(Resource::builder().with_service_name("test").build())
            .build();
        let meter = provider.meter("courier_test");
        let handle = ObsHandle::from_meter(meter, Some(provider), false);
        (handle, exporter)
    }

    /// Sum a `u64` counter across every collected resource metrics
    /// snapshot, restricted to data points whose attribute set
    /// matches `expected_attrs` (subset match — extra attrs ignored).
    pub fn counter_sum(
        exporter: &InMemoryMetricExporter,
        metric_name: &str,
        expected_attrs: &[(&str, &str)],
    ) -> u64 {
        let mut total = 0u64;
        for rm in exporter.get_finished_metrics().unwrap_or_default() {
            for sm in rm.scope_metrics() {
                for metric in sm.metrics() {
                    if metric.name() != metric_name {
                        continue;
                    }
                    if let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() {
                        for dp in sum.data_points() {
                            if attrs_match(dp.attributes(), expected_attrs) {
                                total += dp.value();
                            }
                        }
                    }
                }
            }
        }
        total
    }

    /// Number of histogram observations for `metric_name` matching
    /// `expected_attrs`. Useful for "did this run record N samples?"
    pub fn histogram_count(
        exporter: &InMemoryMetricExporter,
        metric_name: &str,
        expected_attrs: &[(&str, &str)],
    ) -> u64 {
        let mut total = 0u64;
        for rm in exporter.get_finished_metrics().unwrap_or_default() {
            for sm in rm.scope_metrics() {
                for metric in sm.metrics() {
                    if metric.name() != metric_name {
                        continue;
                    }
                    match metric.data() {
                        AggregatedMetrics::F64(MetricData::Histogram(h)) => {
                            for dp in h.data_points() {
                                if attrs_match(dp.attributes(), expected_attrs) {
                                    total += dp.count();
                                }
                            }
                        }
                        AggregatedMetrics::U64(MetricData::Histogram(h)) => {
                            for dp in h.data_points() {
                                if attrs_match(dp.attributes(), expected_attrs) {
                                    total += dp.count();
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        total
    }

    fn attrs_match<'a>(
        actual: impl Iterator<Item = &'a opentelemetry::KeyValue>,
        expected: &[(&str, &str)],
    ) -> bool {
        let actual: Vec<_> = actual.collect();
        expected.iter().all(|(k, v)| {
            actual
                .iter()
                .any(|kv| kv.key.as_str() == *k && kv.value.as_str() == *v)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use opentelemetry::global;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::testing::counter_sum;
    use super::*;

    #[test]
    fn noop_handle_does_not_record_to_global_meter_provider() {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone())
            .with_interval(Duration::from_secs(3600))
            .build();
        let provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(Resource::builder().with_service_name("host").build())
            .build();

        global::set_meter_provider(provider.clone());

        let ctx = NodeCtx::noop();
        ctx.record_processed();
        ctx.record_failed();
        ctx.record_stage_duration_ms(1.0);

        let _ = provider.force_flush();

        assert_eq!(
            counter_sum(&exporter, "courier_envelopes_processed_total", &[]),
            0
        );
        assert_eq!(
            counter_sum(&exporter, "courier_envelopes_failed_total", &[]),
            0
        );
    }

    #[test]
    fn init_metrics_preserves_log_keys_when_exporter_is_disabled() {
        let obs = ObservabilityConfig {
            log_keys: true,
            ..ObservabilityConfig::default()
        };

        let handle = init_metrics(Some(&obs)).unwrap();
        assert!(!handle.is_enabled());

        let ctx = NodeCtx::for_node("p", "p/source", NodeKind::Source, handle);
        assert!(ctx.log_keys());
    }
}
