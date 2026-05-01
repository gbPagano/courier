use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::parse::parse_by_extension;
use super::redact::redact_secret;
use super::types::Config;

impl Config {
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
        parse_by_extension(path, &content, path.parent())
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
                        redact_secret(&pipeline.name),
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

#[cfg(test)]
mod tests {
    use crate::config::{Config, ENV_LOCK, ErrorPolicyConfig, LogFormat};

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
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let config = Config::load(dir.path()).unwrap();
        let names: Vec<_> = config.pipelines.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second"]);
    }

    #[test]
    fn load_directory_interpolates_each_toml_and_json_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_env_var("COURIER_TEST_DIR_SUFFIX", "env");
        set_env_var("COURIER_TEST_DIR_URL", "https://example.test/data");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.json"),
            r#"{
              "pipelines": [
                {
                  "name": "json-${env:COURIER_TEST_DIR_SUFFIX}",
                  "source": {
                    "type": "noop",
                    "url": "${env:COURIER_TEST_DIR_URL}"
                  },
                  "sinks": [{ "type": "noop" }]
                }
              ]
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.toml"),
            r#"
            [[pipelines]]
            name = "toml-${env:COURIER_TEST_DIR_SUFFIX}"
            [pipelines.source]
            type = "noop"
            url = "${env:COURIER_TEST_DIR_URL}"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let config = Config::load(dir.path()).unwrap();
        let names: Vec<_> = config.pipelines.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["json-env", "toml-env"]);
        assert_eq!(
            config.pipelines[0].source.config["url"],
            "https://example.test/data"
        );
        assert_eq!(
            config.pipelines[1].source.config["url"],
            "https://example.test/data"
        );

        remove_env_var("COURIER_TEST_DIR_SUFFIX");
        remove_env_var("COURIER_TEST_DIR_URL");
    }

    #[test]
    fn load_resolves_script_file_relative_to_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let script_dir = dir.path().join("transforms");
        std::fs::create_dir(&script_dir).unwrap();
        std::fs::write(script_dir.join("enrich.rhai"), "fn transform(env) { env }").unwrap();

        let config_path = dir.path().join("courier.toml");
        std::fs::write(
            &config_path,
            r#"
            [[pipelines]]
            name = "script-path"

            [pipelines.source]
            type = "noop"

            [[pipelines.transforms]]
            type = "script"
            runtime = "rhai"
            script_file = "./transforms/enrich.rhai"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        let script_file = config.pipelines[0].transforms[0].config["script_file"]
            .as_str()
            .unwrap();
        assert_eq!(
            script_file,
            dir.path()
                .join("./transforms/enrich.rhai")
                .to_string_lossy()
                .as_ref()
        );
        assert!(std::path::Path::new(script_file).is_absolute());
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

    #[test]
    fn directory_mode_keeps_defaults_per_file() {
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
}
