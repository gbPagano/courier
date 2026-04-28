use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map as JsonMap, Number as JsonNumber, Value};
use toml::{Table, Value as TomlValue};
use tracing_subscriber::EnvFilter;

use crate::pipeline::ErrorPolicy;
use crate::retry::RetryPolicy;

/// Shared helper for factory authors: deserialize a component spec into
/// a typed config and wrap any failure with a uniform
/// "invalid config for component type '{kind}'" context. Using this
/// keeps error messages consistent between built-in and third-party
/// factories.
pub fn parse_config<T: DeserializeOwned>(kind: &str, config: Value) -> Result<T> {
    serde_json::from_value(config)
        .with_context(|| format!("invalid config for component type '{kind}'"))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub pipelines: Vec<PipelineSpec>,
    /// Optional `[observability]` block. Parsed and validated here; the
    /// runtime reads `log_format` / `log_level` to drive the `tracing`
    /// subscriber and (in later PRs) the metrics/tracing OTLP exporters.
    pub observability: Option<ObservabilityConfig>,
}

/// Top-level observability settings. Every field defaults so an empty
/// `[observability]` block matches the runtime's built-in behavior.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservabilityConfig {
    /// `service.name` resource attribute on emitted telemetry.
    pub service_name: String,
    pub log_format: LogFormat,
    /// Filter directive applied when `RUST_LOG` is unset. `RUST_LOG`
    /// keeps precedence over this, matching the prior `env_logger`
    /// semantics — operators can still override on the command line
    /// without editing the config file.
    pub log_level: Option<String>,
    /// Allow `meta.key` to appear as a span field. Off by default so
    /// keys never leak to log aggregators unless the operator opts in.
    pub log_keys: bool,
    pub metrics: MetricsConfig,
    pub tracing: TracingConfig,
    pub logs: LogsConfig,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetricsConfig {
    /// OTLP endpoint to push metrics to. `None` disables metric export.
    pub otlp_endpoint: Option<String>,
    /// Push interval for the OTLP `PeriodicReader`. Matched to the
    /// common Prometheus scrape interval so an OTel Collector relaying
    /// to Prometheus does not see staler data than necessary.
    pub export_interval_ms: u64,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: None,
            export_interval_ms: 15_000,
        }
    }
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            service_name: "courier".to_string(),
            log_format: LogFormat::Text,
            log_level: None,
            log_keys: false,
            metrics: MetricsConfig::default(),
            tracing: TracingConfig::default(),
            logs: LogsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TracingConfig {
    /// OTLP endpoint to push spans to. `None` disables tracing export.
    pub otlp_endpoint: Option<String>,
    /// Parent-based sampling ratio applied at root spans. `0.0` drops
    /// every root, `1.0` keeps everything; default `0.1` is sane for
    /// most production workloads.
    pub sample_ratio: f64,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: None,
            sample_ratio: 0.1,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogsConfig {
    /// OTLP endpoint to push logs to. `None` disables OTLP log export
    /// (events still flow to stdout/stderr through the `tracing-subscriber`
    /// fmt layer).
    pub otlp_endpoint: Option<String>,
}

impl Config {
    /// Parse a Courier config from a TOML string. Unknown keys on each
    /// component are preserved as the component's `config: Value` bucket
    /// and handed to the factory at runtime. TOML datetimes are
    /// stringified on the way through (no native JSON equivalent).
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let toml_value: TomlValue = toml::from_str(s).context("failed to parse TOML config")?;
        let json_value = toml_value_to_json(toml_value);
        let raw: RawConfig =
            serde_json::from_value(json_value).context("failed to parse TOML config")?;
        Ok(raw.into())
    }

    /// Parse a Courier config from a JSON string. Equivalent on-disk
    /// format to TOML; the resulting `Config` is identical.
    pub fn from_json_str(s: &str) -> Result<Self> {
        let raw: RawConfig = serde_json::from_str(s).context("failed to parse JSON config")?;
        Ok(raw.into())
    }

    /// Read and parse a Courier config from disk. `path` may be either:
    /// * A single file — parsed by extension (`.toml` or `.json`).
    /// * A directory — every `.toml`/`.json` file in it is parsed in
    ///   sorted order and their `pipelines` are concatenated. Other
    ///   entries (subdirectories, hidden files, unrelated extensions)
    ///   are ignored. Duplicate pipeline names across files are
    ///   rejected. At most one file may define `[observability]`, since
    ///   observability settings are global to the process.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.is_dir() {
            Self::load_dir(path)
        } else {
            Self::load_file(path)
        }
    }

    /// Validate Courier-owned configuration before any component is built
    /// or runtime task is spawned. Component-owned fields remain in each
    /// factory's validation path so custom/plugin fields stay extensible.
    pub fn validate(&self) -> Result<()> {
        if let Some(obs) = &self.observability {
            validate_observability(obs)?;
        }

        let mut seen_names: HashMap<&str, usize> = HashMap::new();

        for (pipeline_index, pipeline) in self.pipelines.iter().enumerate() {
            let pipeline_path = format!("pipelines[{pipeline_index}]");
            let pipeline_label = pipeline_label(pipeline_index, &pipeline.name);

            if pipeline.name.trim().is_empty() {
                bail!("{pipeline_path}.name: pipeline name must not be empty");
            }

            if let Some(previous_index) = seen_names.insert(pipeline.name.as_str(), pipeline_index)
            {
                bail!(
                    "{pipeline_path}.name: duplicate pipeline name '{}' (already defined at pipelines[{previous_index}].name)",
                    pipeline.name
                );
            }

            if pipeline.source.kind.trim().is_empty() {
                bail!("{pipeline_label}.source.type: source type must not be empty");
            }

            if let Some(retry) = &pipeline.source.retry {
                validate_retry_policy(retry, &format!("{pipeline_label}.source.retry"))?;
            }

            if matches!(pipeline.channel_capacity, Some(0)) {
                bail!("{pipeline_label}.channel_capacity: must be greater than 0");
            }

            if pipeline.sinks.is_empty() {
                bail!("{pipeline_label}.sinks: at least one sink is required");
            }

            for (transform_index, transform) in pipeline.transforms.iter().enumerate() {
                if transform.kind.trim().is_empty() {
                    bail!(
                        "{pipeline_label}.transforms[{transform_index}].type: transform type must not be empty"
                    );
                }
            }

            for (sink_index, sink) in pipeline.sinks.iter().enumerate() {
                if sink.kind.trim().is_empty() {
                    bail!("{pipeline_label}.sinks[{sink_index}].type: sink type must not be empty");
                }
                if let Some(retry) = &sink.retry {
                    validate_retry_policy(
                        retry,
                        &format!("{pipeline_label}.sinks[{sink_index}].retry"),
                    )?;
                }
            }
        }

        Ok(())
    }

    fn load_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        parse_by_extension(path, &content)
            .with_context(|| format!("failed to load config file {}", path.display()))
    }

    fn load_dir(dir: &Path) -> Result<Self> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("failed to read config directory {}", dir.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|p| p.is_file() && is_supported_config_extension(p))
            .collect();
        files.sort();

        let mut merged = Config {
            pipelines: Vec::new(),
            observability: None,
        };
        let mut seen_names: std::collections::HashMap<String, PathBuf> =
            std::collections::HashMap::new();
        let mut observability_file: Option<PathBuf> = None;

        for file in files {
            let Config {
                pipelines,
                observability,
            } = Self::load_file(&file)?;

            if let Some(observability) = observability {
                if let Some(prev) = observability_file.replace(file.clone()) {
                    anyhow::bail!(
                        "duplicate observability config in {} (also defined in {})",
                        file.display(),
                        prev.display(),
                    );
                }
                merged.observability = Some(observability);
            }

            for pipeline in pipelines {
                if let Some(prev) = seen_names.insert(pipeline.name.clone(), file.clone()) {
                    anyhow::bail!(
                        "duplicate pipeline name '{}' in {} (also defined in {})",
                        pipeline.name,
                        file.display(),
                        prev.display(),
                    );
                }
                merged.pipelines.push(pipeline);
            }
        }

        Ok(merged)
    }
}

fn pipeline_label(index: usize, name: &str) -> String {
    if name.trim().is_empty() {
        format!("pipelines[{index}]")
    } else {
        format!("pipeline '{name}'")
    }
}

fn validate_observability(obs: &ObservabilityConfig) -> Result<()> {
    if !(0.0..=1.0).contains(&obs.tracing.sample_ratio) || !obs.tracing.sample_ratio.is_finite() {
        bail!(
            "observability.tracing.sample_ratio: must be a finite value in [0.0, 1.0] (got {})",
            obs.tracing.sample_ratio
        );
    }
    if obs.service_name.trim().is_empty() {
        bail!("observability.service_name: must not be empty");
    }
    if obs.metrics.export_interval_ms == 0 {
        bail!("observability.metrics.export_interval_ms: must be greater than 0");
    }
    if let Some(level) = &obs.log_level {
        if level.trim().is_empty() {
            bail!("observability.log_level: must not be empty when set");
        }
        EnvFilter::try_new(level)
            .with_context(|| "observability.log_level: invalid log filter directive")?;
    }
    Ok(())
}

fn validate_retry_policy(policy: &RetryPolicy, path: &str) -> Result<()> {
    if policy.max_attempts == 0 {
        bail!("{path}.max_attempts: must be greater than or equal to 1");
    }

    if policy.max_attempts > 1 {
        if policy.initial_delay_ms == 0 {
            bail!("{path}.initial_delay_ms: must be greater than 0 when max_attempts > 1");
        }
        if policy.max_delay_ms == 0 {
            bail!("{path}.max_delay_ms: must be greater than 0 when max_attempts > 1");
        }
    }

    if !policy.backoff_multiplier.is_finite() || policy.backoff_multiplier < 1.0 {
        bail!("{path}.backoff_multiplier: must be finite and greater than or equal to 1.0");
    }

    if policy.max_delay_ms < policy.initial_delay_ms {
        bail!("{path}.max_delay_ms: must be greater than or equal to initial_delay_ms");
    }

    if let crate::retry::ExhaustedPolicy::DeadLetter { path: dlq_path } = &policy.on_exhausted {
        if dlq_path.as_os_str().is_empty() {
            bail!("{path}.on_exhausted.path: dead-letter path must not be empty");
        }

        if let Some(parent) = dlq_path.parent()
            && !parent.as_os_str().is_empty()
        {
            if !parent.exists() {
                bail!(
                    "{path}.on_exhausted.path: parent directory '{}' does not exist",
                    parent.display()
                );
            }
            if !parent.is_dir() {
                bail!(
                    "{path}.on_exhausted.path: parent '{}' is not a directory",
                    parent.display()
                );
            }
        }
    }

    Ok(())
}

fn is_supported_config_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("toml" | "json")
    )
}

fn parse_by_extension(path: &Path, content: &str) -> Result<Config> {
    match path.extension().and_then(|s| s.to_str()) {
        Some("json") => Config::from_json_str(content),
        Some("toml") | None => Config::from_toml_str(content),
        Some(other) => Err(anyhow::anyhow!(
            "unsupported config file extension '.{other}' (expected '.toml' or '.json')"
        )),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PipelineSpec {
    pub name: String,
    pub source: SourceSpec,
    pub transforms: Vec<TransformSpec>,
    pub sinks: Vec<SinkSpec>,
    pub channel_capacity: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceSpec {
    pub kind: String,
    pub config: Value,
    /// Retry/backoff policy. Polling sources thread this into their
    /// `PollScheduler`; push-based sources reject it at factory time.
    pub retry: Option<RetryPolicy>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransformSpec {
    pub kind: String,
    pub config: Value,
    pub on_error: Option<ErrorPolicyConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SinkSpec {
    pub kind: String,
    pub config: Value,
    pub on_error: Option<ErrorPolicyConfig>,
    /// Retry policy applied to every write by `ManagedSink`. When `None`
    /// the sink makes a single attempt and defers to `on_error` on failure.
    pub retry: Option<RetryPolicy>,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum ErrorPolicyConfig {
    #[default]
    Drop,
    FailPipeline,
}

impl From<ErrorPolicyConfig> for ErrorPolicy {
    fn from(value: ErrorPolicyConfig) -> Self {
        match value {
            ErrorPolicyConfig::Drop => ErrorPolicy::Drop,
            ErrorPolicyConfig::FailPipeline => ErrorPolicy::FailPipeline,
        }
    }
}

// -----------------------------------------------------------------------
// Config loader
//
// Both TOML and JSON go through the same `Raw*` layer, which flattens
// arbitrary per-component fields into a `serde_json::Map` bucket. For
// TOML we parse into `toml::Value` first (to preserve the TOML-only
// datetime type, which gets stringified) and convert to a JSON value
// before deserializing into the Raw layer.
// -----------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    defaults: RawDefaults,
    #[serde(default)]
    observability: Option<RawObservability>,
    pipelines: Vec<RawPipelineConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawObservability {
    #[serde(default)]
    service_name: Option<String>,
    #[serde(default)]
    log_format: Option<RawLogFormat>,
    #[serde(default)]
    log_level: Option<String>,
    #[serde(default)]
    log_keys: Option<bool>,
    #[serde(default)]
    metrics: Option<RawMetricsConfig>,
    #[serde(default)]
    tracing: Option<RawTracingConfig>,
    #[serde(default)]
    logs: Option<RawLogsConfig>,
}

#[derive(Debug, Default, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum RawLogFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMetricsConfig {
    #[serde(default)]
    otlp_endpoint: Option<String>,
    #[serde(default)]
    export_interval_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTracingConfig {
    #[serde(default)]
    otlp_endpoint: Option<String>,
    #[serde(default)]
    sample_ratio: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLogsConfig {
    #[serde(default)]
    otlp_endpoint: Option<String>,
}

/// Per-file defaults applied to components that omit the matching field.
/// Scope is intentionally per-file: in directory mode each file is parsed
/// independently before pipelines are concatenated, so defaults never
/// leak across files (and load order can't change behavior).
#[derive(Debug, Default, Deserialize)]
struct RawDefaults {
    #[serde(default)]
    source: RawSourceDefaults,
    #[serde(default)]
    sink: RawSinkDefaults,
    #[serde(default)]
    transform: RawTransformDefaults,
}

#[derive(Debug, Default, Deserialize)]
struct RawSourceDefaults {
    #[serde(default)]
    retry: Option<RetryPolicy>,
}

#[derive(Debug, Default, Deserialize)]
struct RawSinkDefaults {
    #[serde(default)]
    on_error: Option<RawErrorPolicyConfig>,
    #[serde(default)]
    retry: Option<RetryPolicy>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTransformDefaults {
    #[serde(default)]
    on_error: Option<RawErrorPolicyConfig>,
}

#[derive(Debug, Deserialize)]
struct RawPipelineConfig {
    name: String,
    source: RawSourceConfig,
    #[serde(default)]
    transforms: Vec<RawTransformConfig>,
    #[serde(default)]
    sinks: Vec<RawSinkConfig>,
    #[serde(default)]
    channel_capacity: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RawSourceConfig {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    retry: Option<RetryPolicy>,
    #[serde(flatten)]
    config: JsonMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct RawTransformConfig {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    on_error: Option<RawErrorPolicyConfig>,
    #[serde(flatten)]
    config: JsonMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct RawSinkConfig {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    on_error: Option<RawErrorPolicyConfig>,
    #[serde(default)]
    retry: Option<RetryPolicy>,
    #[serde(flatten)]
    config: JsonMap<String, Value>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum RawErrorPolicyConfig {
    Drop,
    FailPipeline,
}

impl From<RawConfig> for Config {
    fn from(value: RawConfig) -> Self {
        let defaults = value.defaults;
        Self {
            pipelines: value
                .pipelines
                .into_iter()
                .map(|p| pipeline_from_raw(p, &defaults))
                .collect(),
            observability: value.observability.map(observability_from_raw),
        }
    }
}

fn observability_from_raw(value: RawObservability) -> ObservabilityConfig {
    let default = ObservabilityConfig::default();
    let service_name = value.service_name.unwrap_or(default.service_name);

    ObservabilityConfig {
        service_name,
        log_format: value
            .log_format
            .map(|f| match f {
                RawLogFormat::Text => LogFormat::Text,
                RawLogFormat::Json => LogFormat::Json,
            })
            .unwrap_or(default.log_format),
        log_level: value.log_level,
        log_keys: value.log_keys.unwrap_or(default.log_keys),
        metrics: value
            .metrics
            .map(|m| {
                let d = MetricsConfig::default();
                MetricsConfig {
                    otlp_endpoint: m.otlp_endpoint,
                    export_interval_ms: m.export_interval_ms.unwrap_or(d.export_interval_ms),
                }
            })
            .unwrap_or(default.metrics),
        tracing: value
            .tracing
            .map(|t| {
                let d = TracingConfig::default();
                TracingConfig {
                    otlp_endpoint: t.otlp_endpoint,
                    sample_ratio: t.sample_ratio.unwrap_or(d.sample_ratio),
                }
            })
            .unwrap_or(default.tracing),
        logs: value
            .logs
            .map(|l| LogsConfig {
                otlp_endpoint: l.otlp_endpoint,
            })
            .unwrap_or(default.logs),
    }
}

fn pipeline_from_raw(value: RawPipelineConfig, defaults: &RawDefaults) -> PipelineSpec {
    PipelineSpec {
        name: value.name,
        source: source_from_raw(value.source, &defaults.source),
        transforms: value
            .transforms
            .into_iter()
            .map(|t| transform_from_raw(t, &defaults.transform))
            .collect(),
        sinks: value
            .sinks
            .into_iter()
            .map(|s| sink_from_raw(s, &defaults.sink))
            .collect(),
        channel_capacity: value.channel_capacity,
    }
}

fn source_from_raw(value: RawSourceConfig, defaults: &RawSourceDefaults) -> SourceSpec {
    SourceSpec {
        kind: value.kind,
        config: Value::Object(value.config),
        retry: value.retry.or_else(|| defaults.retry.clone()),
    }
}

/// Shallow merge: an explicit per-component value entirely wins over the
/// default. Deep-merging policy structs would buy little for the
/// configuration cost — operators who want different `RetryPolicy` shapes
/// just spell them out in full on the component.
fn transform_from_raw(value: RawTransformConfig, defaults: &RawTransformDefaults) -> TransformSpec {
    TransformSpec {
        kind: value.kind,
        config: Value::Object(value.config),
        on_error: value.on_error.or(defaults.on_error).map(Into::into),
    }
}

fn sink_from_raw(value: RawSinkConfig, defaults: &RawSinkDefaults) -> SinkSpec {
    SinkSpec {
        kind: value.kind,
        config: Value::Object(value.config),
        on_error: value.on_error.or(defaults.on_error).map(Into::into),
        retry: value.retry.or_else(|| defaults.retry.clone()),
    }
}

impl From<RawErrorPolicyConfig> for ErrorPolicyConfig {
    fn from(value: RawErrorPolicyConfig) -> Self {
        match value {
            RawErrorPolicyConfig::Drop => ErrorPolicyConfig::Drop,
            RawErrorPolicyConfig::FailPipeline => ErrorPolicyConfig::FailPipeline,
        }
    }
}

fn toml_table_to_json(table: Table) -> Value {
    Value::Object(
        table
            .into_iter()
            .map(|(key, value)| (key, toml_value_to_json(value)))
            .collect::<JsonMap<String, Value>>(),
    )
}

fn toml_value_to_json(value: TomlValue) -> Value {
    match value {
        TomlValue::String(value) => Value::String(value),
        TomlValue::Integer(value) => Value::Number(JsonNumber::from(value)),
        TomlValue::Float(value) => {
            Value::Number(JsonNumber::from_f64(value).expect("TOML float should be finite"))
        }
        TomlValue::Boolean(value) => Value::Bool(value),
        TomlValue::Datetime(value) => Value::String(value.to_string()),
        TomlValue::Array(values) => {
            Value::Array(values.into_iter().map(toml_value_to_json).collect())
        }
        TomlValue::Table(table) => toml_table_to_json(table),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::retry::ExhaustedPolicy;

    #[test]
    fn preserves_arbitrary_component_fields() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "plugin-pipeline"
            channel_capacity = 16

            [pipelines.source]
            type = "plugin_source"
            nested = { enabled = true, limit = 3 }
            labels = ["a", "b"]

            [[pipelines.transforms]]
            type = "plugin_transform"
            on_error = "fail_pipeline"
            script = "return value"
            timeout_secs = 10

            [[pipelines.sinks]]
            type = "plugin_sink"
            endpoint = "https://example.test"
            headers = { authorization = "token" }
            "#,
        )
        .unwrap();

        assert_eq!(config.pipelines.len(), 1);
        let pipeline = &config.pipelines[0];
        assert_eq!(pipeline.channel_capacity, Some(16));
        assert_eq!(pipeline.source.kind, "plugin_source");
        assert_eq!(
            pipeline.source.config,
            json!({
                "nested": { "enabled": true, "limit": 3 },
                "labels": ["a", "b"]
            })
        );
        assert_eq!(pipeline.transforms[0].kind, "plugin_transform");
        assert_eq!(
            pipeline.transforms[0].on_error,
            Some(ErrorPolicyConfig::FailPipeline)
        );
        assert_eq!(
            pipeline.transforms[0].config,
            json!({
                "script": "return value",
                "timeout_secs": 10
            })
        );
        assert_eq!(pipeline.sinks[0].kind, "plugin_sink");
        assert_eq!(pipeline.sinks[0].on_error, None);
        assert_eq!(pipeline.sinks[0].retry, None);
        assert_eq!(
            pipeline.sinks[0].config,
            json!({
                "endpoint": "https://example.test",
                "headers": { "authorization": "token" }
            })
        );
    }

    #[test]
    fn parses_retry_policy_with_dead_letter() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "with-retry"

            [pipelines.source]
            type = "noop"

            [[pipelines.sinks]]
            type = "noop"
            target = "x"

            [pipelines.sinks.retry]
            max_attempts = 5
            initial_delay_ms = 200
            backoff_multiplier = 2.0
            max_delay_ms = 5000

            [pipelines.sinks.retry.on_exhausted]
            kind = "dead_letter"
            path = "/tmp/dlq.jsonl"
            "#,
        )
        .unwrap();

        let sink = &config.pipelines[0].sinks[0];
        // The component config bucket does not leak the retry / on_error keys.
        assert_eq!(sink.config, json!({ "target": "x" }));
        let retry = sink.retry.as_ref().expect("retry should deserialize");
        assert_eq!(retry.max_attempts, 5);
        assert_eq!(retry.initial_delay_ms, 200);
        assert_eq!(retry.backoff_multiplier, 2.0);
        assert_eq!(retry.max_delay_ms, 5000);
        assert_eq!(
            retry.on_exhausted,
            ExhaustedPolicy::DeadLetter {
                path: PathBuf::from("/tmp/dlq.jsonl")
            }
        );
    }

    #[test]
    fn defaults_retry_to_none_when_omitted() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "no-retry"

            [pipelines.source]
            type = "noop"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        assert_eq!(config.pipelines[0].sinks[0].retry, None);
    }

    #[test]
    fn load_reads_file_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("courier.toml");
        std::fs::write(
            &path,
            r#"
            [[pipelines]]
            name = "from-disk"

            [pipelines.source]
            type = "noop"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.pipelines.len(), 1);
        assert_eq!(config.pipelines[0].name, "from-disk");
    }

    #[test]
    fn load_reports_missing_file_with_path_context() {
        let err = Config::load("/nonexistent/courier.toml").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("/nonexistent/courier.toml"), "{msg}");
    }

    #[test]
    fn from_toml_str_reports_parse_error() {
        let err = Config::from_toml_str("not valid toml ===").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to parse TOML config"), "{msg}");
    }

    #[test]
    fn from_json_str_preserves_arbitrary_component_fields() {
        let config = Config::from_json_str(
            r#"{
              "pipelines": [
                {
                  "name": "plugin-pipeline",
                  "channel_capacity": 16,
                  "source": {
                    "type": "plugin_source",
                    "nested": { "enabled": true, "limit": 3 },
                    "labels": ["a", "b"]
                  },
                  "transforms": [
                    {
                      "type": "plugin_transform",
                      "on_error": "fail_pipeline",
                      "script": "return value",
                      "timeout_secs": 10
                    }
                  ],
                  "sinks": [
                    {
                      "type": "plugin_sink",
                      "endpoint": "https://example.test",
                      "headers": { "authorization": "token" }
                    }
                  ]
                }
              ]
            }"#,
        )
        .unwrap();

        assert_eq!(config.pipelines.len(), 1);
        let pipeline = &config.pipelines[0];
        assert_eq!(pipeline.channel_capacity, Some(16));
        assert_eq!(
            pipeline.source.config,
            json!({
                "nested": { "enabled": true, "limit": 3 },
                "labels": ["a", "b"]
            })
        );
        assert_eq!(
            pipeline.transforms[0].on_error,
            Some(ErrorPolicyConfig::FailPipeline)
        );
        assert_eq!(
            pipeline.sinks[0].config,
            json!({
                "endpoint": "https://example.test",
                "headers": { "authorization": "token" }
            })
        );
    }

    #[test]
    fn from_json_str_reports_parse_error() {
        let err = Config::from_json_str("{ not valid json ===").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to parse JSON config"), "{msg}");
    }

    #[test]
    fn load_dispatches_on_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("courier.json");
        std::fs::write(
            &path,
            r#"{
              "pipelines": [
                {
                  "name": "from-json",
                  "source": { "type": "noop" },
                  "sinks": [{ "type": "noop" }]
                }
              ]
            }"#,
        )
        .unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.pipelines.len(), 1);
        assert_eq!(config.pipelines[0].name, "from-json");
    }

    #[test]
    fn load_directory_concatenates_pipelines_in_sorted_order() {
        let dir = tempfile::tempdir().unwrap();
        // Written out-of-order on purpose — load should sort by path.
        std::fs::write(
            dir.path().join("b.toml"),
            r#"
            [[pipelines]]
            name = "second"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("a.json"),
            r#"{
              "pipelines": [
                {
                  "name": "first",
                  "source": { "type": "noop" },
                  "sinks": [{ "type": "noop" }]
                }
              ]
            }"#,
        )
        .unwrap();
        // Unsupported extension — should be ignored.
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let config = Config::load(dir.path()).unwrap();
        let names: Vec<_> = config.pipelines.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second"]);
    }

    #[test]
    fn load_directory_rejects_duplicate_pipeline_names() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
            [[pipelines]]
            name = "dup"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
        "#;
        std::fs::write(dir.path().join("a.toml"), body).unwrap();
        std::fs::write(dir.path().join("b.toml"), body).unwrap();

        let err = Config::load(dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("duplicate pipeline name 'dup'"), "{msg}");
    }

    #[test]
    fn load_directory_propagates_parse_error_with_file_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.toml"), "not valid toml ===").unwrap();

        let err = Config::load(dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("broken.toml"), "{msg}");
    }

    #[test]
    fn load_empty_directory_yields_no_pipelines() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert!(config.pipelines.is_empty());
    }

    #[test]
    fn load_rejects_unsupported_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("courier.yaml");
        std::fs::write(&path, "pipelines: []").unwrap();

        let err = Config::load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported config file extension"), "{msg}");
    }

    // -----------------------------------------------------------------
    // [defaults]
    // -----------------------------------------------------------------

    fn dlq_at(path: &str) -> ExhaustedPolicy {
        ExhaustedPolicy::DeadLetter {
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn defaults_apply_when_components_omit_fields() {
        let config = Config::from_toml_str(
            r#"
            [defaults.sink]
            on_error = "fail_pipeline"

            [defaults.sink.retry]
            max_attempts = 5
            initial_delay_ms = 200
            backoff_multiplier = 2.0
            max_delay_ms = 5000

            [defaults.sink.retry.on_exhausted]
            kind = "dead_letter"
            path = "/var/log/dlq.jsonl"

            [defaults.transform]
            on_error = "drop"

            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"

            [[pipelines.transforms]]
            type = "noop"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let p = &config.pipelines[0];
        assert_eq!(p.transforms[0].on_error, Some(ErrorPolicyConfig::Drop));
        assert_eq!(p.sinks[0].on_error, Some(ErrorPolicyConfig::FailPipeline));
        let retry = p.sinks[0].retry.as_ref().expect("default retry");
        assert_eq!(retry.max_attempts, 5);
        assert_eq!(retry.on_exhausted, dlq_at("/var/log/dlq.jsonl"));
    }

    #[test]
    fn component_value_overrides_default() {
        // Both defaults and component-level fields are set — component wins.
        let config = Config::from_toml_str(
            r#"
            [defaults.sink]
            on_error = "fail_pipeline"

            [defaults.sink.retry]
            max_attempts = 5
            initial_delay_ms = 200
            backoff_multiplier = 2.0
            max_delay_ms = 5000

            [defaults.sink.retry.on_exhausted]
            kind = "dead_letter"
            path = "/default.jsonl"

            [defaults.transform]
            on_error = "drop"

            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"

            [[pipelines.transforms]]
            type = "noop"
            on_error = "fail_pipeline"

            [[pipelines.sinks]]
            type = "noop"
            on_error = "drop"

            [pipelines.sinks.retry]
            max_attempts = 1
            initial_delay_ms = 0
            backoff_multiplier = 1.0
            max_delay_ms = 0
            on_exhausted = { kind = "propagate" }
            "#,
        )
        .unwrap();

        let p = &config.pipelines[0];
        assert_eq!(
            p.transforms[0].on_error,
            Some(ErrorPolicyConfig::FailPipeline),
        );
        assert_eq!(p.sinks[0].on_error, Some(ErrorPolicyConfig::Drop));
        let retry = p.sinks[0].retry.as_ref().expect("component retry");
        assert_eq!(retry.max_attempts, 1);
        assert_eq!(retry.on_exhausted, ExhaustedPolicy::Propagate);
    }

    #[test]
    fn shallow_merge_replaces_whole_retry_block() {
        // Component supplies retry but not on_error — only the missing
        // field falls back to the default; retry is taken whole, not
        // merged field-by-field with the default's retry.
        let config = Config::from_toml_str(
            r#"
            [defaults.sink]
            on_error = "fail_pipeline"

            [defaults.sink.retry]
            max_attempts = 9
            initial_delay_ms = 999
            backoff_multiplier = 9.0
            max_delay_ms = 99999
            on_exhausted = { kind = "dead_letter", path = "/default.jsonl" }

            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"

            [[pipelines.sinks]]
            type = "noop"

            [pipelines.sinks.retry]
            max_attempts = 2
            initial_delay_ms = 1
            backoff_multiplier = 1.0
            max_delay_ms = 5
            on_exhausted = { kind = "propagate" }
            "#,
        )
        .unwrap();

        let sink = &config.pipelines[0].sinks[0];
        // on_error came from defaults (component omitted it).
        assert_eq!(sink.on_error, Some(ErrorPolicyConfig::FailPipeline));
        // retry is the component's retry verbatim, not a merge.
        let retry = sink.retry.as_ref().unwrap();
        assert_eq!(retry.max_attempts, 2);
        assert_eq!(retry.initial_delay_ms, 1);
        assert_eq!(retry.backoff_multiplier, 1.0);
        assert_eq!(retry.max_delay_ms, 5);
        assert_eq!(retry.on_exhausted, ExhaustedPolicy::Propagate);
    }

    #[test]
    fn defaults_only_partial_sink_block_works() {
        // [defaults.sink] sets only retry; on_error left None means
        // components without on_error keep on_error = None (i.e. fall
        // back to the runtime default of Drop).
        let config = Config::from_toml_str(
            r#"
            [defaults.sink.retry]
            max_attempts = 4
            initial_delay_ms = 50
            backoff_multiplier = 2.0
            max_delay_ms = 1000
            on_exhausted = { kind = "propagate" }

            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let sink = &config.pipelines[0].sinks[0];
        assert_eq!(sink.on_error, None);
        let retry = sink.retry.as_ref().unwrap();
        assert_eq!(retry.max_attempts, 4);
    }

    #[test]
    fn defaults_only_apply_to_their_own_category() {
        // A sink default must not bleed into transforms (and vice versa).
        let config = Config::from_toml_str(
            r#"
            [defaults.sink]
            on_error = "fail_pipeline"

            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"

            [[pipelines.transforms]]
            type = "noop"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let p = &config.pipelines[0];
        assert_eq!(p.transforms[0].on_error, None);
        assert_eq!(p.sinks[0].on_error, Some(ErrorPolicyConfig::FailPipeline));
    }

    #[test]
    fn json_and_toml_parse_defaults_identically() {
        let toml = Config::from_toml_str(
            r#"
            [defaults.sink]
            on_error = "fail_pipeline"

            [defaults.sink.retry]
            max_attempts = 3
            initial_delay_ms = 100
            backoff_multiplier = 2.0
            max_delay_ms = 1000
            on_exhausted = { kind = "dead_letter", path = "/dlq.jsonl" }

            [defaults.transform]
            on_error = "drop"

            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"
            [[pipelines.transforms]]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let json = Config::from_json_str(
            r#"{
              "defaults": {
                "sink": {
                  "on_error": "fail_pipeline",
                  "retry": {
                    "max_attempts": 3,
                    "initial_delay_ms": 100,
                    "backoff_multiplier": 2.0,
                    "max_delay_ms": 1000,
                    "on_exhausted": { "kind": "dead_letter", "path": "/dlq.jsonl" }
                  }
                },
                "transform": { "on_error": "drop" }
              },
              "pipelines": [
                {
                  "name": "p",
                  "source": { "type": "noop" },
                  "transforms": [{ "type": "noop" }],
                  "sinks": [{ "type": "noop" }]
                }
              ]
            }"#,
        )
        .unwrap();

        assert_eq!(toml, json);
    }

    #[test]
    fn directory_mode_keeps_defaults_per_file() {
        // Two files: one declares a sink default, the other does not.
        // The sink in the second file must NOT inherit the first file's
        // default. This guarantees load order can't change behavior.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.toml"),
            r#"
            [defaults.sink]
            on_error = "fail_pipeline"

            [[pipelines]]
            name = "with-default"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.toml"),
            r#"
            [[pipelines]]
            name = "no-default"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let config = Config::load(dir.path()).unwrap();
        let by_name: std::collections::HashMap<_, _> = config
            .pipelines
            .iter()
            .map(|p| (p.name.as_str(), p))
            .collect();
        assert_eq!(
            by_name["with-default"].sinks[0].on_error,
            Some(ErrorPolicyConfig::FailPipeline),
        );
        assert_eq!(by_name["no-default"].sinks[0].on_error, None);
    }

    #[test]
    fn validate_rejects_empty_pipeline_name() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "  "
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("pipelines[0].name"), "{msg}");
        assert!(msg.contains("must not be empty"), "{msg}");
    }

    #[test]
    fn validate_rejects_duplicate_pipeline_names_in_single_config() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "dup"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"

            [[pipelines]]
            name = "dup"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("pipelines[1].name"), "{msg}");
        assert!(msg.contains("duplicate pipeline name 'dup'"), "{msg}");
    }

    #[test]
    fn validate_rejects_zero_channel_capacity() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"
            channel_capacity = 0
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("pipeline 'p'.channel_capacity"), "{msg}");
        assert!(msg.contains("greater than 0"), "{msg}");
    }

    #[test]
    fn validate_rejects_missing_sinks() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("pipeline 'p'.sinks"), "{msg}");
        assert!(msg.contains("at least one sink is required"), "{msg}");
    }

    #[test]
    fn parses_source_retry_policy() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "api_poll"
            url = "https://example.test/data"
            interval_secs = 30

            [pipelines.source.retry]
            max_attempts = 5
            initial_delay_ms = 200
            backoff_multiplier = 2.0
            max_delay_ms = 5000
            on_exhausted = { kind = "propagate" }

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let source = &config.pipelines[0].source;
        // retry/on_error keys never leak into the component config bucket.
        assert_eq!(
            source.config,
            json!({ "url": "https://example.test/data", "interval_secs": 30 })
        );
        let retry = source.retry.as_ref().expect("source retry should parse");
        assert_eq!(retry.max_attempts, 5);
        assert_eq!(retry.initial_delay_ms, 200);
    }

    #[test]
    fn source_defaults_apply_when_omitted() {
        let config = Config::from_toml_str(
            r#"
            [defaults.source.retry]
            max_attempts = 4
            initial_delay_ms = 50
            backoff_multiplier = 2.0
            max_delay_ms = 1000
            on_exhausted = { kind = "propagate" }

            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "api_poll"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let retry = config.pipelines[0]
            .source
            .retry
            .as_ref()
            .expect("default source retry should apply");
        assert_eq!(retry.max_attempts, 4);
    }

    #[test]
    fn source_component_retry_overrides_default() {
        let config = Config::from_toml_str(
            r#"
            [defaults.source.retry]
            max_attempts = 9
            initial_delay_ms = 999
            backoff_multiplier = 2.0
            max_delay_ms = 9999
            on_exhausted = { kind = "propagate" }

            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "api_poll"

            [pipelines.source.retry]
            max_attempts = 2
            initial_delay_ms = 1
            backoff_multiplier = 1.0
            max_delay_ms = 5
            on_exhausted = { kind = "propagate" }

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let retry = config.pipelines[0].source.retry.as_ref().unwrap();
        assert_eq!(retry.max_attempts, 2);
        assert_eq!(retry.initial_delay_ms, 1);
    }

    #[test]
    fn validate_rejects_invalid_source_retry_policy() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "api_poll"

            [pipelines.source.retry]
            max_attempts = 0
            initial_delay_ms = 1
            backoff_multiplier = 1.0
            max_delay_ms = 1

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(
            msg.contains("pipeline 'p'.source.retry.max_attempts"),
            "{msg}"
        );
    }

    #[test]
    fn validate_rejects_invalid_retry_policy() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            [pipelines.sinks.retry]
            max_attempts = 0
            initial_delay_ms = 1
            backoff_multiplier = 1.0
            max_delay_ms = 1
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(
            msg.contains("pipeline 'p'.sinks[0].retry.max_attempts"),
            "{msg}"
        );
    }

    #[test]
    fn validate_rejects_dead_letter_parent_that_is_not_directory() {
        let dir = tempfile::tempdir().unwrap();
        let parent_file = dir.path().join("not-a-dir");
        std::fs::write(&parent_file, "").unwrap();
        let dlq_path = parent_file.join("dlq.jsonl");
        let config = Config::from_toml_str(&format!(
            r#"
            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            [pipelines.sinks.retry]
            max_attempts = 1
            initial_delay_ms = 0
            backoff_multiplier = 1.0
            max_delay_ms = 0
            on_exhausted = {{ kind = "dead_letter", path = "{}" }}
            "#,
            dlq_path.display()
        ))
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(
            msg.contains("pipeline 'p'.sinks[0].retry.on_exhausted.path"),
            "{msg}"
        );
        assert!(msg.contains("is not a directory"), "{msg}");
    }

    // -----------------------------------------------------------------
    // [observability]
    // -----------------------------------------------------------------

    fn minimal_pipeline_block() -> &'static str {
        r#"
            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
        "#
    }

    #[test]
    fn observability_absent_is_none_and_validates() {
        let config = Config::from_toml_str(minimal_pipeline_block()).unwrap();
        assert_eq!(config.observability, None);
        config.validate().unwrap();
    }

    #[test]
    fn observability_empty_block_yields_defaults() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability]
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let obs = config
            .observability
            .clone()
            .expect("observability should parse");
        assert_eq!(obs, ObservabilityConfig::default());
        config.validate().unwrap();
    }

    #[test]
    fn observability_full_block_round_trips() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability]
            service_name = "courier-prod"
            log_format = "json"
            log_level = "courier=debug,hyper=warn"
            log_keys = true

            [observability.metrics]
            otlp_endpoint = "http://collector:4317"
            export_interval_ms = 5000

            [observability.tracing]
            otlp_endpoint = "http://collector:4317"
            sample_ratio = 0.25

            [observability.logs]
            otlp_endpoint = "http://collector:4317"
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();

        let obs = config.observability.unwrap();
        assert_eq!(obs.log_format, LogFormat::Json);
        assert_eq!(obs.log_level.as_deref(), Some("courier=debug,hyper=warn"));
        assert!(obs.log_keys);
        assert_eq!(
            obs.metrics.otlp_endpoint.as_deref(),
            Some("http://collector:4317")
        );
        assert_eq!(obs.metrics.export_interval_ms, 5000);
        assert_eq!(
            obs.tracing.otlp_endpoint.as_deref(),
            Some("http://collector:4317")
        );
        assert_eq!(obs.tracing.sample_ratio, 0.25);
        assert_eq!(obs.service_name, "courier-prod");
        assert_eq!(
            obs.logs.otlp_endpoint.as_deref(),
            Some("http://collector:4317")
        );
    }

    #[test]
    fn json_and_toml_parse_observability_identically() {
        let toml = Config::from_toml_str(&format!(
            r#"
            [observability]
            service_name = "svc"
            log_format = "json"
            log_level = "info"

            [observability.tracing]
            sample_ratio = 0.5
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();

        let json = Config::from_json_str(
            r#"{
              "observability": {
                "service_name": "svc",
                "log_format": "json",
                "log_level": "info",
                "tracing": { "sample_ratio": 0.5 }
              },
              "pipelines": [
                {
                  "name": "p",
                  "source": { "type": "noop" },
                  "sinks": [{ "type": "noop" }]
                }
              ]
            }"#,
        )
        .unwrap();

        assert_eq!(toml, json);
    }

    #[test]
    fn tracing_service_name_is_rejected() {
        let err = Config::from_toml_str(&format!(
            r#"
            [observability.tracing]
            service_name = "legacy-svc"
            {}"#,
            minimal_pipeline_block()
        ))
        .expect_err("legacy tracing service_name should be rejected");

        let msg = format!("{err:#}");
        assert!(msg.contains("unknown field `service_name`"), "{msg}");
    }

    #[test]
    fn directory_mode_preserves_observability() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.toml"),
            r#"
            [observability]
            service_name = "courier-prod"
            log_format = "json"
            log_level = "courier=debug"
            log_keys = true

            [observability.metrics]
            otlp_endpoint = "http://metrics:4317"
            export_interval_ms = 5000

            [observability.tracing]
            otlp_endpoint = "http://traces:4317"
            sample_ratio = 0.25

            [[pipelines]]
            name = "p1"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.toml"),
            r#"
            [[pipelines]]
            name = "p2"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let config = Config::load(dir.path()).unwrap();
        assert_eq!(config.pipelines.len(), 2);

        let obs = config.observability.expect("observability should load");
        assert_eq!(obs.log_format, LogFormat::Json);
        assert_eq!(obs.log_level.as_deref(), Some("courier=debug"));
        assert!(obs.log_keys);
        assert_eq!(
            obs.metrics.otlp_endpoint.as_deref(),
            Some("http://metrics:4317")
        );
        assert_eq!(obs.metrics.export_interval_ms, 5000);
        assert_eq!(
            obs.tracing.otlp_endpoint.as_deref(),
            Some("http://traces:4317")
        );
        assert_eq!(obs.tracing.sample_ratio, 0.25);
        assert_eq!(obs.service_name, "courier-prod");
    }

    #[test]
    fn directory_mode_rejects_duplicate_observability_blocks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.toml"),
            r#"
            [observability]
            log_level = "info"

            [[pipelines]]
            name = "p1"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.toml"),
            r#"
            [observability]
            log_level = "debug"

            [[pipelines]]
            name = "p2"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", Config::load(dir.path()).unwrap_err());
        assert!(msg.contains("duplicate observability config"), "{msg}");
        assert!(msg.contains("a.toml"), "{msg}");
        assert!(msg.contains("b.toml"), "{msg}");
    }

    #[test]
    fn validate_rejects_sample_ratio_above_one() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability.tracing]
            sample_ratio = 1.5
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("observability.tracing.sample_ratio"), "{msg}");
    }

    #[test]
    fn validate_rejects_negative_sample_ratio() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability.tracing]
            sample_ratio = -0.1
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        config.validate().unwrap_err();
    }

    #[test]
    fn validate_rejects_empty_service_name() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability]
            service_name = ""
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("observability.service_name"), "{msg}");
    }

    #[test]
    fn validate_rejects_zero_export_interval() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability.metrics]
            export_interval_ms = 0
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(
            msg.contains("observability.metrics.export_interval_ms"),
            "{msg}"
        );
    }

    #[test]
    fn validate_rejects_empty_log_level() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability]
            log_level = "   "
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("observability.log_level"), "{msg}");
    }

    #[test]
    fn validate_rejects_invalid_log_level() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability]
            log_level = "courier==debug"
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("observability.log_level"), "{msg}");
        assert!(msg.contains("invalid log filter directive"), "{msg}");
    }

    #[test]
    fn observability_rejects_unknown_top_level_field() {
        let err = Config::from_toml_str(&format!(
            r#"
            [observability]
            unknown_field = "oops"
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown_field"), "{msg}");
    }
}
