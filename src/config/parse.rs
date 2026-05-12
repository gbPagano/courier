use std::path::Path;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;
use toml::{Table, Value as TomlValue};

use super::interpolate::interpolate_config_value;
use super::raw::RawConfig;
use super::redact::redact_secret_values_in_text;
use super::types::Config;

pub fn parse_config<T: DeserializeOwned>(kind: &str, config: Value) -> Result<T> {
    deserialize_json_value(config)
        .with_context(|| format!("invalid config for component type '{kind}'"))
}

impl Config {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        Self::from_toml_str_with_base(s, None)
    }

    pub(super) fn from_toml_str_with_base(s: &str, base_dir: Option<&Path>) -> Result<Self> {
        let toml_value: TomlValue = toml::from_str(s).context("failed to parse TOML config")?;
        let mut json_value =
            toml_value_to_json(toml_value).context("failed to parse TOML config")?;
        interpolate_config_value(&mut json_value, base_dir)?;
        resolve_script_file_paths(&mut json_value, base_dir);
        let raw: RawConfig =
            deserialize_json_value(json_value).context("failed to parse TOML config")?;
        Ok(raw.into())
    }

    pub fn from_json_str(s: &str) -> Result<Self> {
        Self::from_json_str_with_base(s, None)
    }

    pub(super) fn from_json_str_with_base(s: &str, base_dir: Option<&Path>) -> Result<Self> {
        let mut json_value: Value =
            serde_json::from_str(s).context("failed to parse JSON config")?;
        interpolate_config_value(&mut json_value, base_dir)?;
        resolve_script_file_paths(&mut json_value, base_dir);
        let raw: RawConfig =
            deserialize_json_value(json_value).context("failed to parse JSON config")?;
        Ok(raw.into())
    }
}

pub(super) fn parse_by_extension(
    path: &Path,
    content: &str,
    base_dir: Option<&Path>,
) -> Result<Config> {
    match path.extension().and_then(|s| s.to_str()) {
        Some("json") => Config::from_json_str_with_base(content, base_dir),
        Some("toml") | None => Config::from_toml_str_with_base(content, base_dir),
        Some(other) => Err(anyhow::anyhow!(
            "unsupported config file extension '.{other}' (expected '.toml' or '.json')"
        )),
    }
}

fn deserialize_json_value<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value)
        .map_err(|err| anyhow::anyhow!("{}", redact_secret_values_in_text(&err.to_string())))
}

fn resolve_script_file_paths(value: &mut Value, base_dir: Option<&Path>) {
    let Some(base_dir) = base_dir else {
        return;
    };
    let Some(pipelines) = value.get_mut("pipelines").and_then(Value::as_array_mut) else {
        return;
    };

    for pipeline in pipelines {
        let Some(transforms) = pipeline.get_mut("transforms").and_then(Value::as_array_mut) else {
            continue;
        };

        for transform in transforms {
            if transform.get("type").and_then(Value::as_str) != Some("script") {
                continue;
            }

            let resolved = transform
                .get("script_file")
                .and_then(Value::as_str)
                .and_then(|script_file| {
                    let path = Path::new(script_file);
                    path.is_relative()
                        .then(|| base_dir.join(path).to_string_lossy().into_owned())
                });

            if let Some(resolved) = resolved {
                transform["script_file"] = Value::String(resolved);
            }
        }
    }
}

fn toml_table_to_json(table: Table) -> Result<Value> {
    Ok(Value::Object(
        table
            .into_iter()
            .map(|(key, value)| toml_value_to_json(value).map(|value| (key, value)))
            .collect::<Result<serde_json::Map<String, Value>>>()?,
    ))
}

fn toml_value_to_json(value: TomlValue) -> Result<Value> {
    Ok(match value {
        TomlValue::String(value) => Value::String(value),
        TomlValue::Integer(value) => Value::Number(serde_json::Number::from(value)),
        TomlValue::Float(value) => Value::Number(
            serde_json::Number::from_f64(value)
                .ok_or_else(|| anyhow::anyhow!("non-finite TOML floats are not supported"))?,
        ),
        TomlValue::Boolean(value) => Value::Bool(value),
        TomlValue::Datetime(value) => Value::String(value.to_string()),
        TomlValue::Array(values) => Value::Array(
            values
                .into_iter()
                .map(toml_value_to_json)
                .collect::<Result<Vec<_>>>()?,
        ),
        TomlValue::Table(table) => toml_table_to_json(table)?,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use crate::config::{
        Config, ENV_LOCK, ErrorPolicyConfig, REDACTED_SECRET, clear_secret_values_for_test,
        register_secret_value,
    };
    use crate::retry::ExhaustedPolicy;

    use super::parse_config;

    fn set_env_var(key: &str, value: &str) {
        unsafe {
            std::env::set_var(key, value);
        }
    }

    fn remove_env_var(key: &str) {
        unsafe {
            std::env::remove_var(key);
        }
    }

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
    fn from_toml_str_reports_parse_error() {
        let err = Config::from_toml_str("not valid toml ===").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to parse TOML config"), "{msg}");
    }

    #[test]
    fn from_toml_str_rejects_non_finite_floats_without_panicking() {
        for value in ["nan", "inf", "-inf"] {
            let err = Config::from_toml_str(&format!(
                r#"
                [[pipelines]]
                name = "p"

                [pipelines.source]
                type = "noop"
                threshold = {value}

                [[pipelines.sinks]]
                type = "noop"
                "#
            ))
            .unwrap_err();

            let msg = format!("{err:#}");
            assert!(msg.contains("failed to parse TOML config"), "{msg}");
            assert!(
                msg.contains("non-finite TOML floats are not supported"),
                "{msg}"
            );
        }
    }

    #[test]
    fn in_memory_config_leaves_relative_script_file_unchanged() {
        // from_toml_str has no base_dir → resolve_script_file_paths is a no-op.
        // The relative path is left as-is; std::fs::read_to_string will later
        // resolve it against the process CWD (documented fallback behaviour).
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"

            [[pipelines.transforms]]
            type = "script"
            runtime = "rhai"
            script_file = "./scripts/transform.rhai"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let script_file = config.pipelines[0].transforms[0].config["script_file"]
            .as_str()
            .unwrap();
        assert_eq!(script_file, "./scripts/transform.rhai");
        assert!(!std::path::Path::new(script_file).is_absolute());
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
    fn config_parse_errors_redact_interpolated_secrets() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_secret_values_for_test();
        set_env_var("COURIER_TEST_CHANNEL_CAPACITY", "super-secret-capacity");

        let err = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"
            channel_capacity = "${secret:COURIER_TEST_CHANNEL_CAPACITY}"

            [pipelines.source]
            type = "noop"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("failed to parse TOML config"), "{msg}");
        assert!(!msg.contains("super-secret-capacity"), "{msg}");
        assert!(msg.contains(REDACTED_SECRET), "{msg}");

        remove_env_var("COURIER_TEST_CHANNEL_CAPACITY");
    }

    #[test]
    fn component_parse_errors_redact_registered_secrets() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_secret_values_for_test();
        register_secret_value("component-secret-value");

        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct NeedsNumber {
            n: u64,
        }

        let err = parse_config::<NeedsNumber>(
            "demo",
            json!({
                "n": "component-secret-value"
            }),
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid config for component type 'demo'"),
            "{msg}"
        );
        assert!(!msg.contains("component-secret-value"), "{msg}");
        assert!(msg.contains(REDACTED_SECRET), "{msg}");
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
        assert_eq!(
            source.config,
            json!({ "url": "https://example.test/data", "interval_secs": 30 })
        );
        let retry = source.retry.as_ref().expect("source retry should parse");
        assert_eq!(retry.max_attempts, 5);
        assert_eq!(retry.initial_delay_ms, 200);
    }
}
