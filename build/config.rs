use serde::Deserialize;
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};
use toml::{Table, Value as TomlValue};

#[derive(Debug, Clone)]
pub struct Config {
    pub pipelines: Vec<PipelineConfig>,
}

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub name: String,
    pub source: SourceConfig,
    pub transforms: Vec<TransformConfig>,
    pub sinks: Vec<SinkConfig>,
    pub channel_capacity: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub kind: String,
    pub config: JsonValue,
}

#[derive(Debug, Clone)]
pub struct TransformConfig {
    pub kind: String,
    pub config: JsonValue,
    pub on_error: Option<ErrorPolicyConfig>,
}

#[derive(Debug, Clone)]
pub struct SinkConfig {
    pub kind: String,
    pub config: JsonValue,
    pub on_error: Option<ErrorPolicyConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPolicyConfig {
    Drop,
    FailPipeline,
}

pub fn parse_config(content: &str) -> Result<Config, toml::de::Error> {
    toml::from_str::<RawConfig>(content).map(Into::into)
}

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
    config: Table,
}

#[derive(Debug, Deserialize)]
struct RawTransformConfig {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    on_error: Option<RawErrorPolicyConfig>,
    #[serde(flatten)]
    config: Table,
}

#[derive(Debug, Deserialize)]
struct RawSinkConfig {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    on_error: Option<RawErrorPolicyConfig>,
    #[serde(flatten)]
    config: Table,
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

impl From<RawPipelineConfig> for PipelineConfig {
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

impl From<RawSourceConfig> for SourceConfig {
    fn from(value: RawSourceConfig) -> Self {
        Self {
            kind: value.kind,
            config: toml_table_to_json(value.config),
        }
    }
}

impl From<RawTransformConfig> for TransformConfig {
    fn from(value: RawTransformConfig) -> Self {
        Self {
            kind: value.kind,
            config: toml_table_to_json(value.config),
            on_error: value.on_error.map(Into::into),
        }
    }
}

impl From<RawSinkConfig> for SinkConfig {
    fn from(value: RawSinkConfig) -> Self {
        Self {
            kind: value.kind,
            config: toml_table_to_json(value.config),
            on_error: value.on_error.map(Into::into),
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

fn toml_table_to_json(table: Table) -> JsonValue {
    JsonValue::Object(
        table
            .into_iter()
            .map(|(key, value)| (key, toml_value_to_json(value)))
            .collect::<JsonMap<String, JsonValue>>(),
    )
}

fn toml_value_to_json(value: TomlValue) -> JsonValue {
    match value {
        TomlValue::String(value) => JsonValue::String(value),
        TomlValue::Integer(value) => JsonValue::Number(JsonNumber::from(value)),
        TomlValue::Float(value) => {
            JsonValue::Number(JsonNumber::from_f64(value).expect("TOML float should be finite"))
        }
        TomlValue::Boolean(value) => JsonValue::Bool(value),
        TomlValue::Datetime(value) => JsonValue::String(value.to_string()),
        TomlValue::Array(values) => {
            JsonValue::Array(values.into_iter().map(toml_value_to_json).collect())
        }
        TomlValue::Table(table) => toml_table_to_json(table),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn preserves_arbitrary_component_fields() {
        let config = parse_config(
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
        assert_eq!(
            pipeline.sinks[0].config,
            json!({
                "endpoint": "https://example.test",
                "headers": { "authorization": "token" }
            })
        );
    }
}
