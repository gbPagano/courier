use serde::Deserialize;
use serde_json::{Map as JsonMap, Value};

use crate::retry::RetryPolicy;

use super::observability::{
    LogFormat, LogsConfig, MetricsConfig, ObservabilityConfig, TracingConfig,
};
use super::types::{
    Config, ErrorPolicyConfig, FanOutPolicyConfig, PipelineSpec, SinkSpec, SourceSpec,
    TransformSpec,
};

#[derive(Debug, Deserialize)]
pub(super) struct RawConfig {
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
    #[serde(default)]
    fan_out: Option<RawFanOutPolicyConfig>,
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

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum RawFanOutPolicyConfig {
    Broadcast,
    BroadcastWithDrop,
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
        fan_out: value
            .fan_out
            .map(Into::into)
            .unwrap_or(FanOutPolicyConfig::Broadcast),
    }
}

fn source_from_raw(value: RawSourceConfig, defaults: &RawSourceDefaults) -> SourceSpec {
    SourceSpec {
        kind: value.kind,
        config: Value::Object(value.config),
        retry: value.retry.or_else(|| defaults.retry.clone()),
    }
}

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

impl From<RawFanOutPolicyConfig> for FanOutPolicyConfig {
    fn from(value: RawFanOutPolicyConfig) -> Self {
        match value {
            RawFanOutPolicyConfig::Broadcast => FanOutPolicyConfig::Broadcast,
            RawFanOutPolicyConfig::BroadcastWithDrop => FanOutPolicyConfig::BroadcastWithDrop,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::config::{Config, ErrorPolicyConfig, FanOutPolicyConfig};
    use crate::retry::ExhaustedPolicy;

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
        assert_eq!(sink.on_error, Some(ErrorPolicyConfig::FailPipeline));
        let retry = sink.retry.as_ref().unwrap();
        assert_eq!(retry.max_attempts, 2);
        assert_eq!(retry.initial_delay_ms, 1);
        assert_eq!(retry.backoff_multiplier, 1.0);
        assert_eq!(retry.max_delay_ms, 5);
        assert_eq!(retry.on_exhausted, ExhaustedPolicy::Propagate);
    }

    #[test]
    fn defaults_only_partial_sink_block_works() {
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
    fn fan_out_parses_from_toml_and_defaults_to_broadcast() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"
            fan_out = "broadcast_with_drop"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"

            [[pipelines]]
            name = "q"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.pipelines[0].fan_out,
            FanOutPolicyConfig::BroadcastWithDrop
        );
        assert_eq!(config.pipelines[1].fan_out, FanOutPolicyConfig::Broadcast);
    }

    #[test]
    fn fan_out_parses_from_json() {
        let config = Config::from_json_str(
            r#"{
              "pipelines": [
                {
                  "name": "p",
                  "fan_out": "broadcast_with_drop",
                  "source": { "type": "noop" },
                  "sinks": [{ "type": "noop" }]
                }
              ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            config.pipelines[0].fan_out,
            FanOutPolicyConfig::BroadcastWithDrop
        );
    }
}
