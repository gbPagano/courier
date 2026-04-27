//! Observability primitives for the Courier runtime.
//!
//! - [`init_default_logging`] / [`init_from_config`] install the global
//!   `tracing-subscriber` (text or JSON) plus the `log` → `tracing`
//!   bridge, so the codebase's existing `log::` call sites flow through
//!   the same pipeline.
//! - [`metrics::ObsHandle`] / [`metrics::NodeCtx`] own the OpenTelemetry
//!   metrics SDK wiring and pre-bind counters/histograms per node.
//!
//! W3C trace-context propagation and OTLP traces ship in PR 4 (see
//! `OBSERVABILITY_PLAN.md`).

use std::sync::Once;

use tracing_log::LogTracer;
use tracing_subscriber::{EnvFilter, fmt};

use crate::config::{LogFormat, ObservabilityConfig};

pub mod metrics;

pub use metrics::{NodeCtx, NodeKind, ObsHandle, init_metrics};

static INIT: Once = Once::new();

/// Initialize the global logging subscriber with built-in defaults.
///
/// `default_directive` is the filter used when `RUST_LOG` is unset (e.g.
/// `"info"` for `run`, `"off"` for one-shot CLI commands). `RUST_LOG`, when
/// set, takes precedence — same semantics as the previous `env_logger`
/// integration.
///
/// Idempotent: a second call on the same process is a no-op so tests and
/// embedders that re-enter the entry point do not panic.
pub fn init_default_logging(default_directive: &str) {
    init_from_config(None, default_directive);
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
pub fn init_from_config(config: Option<&ObservabilityConfig>, default_directive: &str) {
    let format = config.map(|c| c.log_format).unwrap_or_default();
    let configured_level = config.and_then(|c| c.log_level.clone());
    INIT.call_once(|| install(format, configured_level.as_deref(), default_directive));
}

fn install(format: LogFormat, configured_level: Option<&str>, default_directive: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::try_new(configured_level.unwrap_or(default_directive))
            .expect("configured/default log filter should be valid")
    });

    let _ = LogTracer::init();
    // Match the prior `env_logger` behavior: write to stderr so stdout stays
    // clean for command output (`validate`, `list-components`) and for users
    // piping `courier run` output downstream.
    let builder = fmt().with_env_filter(filter).with_writer(std::io::stderr);
    let _ = match format {
        LogFormat::Text => builder.try_init(),
        LogFormat::Json => builder.json().try_init(),
    };
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

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
