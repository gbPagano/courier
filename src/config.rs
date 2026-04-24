use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map as JsonMap, Number as JsonNumber, Value};
use toml::{Table, Value as TomlValue};

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

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub pipelines: Vec<PipelineSpec>,
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
    ///   rejected.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.is_dir() {
            Self::load_dir(path)
        } else {
            Self::load_file(path)
        }
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
        };
        let mut seen_names: std::collections::HashMap<String, PathBuf> =
            std::collections::HashMap::new();

        for file in files {
            let part = Self::load_file(&file)?;
            for pipeline in part.pipelines {
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
    pipelines: Vec<RawPipelineConfig>,
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
        Self {
            pipelines: value.pipelines.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<RawPipelineConfig> for PipelineSpec {
    fn from(value: RawPipelineConfig) -> Self {
        Self {
            name: value.name,
            source: value.source.into(),
            transforms: value.transforms.into_iter().map(Into::into).collect(),
            sinks: value.sinks.into_iter().map(Into::into).collect(),
            channel_capacity: value.channel_capacity,
        }
    }
}

impl From<RawSourceConfig> for SourceSpec {
    fn from(value: RawSourceConfig) -> Self {
        Self {
            kind: value.kind,
            config: Value::Object(value.config),
        }
    }
}

impl From<RawTransformConfig> for TransformSpec {
    fn from(value: RawTransformConfig) -> Self {
        Self {
            kind: value.kind,
            config: Value::Object(value.config),
            on_error: value.on_error.map(Into::into),
        }
    }
}

impl From<RawSinkConfig> for SinkSpec {
    fn from(value: RawSinkConfig) -> Self {
        Self {
            kind: value.kind,
            config: Value::Object(value.config),
            on_error: value.on_error.map(Into::into),
            retry: value.retry,
        }
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
}
