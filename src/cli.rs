use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::{Courier, Registry};

pub const DEFAULT_CONFIG_PATH: &str = "config.toml";
pub const CONFIG_ENV_VAR: &str = "COURIER_CONFIG";

#[derive(Debug, Parser)]
#[command(
    name = "courier",
    about = "Run and inspect Courier pipeline runtimes",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

#[derive(Debug, Clone, Eq, PartialEq, Subcommand)]
pub enum CliCommand {
    /// Load config and start pipelines.
    Run {
        /// Config file or directory. Overrides COURIER_CONFIG.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Load config and build the runtime, then exit.
    Validate {
        /// Config file or directory. Overrides COURIER_CONFIG.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Print registered component kinds.
    ListComponents,
}

pub fn resolve_config_path(config: Option<PathBuf>) -> PathBuf {
    resolve_config_path_with_env(config, |key| std::env::var(key).ok())
}

fn resolve_config_path_with_env(
    config: Option<PathBuf>,
    env: impl FnOnce(&str) -> Option<String>,
) -> PathBuf {
    config.unwrap_or_else(|| {
        env(CONFIG_ENV_VAR)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
    })
}

pub fn build_runtime(path: &Path) -> Result<Courier> {
    let config = Config::load(path)?;
    let registry = Registry::with_builtins()?;
    registry
        .build_courier(config)
        .with_context(|| format!("failed to build runtime from config {}", path.display()))
}

pub fn validate_config(path: &Path) -> Result<()> {
    build_runtime(path).map(drop)
}

pub fn list_components(registry: &Registry) -> String {
    fn sorted(items: impl Iterator<Item = impl AsRef<str>>) -> Vec<String> {
        let mut values: Vec<_> = items.map(|item| item.as_ref().to_string()).collect();
        values.sort();
        values
    }

    let sources = sorted(registry.source_kinds());
    let transforms = sorted(registry.transform_kinds());
    let sinks = sorted(registry.sink_kinds());

    let mut out = String::new();
    out.push_str("sources:\n");
    for kind in sources {
        out.push_str("  ");
        out.push_str(&kind);
        out.push('\n');
    }
    out.push_str("transforms:\n");
    for kind in transforms {
        out.push_str("  ");
        out.push_str(&kind);
        out.push('\n');
    }
    out.push_str("sinks:\n");
    for kind in sinks {
        out.push_str("  ");
        out.push_str(&kind);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_prefers_arg_then_env_then_default() {
        assert_eq!(
            resolve_config_path_with_env(Some(PathBuf::from("arg.toml")), |_| {
                Some("env.toml".to_string())
            }),
            PathBuf::from("arg.toml")
        );
        assert_eq!(
            resolve_config_path_with_env(None, |_| Some("env.toml".to_string())),
            PathBuf::from("env.toml")
        );
        assert_eq!(
            resolve_config_path_with_env(None, |_| None),
            PathBuf::from(DEFAULT_CONFIG_PATH)
        );
    }

    #[test]
    fn list_components_includes_registered_builtins() {
        let registry = Registry::with_builtins().unwrap();
        let output = list_components(&registry);

        assert!(output.contains("sources:\n"));
        assert!(output.contains("  api_poll\n"));
        assert!(output.contains("  http_webhook\n"));
        assert!(output.contains("  sql_query_poll\n"));
        assert!(output.contains("transforms:\n"));
        assert!(output.contains("  script\n"));
        assert!(output.contains("sinks:\n"));
        assert!(output.contains("  sql\n"));
    }

    #[test]
    fn validate_accepts_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("valid.toml");
        std::fs::write(
            &path,
            r#"
[[pipelines]]
name = "valid"

[pipelines.source]
type = "api_poll"
url = "http://localhost/data"
interval_secs = 60

[[pipelines.sinks]]
type = "api"
url = "http://localhost/ingest"
"#,
        )
        .unwrap();

        validate_config(&path).unwrap();
    }

    #[test]
    fn validate_does_not_create_file_sink_output() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("valid.toml");
        let output_path = dir.path().join("nested/out.jsonl");
        std::fs::write(
            &config_path,
            format!(
                r#"
[[pipelines]]
name = "valid-file"

[pipelines.source]
type = "api_poll"
url = "http://localhost/data"
interval_secs = 60

[[pipelines.sinks]]
type = "file"
path = "{}"
"#,
                output_path.display()
            ),
        )
        .unwrap();

        validate_config(&config_path).unwrap();
        assert!(!output_path.exists());
        assert!(!output_path.parent().unwrap().exists());
    }

    #[test]
    fn validate_rejects_invalid_config_with_path_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid.toml");
        std::fs::write(
            &path,
            r#"
[[pipelines]]
name = "invalid"

[pipelines.source]
type = "api_poll"
url = "http://localhost/data"
interval_secs = 60

[[pipelines.sinks]]
type = "missing"
"#,
        )
        .unwrap();

        let err = validate_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to build runtime from config"), "{msg}");
        assert!(msg.contains("invalid.toml"), "{msg}");
        assert!(msg.contains("unknown sink type 'missing'"), "{msg}");
    }
}
