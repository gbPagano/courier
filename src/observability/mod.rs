//! Observability primitives for the Courier runtime.
//!
//! - [`init_default_logging`] / [`init_from_config`] install the global
//!   `tracing-subscriber` (text or JSON) plus the `log` → `tracing`
//!   bridge, so the codebase's existing `log::` call sites flow through
//!   the same pipeline.
//! - [`metrics::ObsHandle`] / [`metrics::NodeCtx`] own the OpenTelemetry
//!   metrics SDK wiring and pre-bind counters/histograms per node.
//! - [`trace_context`] propagates W3C `traceparent` / `tracestate` through
//!   envelope metadata.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Once, OnceLock};

use anyhow::{Context, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt, registry};

use crate::config::{LogFormat, ObservabilityConfig, redact_secret};

pub mod metrics;
pub mod source_ctx;
pub mod trace_context;

pub use metrics::{NodeCtx, NodeKind, ObsHandle, init_metrics};
pub use source_ctx::{SendStopped, SourceCtx};

static INIT: Once = Once::new();
static INIT_OK: AtomicBool = AtomicBool::new(false);
static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();
static LOGGER_PROVIDER: OnceLock<SdkLoggerProvider> = OnceLock::new();

/// Initialize the global logging subscriber with built-in defaults.
///
/// `default_directive` is the filter used when `RUST_LOG` is unset (e.g.
/// `"info"` for `run`, `"off"` for one-shot CLI commands). `RUST_LOG`, when
/// set, takes precedence — same semantics as the previous `env_logger`
/// integration.
///
/// Idempotent: a second call on the same process is a no-op so tests and
/// embedders that re-enter the entry point do not panic.
pub fn init_default_logging(default_directive: &str) -> Result<()> {
    init_from_config(None, default_directive)
}

/// Initialize the subscriber from an optional `[observability]` config.
///
/// Precedence for the filter directive (highest first):
/// 1. `RUST_LOG` env var — keeps existing operator muscle memory and is
///    consistent with the prior `env_logger` behavior.
/// 2. `observability.log_level` from config, if set.
/// 3. The caller-supplied `default_directive` (built-in per-subcommand).
///
/// Idempotent like [`init_default_logging`].
pub fn init_from_config(
    config: Option<&ObservabilityConfig>,
    default_directive: &str,
) -> Result<()> {
    if INIT.is_completed() {
        if INIT_OK.load(Ordering::Acquire) {
            return Ok(());
        } else {
            return Err(anyhow::anyhow!(
                "tracing subscriber initialization failed on a previous attempt"
            ));
        }
    }

    let format = config.map(|c| c.log_format).unwrap_or_default();
    let configured_level = config.and_then(|c| c.log_level.clone());

    // Build OTLP providers inside `call_once` so two concurrent first
    // calls cannot each construct an `SdkTracerProvider` /
    // `SdkLoggerProvider` (the loser's would otherwise be dropped
    // silently and trigger a no-op flush). The winning thread captures
    // any exporter-build error in `provider_err` and propagates it
    // here; the losing thread observes `INIT_OK` instead.
    let provider_err: Cell<Option<anyhow::Error>> = Cell::new(None);
    INIT.call_once(|| {
        let tracer_provider = match init_traces(config) {
            Ok(p) => p,
            Err(e) => {
                provider_err.set(Some(e));
                INIT_OK.store(false, Ordering::Release);
                return;
            }
        };
        let logger_provider = match init_logs(config) {
            Ok(p) => p,
            Err(e) => {
                provider_err.set(Some(e));
                INIT_OK.store(false, Ordering::Release);
                return;
            }
        };
        let ok = install(
            format,
            configured_level.as_deref(),
            default_directive,
            tracer_provider,
            logger_provider,
        );
        INIT_OK.store(ok, Ordering::Release);
    });

    if let Some(err) = provider_err.take() {
        return Err(err);
    }
    if INIT_OK.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err(anyhow::anyhow!("failed to install tracing subscriber"))
    }
}

fn install(
    format: LogFormat,
    configured_level: Option<&str>,
    default_directive: &str,
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
) -> bool {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::try_new(configured_level.unwrap_or(default_directive))
            .expect("configured/default log filter should be valid")
    });

    // Match the prior `env_logger` behavior: write to stderr so stdout stays
    // clean for command output (`validate`, `list-components`) and for users
    // piping `courier run` output downstream.
    let fmt_layer = match format {
        LogFormat::Text => fmt::layer().with_writer(std::io::stderr).boxed(),
        LogFormat::Json => fmt::layer().json().with_writer(std::io::stderr).boxed(),
    };

    let telemetry_layer = tracer_provider.map(|provider| {
        let tracer = provider.tracer("courier");
        let layer = tracing_opentelemetry::layer().with_tracer(tracer).boxed();
        let _ = TRACER_PROVIDER.set(provider);
        layer
    });

    // OTLP log appender. The bridge converts every `tracing` event into an
    // OTel `LogRecord` and routes it through the `SdkLoggerProvider` (which
    // we own via `LOGGER_PROVIDER` so shutdown can flush). Local stderr
    // output keeps working through `fmt_layer` — this is additive.
    let otlp_log_layer = logger_provider.map(|provider| {
        let layer = OpenTelemetryTracingBridge::new(&provider).boxed();
        let _ = LOGGER_PROVIDER.set(provider);
        layer
    });

    registry()
        .with(filter)
        .with(telemetry_layer)
        .with(otlp_log_layer)
        .with(fmt_layer)
        .try_init()
        .is_ok()
}

fn configured_endpoint(endpoint: Option<&str>) -> Option<&str> {
    endpoint.and_then(|endpoint| {
        let endpoint = endpoint.trim();
        (!endpoint.is_empty()).then_some(endpoint)
    })
}

fn init_traces(config: Option<&ObservabilityConfig>) -> Result<Option<SdkTracerProvider>> {
    let Some(obs) = config else {
        return Ok(None);
    };
    let Some(endpoint) = configured_endpoint(obs.tracing.otlp_endpoint.as_deref()) else {
        return Ok(None);
    };

    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .with_context(|| {
            format!(
                "failed to build OTLP span exporter for {}",
                redact_secret(endpoint)
            )
        })?;

    let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
        obs.tracing.sample_ratio,
    )));
    let resource = Resource::builder()
        .with_service_name(obs.service_name.clone())
        .build();
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler)
        .with_resource(resource)
        .build();
    Ok(Some(provider))
}

fn init_logs(config: Option<&ObservabilityConfig>) -> Result<Option<SdkLoggerProvider>> {
    let Some(obs) = config else {
        return Ok(None);
    };
    let Some(endpoint) = configured_endpoint(obs.logs.otlp_endpoint.as_deref()) else {
        return Ok(None);
    };

    let exporter = LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .with_context(|| {
            format!(
                "failed to build OTLP log exporter for {}",
                redact_secret(endpoint)
            )
        })?;

    let resource = Resource::builder()
        .with_service_name(obs.service_name.clone())
        .build();

    let provider = SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();
    Ok(Some(provider))
}

pub(crate) fn force_flush_traces() {
    if let Some(provider) = TRACER_PROVIDER.get() {
        let _ = provider.force_flush();
    }
}

pub(crate) fn shutdown_traces() {
    if let Some(provider) = TRACER_PROVIDER.get() {
        let _ = provider.shutdown();
    }
}

pub(crate) fn force_flush_logs() {
    if let Some(provider) = LOGGER_PROVIDER.get() {
        let _ = provider.force_flush();
    }
}

pub(crate) fn shutdown_logs() {
    if let Some(provider) = LOGGER_PROVIDER.get() {
        let _ = provider.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{configured_endpoint, init_from_config};
    use tracing::Subscriber;
    use tracing::subscriber::with_default;
    use tracing_log::LogTracer;
    use tracing_subscriber::{EnvFilter, layer::SubscriberExt, registry::Registry};

    /// Records the `target` of every event the subscriber sees, so the test
    /// can assert that records went through the `log` → `tracing` bridge.
    #[derive(Clone, Default)]
    struct CapturingLayer {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl<S: Subscriber> tracing_subscriber::Layer<S> for CapturingLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.events
                .lock()
                .unwrap()
                .push(event.metadata().target().to_string());
        }
    }

    #[test]
    fn rust_log_directive_parses_through_env_filter() {
        // No env var fiddling: parse a directive directly. This exercises
        // the same `EnvFilter` parser the runtime uses; env-var pickup is a
        // standard `tracing-subscriber` behavior we don't need to re-test.
        let filter = EnvFilter::new("courier=debug,hyper=warn");
        let rendered = filter.to_string();
        assert!(rendered.contains("courier=debug"), "got: {rendered}");
        assert!(rendered.contains("hyper=warn"), "got: {rendered}");
    }

    #[test]
    fn empty_otlp_endpoints_are_disabled() {
        assert_eq!(configured_endpoint(None), None);
        assert_eq!(configured_endpoint(Some("")), None);
        assert_eq!(configured_endpoint(Some("   \t\n")), None);
        assert_eq!(
            configured_endpoint(Some("  http://collector:4317  ")),
            Some("http://collector:4317")
        );
    }

    #[test]
    fn init_from_config_second_call_is_noop() {
        let config = crate::config::ObservabilityConfig::default();

        let first = init_from_config(Some(&config), "off");
        let second = init_from_config(Some(&config), "off");

        // Idempotent: both calls must return the same result.
        // When another test has already installed a global subscriber,
        // both calls will fail; otherwise both succeed.
        assert_eq!(
            first.is_ok(),
            second.is_ok(),
            "init_from_config should be idempotent: first={first:?}, second={second:?}"
        );
    }

    #[test]
    fn log_macros_flow_through_tracing_subscriber() {
        // Install the LogTracer once for the test process. `init` errors if
        // already installed (e.g. by a prior test), which is fine.
        let _ = LogTracer::init();

        let layer = CapturingLayer::default();
        let events = layer.events.clone();
        let subscriber = Registry::default().with(layer);

        with_default(subscriber, || {
            log::warn!(target: "courier_pr1_bridge_check", "bridge ok");
        });

        // `tracing-log` sets the event's metadata target to "log" and exposes
        // the original log target as a field. Existence of any event proves
        // the bridge installed a `log` → `tracing` route; the `"log"` target
        // is the bridge's own marker.
        let captured = events.lock().unwrap().clone();
        assert!(
            captured.iter().any(|t| t == "log"),
            "expected log:: record to reach tracing subscriber, got: {captured:?}"
        );
    }
}
