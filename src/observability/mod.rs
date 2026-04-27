//! Observability primitives for the Courier runtime.
//!
//! For now this only sets up the logging facade: a `tracing-subscriber`
//! layered with `EnvFilter`, plus `tracing-log::LogTracer` so the existing
//! `log::` call sites flow through the same subscriber. Metrics, tracing,
//! and OTLP export are added in subsequent PRs (see OBSERVABILITY_PLAN.md).

use std::sync::Once;

use tracing_log::LogTracer;
use tracing_subscriber::{EnvFilter, fmt};

static INIT: Once = Once::new();

/// Initialize the global logging subscriber.
///
/// `default_directive` is the filter used when `RUST_LOG` is unset (e.g.
/// `"info"` for `run`, `"off"` for one-shot CLI commands). `RUST_LOG`, when
/// set, takes precedence — same semantics as the previous `env_logger`
/// integration.
///
/// Idempotent: a second call on the same process is a no-op so tests and
/// embedders that re-enter the entry point do not panic.
pub fn init_default_logging(default_directive: &str) {
    INIT.call_once(|| install(default_directive));
}

fn install(default_directive: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive));

    let _ = LogTracer::init();
    // Match the prior `env_logger` behavior: write to stderr so stdout stays
    // clean for command output (`validate`, `list-components`) and for users
    // piping `courier run` output downstream.
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
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
