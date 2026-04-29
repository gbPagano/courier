use std::fmt;

use super::redact::{RedactedOptionStr, RedactedStr};

#[derive(Clone, PartialEq)]
pub struct ObservabilityConfig {
    pub service_name: String,
    pub log_format: LogFormat,
    pub log_level: Option<String>,
    pub log_keys: bool,
    pub metrics: MetricsConfig,
    pub tracing: TracingConfig,
    pub logs: LogsConfig,
}

impl fmt::Debug for ObservabilityConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObservabilityConfig")
            .field("service_name", &RedactedStr(&self.service_name))
            .field("log_format", &self.log_format)
            .field("log_level", &RedactedOptionStr(self.log_level.as_deref()))
            .field("log_keys", &self.log_keys)
            .field("metrics", &self.metrics)
            .field("tracing", &self.tracing)
            .field("logs", &self.logs)
            .finish()
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

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

#[derive(Clone, PartialEq)]
pub struct MetricsConfig {
    pub otlp_endpoint: Option<String>,
    pub export_interval_ms: u64,
}

impl fmt::Debug for MetricsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetricsConfig")
            .field(
                "otlp_endpoint",
                &RedactedOptionStr(self.otlp_endpoint.as_deref()),
            )
            .field("export_interval_ms", &self.export_interval_ms)
            .finish()
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: None,
            export_interval_ms: 15_000,
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct TracingConfig {
    pub otlp_endpoint: Option<String>,
    pub sample_ratio: f64,
}

impl fmt::Debug for TracingConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TracingConfig")
            .field(
                "otlp_endpoint",
                &RedactedOptionStr(self.otlp_endpoint.as_deref()),
            )
            .field("sample_ratio", &self.sample_ratio)
            .finish()
    }
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: None,
            sample_ratio: 0.1,
        }
    }
}

#[derive(Clone, Default, PartialEq)]
pub struct LogsConfig {
    pub otlp_endpoint: Option<String>,
}

impl fmt::Debug for LogsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogsConfig")
            .field(
                "otlp_endpoint",
                &RedactedOptionStr(self.otlp_endpoint.as_deref()),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{Config, LogFormat, ObservabilityConfig};

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
