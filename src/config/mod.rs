mod interpolate;
mod loader;
mod observability;
mod parse;
mod raw;
mod redact;
mod types;
mod validate;

pub use observability::{LogFormat, LogsConfig, MetricsConfig, ObservabilityConfig, TracingConfig};
pub use parse::parse_config;
pub use redact::{redact_secret, redact_secret_path};
pub use types::{
    Config, ErrorPolicyConfig, FanOutPolicyConfig, HealthConfig, PipelineSpec, ShutdownConfig,
    SinkSpec, SourceSpec, TransformSpec,
};

#[cfg(test)]
pub(crate) use redact::REDACTED_SECRET;
#[cfg(test)]
pub(crate) use redact::clear_secret_values_for_test;
#[cfg(test)]
pub(crate) use redact::register_secret_value;

// Shared lock for all config tests that mutate env vars or the global
// secret registry. A single lock prevents concurrent test threads from
// clearing each other's registered secrets.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
