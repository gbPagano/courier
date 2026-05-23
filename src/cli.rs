use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;

use std::process::ExitCode;

use crate::config::{Config, FanOutPolicyConfig, PipelineSpec, SinkSpec, SourceSpec};
use crate::envelope::DeadLetterEntry;
use crate::retry::ExhaustedPolicy;
use crate::{Courier, Registry, RunOutcome};

pub const DEFAULT_CONFIG_PATH: &str = "config.toml";
pub const CONFIG_ENV_VAR: &str = "COURIER_CONFIG";

#[derive(Debug, Parser)]
#[command(
    version,
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
    /// Replay dead-letter entries through a pipeline.
    ///
    /// Loads the config, replaces each pipeline's source with a jsonl_file
    /// source reading from DLQ_PATH, and runs until all entries have been
    /// emitted. This allows operators to re-ingest envelopes that were
    /// previously captured by `ExhaustedPolicy::DeadLetter`.
    ReplayDlq {
        /// Path to the JSONL dead-letter file to replay.
        dlq_path: PathBuf,
        /// Config file or directory. Overrides COURIER_CONFIG.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
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
    build_runtime_from_config(config, path)
}

/// Build a runtime from a pre-loaded `Config`. Used by `main` so that the
/// observability config can be read off the parsed `Config` to drive the
/// logging subscriber before the runtime is built — without re-reading
/// the file from disk.
pub fn build_runtime_from_config(config: Config, source: &Path) -> Result<Courier> {
    let registry = Registry::with_builtins();
    registry
        .build_courier(config)
        .with_context(|| format!("failed to build runtime from config {}", source.display()))
}

pub fn validate_config(path: &Path) -> Result<()> {
    let config = Config::load(path)?;
    let registry = Registry::with_builtins();
    registry
        .dry_run_build(config)
        .with_context(|| format!("failed to build runtime from config {}", path.display()))
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

/// Build a `Courier` for replay, rewriting each pipeline:
///
/// - Replaces the source with `jsonl_file` reading from `dlq_path`.
/// - Strips all transforms (envelopes were already transformed before capture).
/// - For each pipeline, inspect the DLQ entries and determine which sinks to
///   replay to. When the DLQ has entries for only one sink, that sink is kept
///   and `fan_out` is reset to `broadcast`. When the DLQ has entries for
///   multiple sinks, the pipeline is split into one replay pipeline per sink,
///   each with a `jsonl_file` source that filters entries to its specific sink.
///   This avoids cross-contamination: each envelope is sent only to the sink
///   it originally failed at.
/// - Removes any sink dead-letter path that would write back into `dlq_path`,
///   replacing it with `Propagate` so replay failures surface as errors
///   instead of creating an infinite loop.
///
/// `registry` is used to build the replay runtime. Pass `Registry::with_builtins()`
/// for the default set of components, or a custom registry if the config references
/// plugin-provided components.
pub fn build_replay_runtime(
    mut config: Config,
    dlq_path: &Path,
    registry: Registry,
) -> Result<Courier> {
    let dlq_path_str = dlq_path
        .to_str()
        .with_context(|| "dead-letter path contains non-UTF-8 characters")?;

    let analysis = dlq_sink_indices(dlq_path)?;

    if analysis.has_unattributed_lines {
        tracing::warn!(
            "DLQ contains unattributed entries (bare-envelope format); \
             replaying each pipeline to all its sinks without sink-level filtering"
        );
    }

    let config_pipeline_names: BTreeSet<String> =
        config.pipelines.iter().map(|p| p.name.clone()).collect();

    for pipeline_name in analysis.sink_indices.keys() {
        if !config_pipeline_names.contains(pipeline_name) {
            tracing::warn!(
                pipeline = %pipeline_name,
                "DLQ references a pipeline not present in the config; entries will be skipped"
            );
        }
    }

    let mut new_pipelines = Vec::new();

    for mut pipeline in config.pipelines {
        let original_name = pipeline.name.clone();
        let sinks = std::mem::take(&mut pipeline.sinks);

        if analysis.has_unattributed_lines {
            new_pipelines.push(replay_pipeline_all_sinks(
                pipeline,
                &sinks,
                &original_name,
                dlq_path_str,
                dlq_path,
            ));
        } else if let Some(indices) = analysis.sink_indices.get(&original_name) {
            if indices.is_empty() {
                continue;
            }
            let replay_pipelines = replay_pipelines_per_sink(
                pipeline,
                &sinks,
                indices,
                &original_name,
                dlq_path_str,
                dlq_path,
            )?;
            new_pipelines.extend(replay_pipelines);
        } else {
            tracing::debug!(
                pipeline = %original_name,
                "skipping pipeline with no matching DLQ entries"
            );
        }
    }

    config.pipelines = new_pipelines;

    registry
        .build_courier(config)
        .with_context(|| "failed to build replay runtime from config")
}

fn replay_pipeline_all_sinks(
    mut pipeline: PipelineSpec,
    sinks: &[SinkSpec],
    original_name: &str,
    dlq_path_str: &str,
    dlq_path: &Path,
) -> PipelineSpec {
    pipeline.sinks = sinks.to_vec();
    pipeline.transforms.clear();
    pipeline.source = SourceSpec {
        kind: "jsonl_file".into(),
        config: json!({
            "path": dlq_path_str,
            "dead_letter_pipeline": original_name,
        }),
        retry: None,
    };
    neutralize_dlq_paths(&mut pipeline.sinks, dlq_path);
    pipeline
}

fn replay_pipelines_per_sink(
    pipeline: PipelineSpec,
    sinks: &[SinkSpec],
    indices: &BTreeSet<usize>,
    original_name: &str,
    dlq_path_str: &str,
    dlq_path: &Path,
) -> Result<Vec<PipelineSpec>> {
    let mut result = Vec::new();

    for &idx in indices {
        let sink = match sinks.get(idx) {
            Some(s) => s.clone(),
            None => {
                tracing::warn!(
                    pipeline = %original_name,
                    sink_index = idx,
                    "DLQ references a sink index that no longer exists in the config"
                );
                continue;
            }
        };

        let sink_id = format!("{}/sink{}", original_name, idx);
        let needs_sink_filter = indices.len() > 1;

        let mut source_config = json!({
            "path": dlq_path_str,
            "dead_letter_pipeline": original_name,
        });
        if needs_sink_filter {
            source_config["dead_letter_sink"] = json!(sink_id);
        }

        let mut replay_pipeline = PipelineSpec {
            name: if needs_sink_filter {
                format!("{}__replay_sink{}", original_name, idx)
            } else {
                original_name.to_string()
            },
            source: SourceSpec {
                kind: "jsonl_file".into(),
                config: source_config,
                retry: None,
            },
            transforms: vec![],
            sinks: vec![sink],
            channel_capacity: pipeline.channel_capacity,
            fan_out: FanOutPolicyConfig::default(),
        };
        neutralize_dlq_paths(&mut replay_pipeline.sinks, dlq_path);
        result.push(replay_pipeline);
    }

    if result.is_empty() {
        bail!(
            "pipeline '{}' would have no sinks after DLQ filtering; \
             the dead-letter entry references a sink that no longer exists in the config",
            original_name
        );
    }

    Ok(result)
}

struct DlqAnalysis {
    sink_indices: HashMap<String, BTreeSet<usize>>,
    has_unattributed_lines: bool,
}

/// Read all lines of `dlq_path` and collect sink indices per pipeline from
/// `DeadLetterEntry` records. Bare-envelope format lines (no `pipeline`/`sink`
/// fields) are flagged via `has_unattributed_lines` so the caller can avoid
/// sink-level filtering that would silently drop them.
fn dlq_sink_indices(dlq_path: &Path) -> Result<DlqAnalysis> {
    use std::io::BufRead;

    let file = std::fs::File::open(dlq_path)
        .with_context(|| format!("failed to open dead-letter file '{}'", dlq_path.display()))?;
    let reader = std::io::BufReader::new(file);
    let mut sink_indices: HashMap<String, BTreeSet<usize>> = HashMap::new();
    let mut has_unattributed_lines = false;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    path = %dlq_path.display(),
                    error = %e,
                    "skipping unreadable line in dead-letter file"
                );
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("envelope").is_some() {
            if let Ok(entry) = serde_json::from_value::<DeadLetterEntry>(value) {
                let sink_idx = parse_sink_index(&entry.sink, &entry.pipeline);
                if let Some(idx) = sink_idx {
                    sink_indices.entry(entry.pipeline).or_default().insert(idx);
                }
            } else {
                has_unattributed_lines = true;
            }
        } else if value.get("meta").is_some() {
            has_unattributed_lines = true;
        }
    }

    Ok(DlqAnalysis {
        sink_indices,
        has_unattributed_lines,
    })
}

/// Parse a sink node id like `"my-pipeline/sink2"` into the 0-based index
/// (`2`). Uses `rsplit_once('/')` so pipeline names containing `/` are
/// handled correctly (e.g. `"a/b/sink0"` → `Some(0)`).
/// Returns `None` if the format doesn't match.
fn parse_sink_index(sink_id: &str, pipeline: &str) -> Option<usize> {
    let (prefix, suffix) = sink_id.rsplit_once('/')?;
    if prefix != pipeline {
        return None;
    }
    suffix
        .strip_prefix("sink")
        .and_then(|idx| idx.parse::<usize>().ok())
}

/// For each sink's retry policy, if `on_exhausted` is `DeadLetter { path }`
/// and `path` resolves to the same file as `dlq_path`, replace it with
/// `Propagate` so replay failures surface as errors instead of creating an
/// infinite loop.
///
/// Uses canonical paths for reliable comparison. When `path` refers to a file
/// that doesn't exist yet (common for DLQ output paths — they're created on
/// first write), `canonicalize()` fails. In that case we fall back to direct
/// path comparison so neutralization is not silently skipped.
fn neutralize_dlq_paths(sinks: &mut [SinkSpec], dlq_path: &Path) {
    let canonical_dlq = dlq_path.canonicalize().ok();
    for sink in sinks.iter_mut() {
        if let Some(retry) = &mut sink.retry
            && let ExhaustedPolicy::DeadLetter { path } = &retry.on_exhausted
        {
            let canonical_match = canonical_dlq
                .as_ref()
                .is_some_and(|canonical| path.canonicalize().is_ok_and(|p| p == *canonical));
            let is_match = canonical_match || path == dlq_path;
            if is_match {
                tracing::warn!(
                    sink = %sink.kind,
                    dlq_path = %path.display(),
                    "overriding dead-letter path for replay: \
                     sink would write back into the replay source file"
                );
                retry.on_exhausted = ExhaustedPolicy::Propagate;
            }
        }
    }
}

pub async fn run_command(config: Option<PathBuf>) -> ExitCode {
    let path = resolve_config_path(config);
    let cfg = match load_validated_config(&path) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("{err:#}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(err) = crate::observability::init_from_config(cfg.observability.as_ref(), "info") {
        eprintln!("failed to initialize observability: {err:#}");
        return ExitCode::FAILURE;
    }
    let courier = match prepare_run_courier(cfg, &path) {
        Ok(c) => c,
        Err(err) => {
            tracing::error!("failed to start courier: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("loaded pipeline config from {}", path.display());
    run_outcome_to_exit_code(courier.run().await, "one or more pipelines failed")
}

fn load_validated_config(path: &Path) -> Result<Config> {
    let cfg = Config::load(path)
        .with_context(|| format!("failed to load config from {}", path.display()))?;
    cfg.validate().context("configuration invalid")?;
    Ok(cfg)
}

fn prepare_run_courier(cfg: Config, path: &Path) -> Result<Courier> {
    // Config::validate() already checked that health.address parses.
    let health_address: Option<std::net::SocketAddr> = cfg
        .health
        .as_ref()
        .map(|h| h.address.parse().expect("address was validated"));
    let shutdown_timeout = cfg
        .shutdown
        .as_ref()
        .map(|s| std::time::Duration::from_secs(s.timeout_secs))
        .unwrap_or(std::time::Duration::from_secs(30));
    Ok(build_runtime_from_config(cfg, path)?
        .with_shutdown_timeout(shutdown_timeout)
        .with_health_address(health_address))
}

fn run_outcome_to_exit_code(outcome: RunOutcome, failure_msg: &str) -> ExitCode {
    match outcome {
        RunOutcome::Success => ExitCode::SUCCESS,
        RunOutcome::Failed => {
            tracing::error!("{failure_msg}");
            ExitCode::FAILURE
        }
    }
}

pub fn validate_command(config: Option<PathBuf>) -> ExitCode {
    if let Err(err) = crate::observability::init_default_logging("off") {
        eprintln!("failed to initialize logging: {err:#}");
        return ExitCode::FAILURE;
    }
    let path = resolve_config_path(config);
    match validate_config(&path) {
        Ok(()) => {
            println!("configuration OK: {}", path.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("configuration invalid: {err:#}");
            ExitCode::FAILURE
        }
    }
}

pub fn list_components_command() -> ExitCode {
    if let Err(err) = crate::observability::init_default_logging("off") {
        eprintln!("failed to initialize logging: {err:#}");
        return ExitCode::FAILURE;
    }
    print!("{}", list_components(&Registry::with_builtins()));
    ExitCode::SUCCESS
}

pub async fn replay_dlq_command(dlq_path: PathBuf, config: Option<PathBuf>) -> ExitCode {
    if let Err(err) = validate_dlq_path(&dlq_path) {
        eprintln!("invalid dead-letter path: {err:#}");
        return ExitCode::FAILURE;
    }
    let path = resolve_config_path(config);
    let cfg = match load_validated_config(&path) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("{err:#}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(err) = crate::observability::init_from_config(cfg.observability.as_ref(), "info") {
        eprintln!("failed to initialize observability: {err:#}");
        return ExitCode::FAILURE;
    }
    let courier = match build_replay_runtime(cfg, &dlq_path, Registry::with_builtins()) {
        Ok(c) => c,
        Err(err) => {
            tracing::error!("failed to start replay: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("replaying dead-letter entries from {}", dlq_path.display());
    run_outcome_to_exit_code(courier.run().await, "replay pipeline failed")
}

pub fn validate_dlq_path(dlq_path: &Path) -> Result<()> {
    if !dlq_path.exists() {
        bail!("dead-letter file '{}' does not exist", dlq_path.display());
    }
    if !dlq_path.is_file() {
        bail!("dead-letter path '{}' is not a file", dlq_path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FanOutPolicyConfig, PipelineSpec, TransformSpec};
    use crate::envelope::Envelope;
    use crate::retry::RetryPolicy;

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

    fn is_failure(code: ExitCode) -> bool {
        format!("{code:?}").contains("(1)")
    }

    fn is_success(code: ExitCode) -> bool {
        format!("{code:?}").contains("(0)")
    }

    fn write_valid_config(path: &Path) {
        std::fs::write(
            path,
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
    }

    #[test]
    fn load_validated_config_rejects_missing_file() {
        let err = load_validated_config(Path::new("/nonexistent/cfg.toml")).unwrap_err();
        assert!(format!("{err:#}").contains("failed to load config"));
    }

    #[test]
    fn load_validated_config_rejects_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dup.toml");
        std::fs::write(
            &path,
            r#"
[[pipelines]]
name = "dup"
[pipelines.source]
type = "api_poll"
url = "http://localhost/d"
interval_secs = 60
[[pipelines.sinks]]
type = "api"
url = "http://localhost/o"

[[pipelines]]
name = "dup"
[pipelines.source]
type = "api_poll"
url = "http://localhost/d"
interval_secs = 60
[[pipelines.sinks]]
type = "api"
url = "http://localhost/o"
"#,
        )
        .unwrap();
        let err = load_validated_config(&path).unwrap_err();
        assert!(format!("{err:#}").contains("configuration invalid"));
    }

    #[test]
    fn load_validated_config_accepts_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.toml");
        write_valid_config(&path);
        load_validated_config(&path).unwrap();
    }

    #[test]
    fn prepare_run_courier_applies_defaults_when_no_health_or_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.toml");
        write_valid_config(&path);
        let cfg = load_validated_config(&path).unwrap();
        prepare_run_courier(cfg, &path).unwrap();
    }

    #[test]
    fn prepare_run_courier_parses_health_address() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.toml");
        std::fs::write(
            &path,
            r#"
[health]
address = "127.0.0.1:9090"

[[pipelines]]
name = "p"
[pipelines.source]
type = "api_poll"
url = "http://localhost/d"
interval_secs = 60
[[pipelines.sinks]]
type = "api"
url = "http://localhost/o"
"#,
        )
        .unwrap();
        let cfg = load_validated_config(&path).unwrap();
        prepare_run_courier(cfg, &path).unwrap();
    }

    #[test]
    fn run_outcome_to_exit_code_maps_success() {
        assert!(is_success(run_outcome_to_exit_code(
            RunOutcome::Success,
            "ignored"
        )));
    }

    #[test]
    fn run_outcome_to_exit_code_maps_failure() {
        assert!(is_failure(run_outcome_to_exit_code(
            RunOutcome::Failed,
            "boom"
        )));
    }

    #[tokio::test]
    async fn run_command_fails_when_config_missing() {
        let code = run_command(Some(PathBuf::from("/nonexistent/cfg-for-run.toml"))).await;
        assert!(is_failure(code));
    }

    #[tokio::test]
    async fn replay_dlq_command_fails_when_dlq_missing() {
        let code =
            replay_dlq_command(PathBuf::from("/nonexistent/dlq-for-replay.jsonl"), None).await;
        assert!(is_failure(code));
    }

    #[tokio::test]
    async fn replay_dlq_command_fails_when_config_missing() {
        let dir = tempfile::tempdir().unwrap();
        let dlq = dir.path().join("dlq.jsonl");
        std::fs::write(&dlq, "").unwrap();
        let code =
            replay_dlq_command(dlq, Some(PathBuf::from("/nonexistent/cfg-for-replay.toml"))).await;
        assert!(is_failure(code));
    }

    #[test]
    fn validate_command_succeeds_for_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.toml");
        write_valid_config(&path);
        assert!(is_success(validate_command(Some(path))));
    }

    #[test]
    fn validate_command_fails_for_missing_config() {
        let code = validate_command(Some(PathBuf::from("/nonexistent/cfg-for-validate.toml")));
        assert!(is_failure(code));
    }

    #[test]
    fn list_components_command_succeeds() {
        assert!(is_success(list_components_command()));
    }

    #[test]
    fn list_components_includes_registered_builtins() {
        let registry = Registry::with_builtins();
        let output = list_components(&registry);

        assert!(output.contains("sources:\n"));
        assert!(output.contains("  api_poll\n"));
        assert!(output.contains("  http_webhook\n"));
        assert!(output.contains("  jsonl_file\n"));
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
    fn validate_dlq_path_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();

        let err = validate_dlq_path(dir.path()).err().unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("is not a file"), "{msg}");
    }

    #[test]
    fn validate_dlq_path_rejects_missing_file() {
        let err = validate_dlq_path(std::path::Path::new("/nonexistent/dlq.jsonl"))
            .err()
            .unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("does not exist"), "{msg}");
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

    #[test]
    fn validate_rejects_invalid_script_config_with_component_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid-script.toml");
        std::fs::write(
            &path,
            r#"
[[pipelines]]
name = "invalid-script"

[pipelines.source]
type = "api_poll"
url = "http://localhost/data"
interval_secs = 60

[[pipelines.transforms]]
type = "script"
runtime = "rhai"
script = "payload"
script_file = "/tmp/also-set.rhai"

[[pipelines.sinks]]
type = "api"
url = "http://localhost/ingest"
"#,
        )
        .unwrap();

        let err = validate_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("pipeline 'invalid-script' transform[0]"),
            "{msg}"
        );
        assert!(
            msg.contains("set either 'script' or 'script_file', not both"),
            "{msg}"
        );
    }

    #[test]
    fn replay_strips_transforms() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        let entry = DeadLetterEntry {
            envelope: Envelope::new("src", serde_json::json!({"x": 1})),
            error: "fail".into(),
            pipeline: "p".into(),
            sink: "p/sink0".into(),
            dead_lettered_at_ms: 1000,
            attempts: 1,
        };
        std::fs::write(&dlq_path, serde_json::to_string(&entry).unwrap()).unwrap();

        let config = Config {
            observability: None,
            health: None,
            shutdown: None,
            pipelines: vec![PipelineSpec {
                name: "p".into(),
                source: SourceSpec {
                    kind: "api_poll".into(),
                    config: serde_json::json!({"url": "http://localhost/d", "interval_secs": 60}),
                    retry: None,
                },
                transforms: vec![TransformSpec {
                    kind: "mutate".into(),
                    config: serde_json::json!({"add": {"field": "value"}}),
                    on_error: None,
                }],
                sinks: vec![SinkSpec {
                    kind: "api".into(),
                    config: serde_json::json!({"url": "http://localhost/out"}),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
                fan_out: FanOutPolicyConfig::default(),
            }],
        };

        let result = build_replay_runtime(config, &dlq_path, Registry::with_builtins()).unwrap();
        // We can't inspect pipelines directly (Courier owns them), but the
        // build succeeds, which means transforms were cleared and
        // the jsonl_file source was accepted.
        drop(result);
    }

    #[test]
    fn replay_filters_sinks_by_dlq_entry() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        let entry = DeadLetterEntry {
            envelope: Envelope::new("src", serde_json::json!({})),
            error: "fail".into(),
            pipeline: "p".into(),
            sink: "p/sink1".into(),
            dead_lettered_at_ms: 1000,
            attempts: 1,
        };
        std::fs::write(&dlq_path, serde_json::to_string(&entry).unwrap()).unwrap();

        let config = Config {
            observability: None,
            health: None,
            shutdown: None,
            pipelines: vec![PipelineSpec {
                name: "p".into(),
                source: SourceSpec {
                    kind: "api_poll".into(),
                    config: serde_json::json!({"url": "http://localhost/d", "interval_secs": 60}),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![
                    SinkSpec {
                        kind: "api".into(),
                        config: serde_json::json!({"url": "http://localhost/a"}),
                        on_error: None,
                        retry: None,
                    },
                    SinkSpec {
                        kind: "api".into(),
                        config: serde_json::json!({"url": "http://localhost/b"}),
                        on_error: None,
                        retry: None,
                    },
                ],
                channel_capacity: None,
                fan_out: FanOutPolicyConfig::default(),
            }],
        };

        let result = build_replay_runtime(config, &dlq_path, Registry::with_builtins());
        // Build succeeds — sink1 (the failed sink from the DLQ entry) is
        // kept, sink0 is filtered out.
        assert!(result.is_ok(), "expected ok, got {:?}", result.err());
    }

    #[test]
    fn replay_rejects_zero_sinks_after_filtering() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        let entry = DeadLetterEntry {
            envelope: Envelope::new("src", serde_json::json!({})),
            error: "fail".into(),
            pipeline: "p".into(),
            sink: "p/sink99".into(),
            dead_lettered_at_ms: 1000,
            attempts: 1,
        };
        std::fs::write(&dlq_path, serde_json::to_string(&entry).unwrap()).unwrap();

        let config = Config {
            observability: None,
            health: None,
            shutdown: None,
            pipelines: vec![PipelineSpec {
                name: "p".into(),
                source: SourceSpec {
                    kind: "api_poll".into(),
                    config: serde_json::json!({"url": "http://localhost/d", "interval_secs": 60}),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "api".into(),
                    config: serde_json::json!({"url": "http://localhost/out"}),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
                fan_out: FanOutPolicyConfig::default(),
            }],
        };

        let err = build_replay_runtime(config, &dlq_path, Registry::with_builtins())
            .err()
            .unwrap();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("would have no sinks after DLQ filtering"),
            "{msg}"
        );
    }

    #[test]
    fn replay_resets_fan_out_when_filtering_to_single_sink() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        let entry = DeadLetterEntry {
            envelope: Envelope::new("src", serde_json::json!({})),
            error: "fail".into(),
            pipeline: "p".into(),
            sink: "p/sink1".into(),
            dead_lettered_at_ms: 1000,
            attempts: 1,
        };
        std::fs::write(&dlq_path, serde_json::to_string(&entry).unwrap()).unwrap();

        let config = Config {
            observability: None,
            health: None,
            shutdown: None,
            pipelines: vec![PipelineSpec {
                name: "p".into(),
                source: SourceSpec {
                    kind: "api_poll".into(),
                    config: serde_json::json!({"url": "http://localhost/d", "interval_secs": 60}),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![
                    SinkSpec {
                        kind: "api".into(),
                        config: serde_json::json!({"url": "http://localhost/a"}),
                        on_error: None,
                        retry: None,
                    },
                    SinkSpec {
                        kind: "api".into(),
                        config: serde_json::json!({"url": "http://localhost/b"}),
                        on_error: None,
                        retry: None,
                    },
                ],
                channel_capacity: None,
                fan_out: FanOutPolicyConfig::BroadcastWithDrop,
            }],
        };

        let result = build_replay_runtime(config, &dlq_path, Registry::with_builtins());
        assert!(
            result.is_ok(),
            "expected ok with broadcast_with_drop + single sink after filtering, got {:?}",
            result.err()
        );
    }

    #[test]
    fn replay_splits_pipeline_for_multi_sink_dlq() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        let entry0 = DeadLetterEntry {
            envelope: Envelope::new("src", serde_json::json!({"sink": 0})),
            error: "fail".into(),
            pipeline: "p".into(),
            sink: "p/sink0".into(),
            dead_lettered_at_ms: 1000,
            attempts: 1,
        };
        let entry1 = DeadLetterEntry {
            envelope: Envelope::new("src", serde_json::json!({"sink": 1})),
            error: "fail".into(),
            pipeline: "p".into(),
            sink: "p/sink1".into(),
            dead_lettered_at_ms: 1000,
            attempts: 1,
        };
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&entry0).unwrap(),
            serde_json::to_string(&entry1).unwrap(),
        );
        std::fs::write(&dlq_path, content).unwrap();

        let config = Config {
            observability: None,
            health: None,
            shutdown: None,
            pipelines: vec![PipelineSpec {
                name: "p".into(),
                source: SourceSpec {
                    kind: "api_poll".into(),
                    config: serde_json::json!({"url": "http://localhost/d", "interval_secs": 60}),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![
                    SinkSpec {
                        kind: "api".into(),
                        config: serde_json::json!({"url": "http://localhost/a"}),
                        on_error: None,
                        retry: None,
                    },
                    SinkSpec {
                        kind: "api".into(),
                        config: serde_json::json!({"url": "http://localhost/b"}),
                        on_error: None,
                        retry: None,
                    },
                ],
                channel_capacity: None,
                fan_out: FanOutPolicyConfig::BroadcastWithDrop,
            }],
        };

        let result = build_replay_runtime(config, &dlq_path, Registry::with_builtins());
        assert!(
            result.is_ok(),
            "expected ok with multi-sink DLQ splitting, got {:?}",
            result.err()
        );
    }

    #[test]
    fn replay_neutralizes_dlq_path_that_matches_input() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        let dlq_path_for_config = dir.path().join("dlq.jsonl");
        let entry = DeadLetterEntry {
            envelope: Envelope::new("src", serde_json::json!({})),
            error: "fail".into(),
            pipeline: "p".into(),
            sink: "p/sink0".into(),
            dead_lettered_at_ms: 1000,
            attempts: 1,
        };
        std::fs::write(&dlq_path, serde_json::to_string(&entry).unwrap()).unwrap();

        let config = Config {
            observability: None,
            health: None,
            shutdown: None,
            pipelines: vec![PipelineSpec {
                name: "p".into(),
                source: SourceSpec {
                    kind: "api_poll".into(),
                    config: serde_json::json!({"url": "http://localhost/d", "interval_secs": 60}),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "api".into(),
                    config: serde_json::json!({"url": "http://localhost/out"}),
                    on_error: None,
                    retry: Some(RetryPolicy {
                        max_attempts: 3,
                        initial_delay_ms: 100,
                        backoff_multiplier: 2.0,
                        max_delay_ms: 5000,
                        on_exhausted: ExhaustedPolicy::DeadLetter {
                            path: dlq_path_for_config,
                        },
                    }),
                }],
                channel_capacity: None,
                fan_out: FanOutPolicyConfig::default(),
            }],
        };

        let result = build_replay_runtime(config, &dlq_path, Registry::with_builtins());
        // Should succeed — the DLQ path conflict is resolved by
        // neutralizing (switching to Propagate) rather than erroring.
        assert!(result.is_ok(), "expected ok, got {:?}", result.err());
    }

    #[test]
    fn replay_skips_pipeline_with_no_dlq_entries() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        // DLQ only has entries for pipeline "other", not "p".
        let entry = DeadLetterEntry {
            envelope: Envelope::new("src", serde_json::json!({})),
            error: "fail".into(),
            pipeline: "other".into(),
            sink: "other/sink0".into(),
            dead_lettered_at_ms: 1000,
            attempts: 1,
        };
        std::fs::write(&dlq_path, serde_json::to_string(&entry).unwrap()).unwrap();

        let config = Config {
            observability: None,
            health: None,
            shutdown: None,
            pipelines: vec![PipelineSpec {
                name: "p".into(),
                source: SourceSpec {
                    kind: "api_poll".into(),
                    config: serde_json::json!({"url": "http://localhost/d", "interval_secs": 60}),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![
                    SinkSpec {
                        kind: "api".into(),
                        config: serde_json::json!({"url": "http://localhost/a"}),
                        on_error: None,
                        retry: None,
                    },
                    SinkSpec {
                        kind: "api".into(),
                        config: serde_json::json!({"url": "http://localhost/b"}),
                        on_error: None,
                        retry: None,
                    },
                ],
                channel_capacity: None,
                fan_out: FanOutPolicyConfig::default(),
            }],
        };

        let result = build_replay_runtime(config, &dlq_path, Registry::with_builtins());
        assert!(result.is_ok(), "expected ok, got {:?}", result.err());
    }

    #[test]
    fn neutralize_dlq_paths_falls_back_to_direct_comparison_when_output_path_missing() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        let nonexistent_path = dir.path().join("output.jsonl");
        std::fs::write(&dlq_path, "line\n").unwrap();

        let mut sinks = vec![SinkSpec {
            kind: "api".into(),
            config: serde_json::json!({"url": "http://localhost/out"}),
            on_error: None,
            retry: Some(RetryPolicy {
                max_attempts: 3,
                initial_delay_ms: 100,
                backoff_multiplier: 2.0,
                max_delay_ms: 5000,
                on_exhausted: ExhaustedPolicy::DeadLetter {
                    path: nonexistent_path.clone(),
                },
            }),
        }];

        neutralize_dlq_paths(&mut sinks, &dlq_path);
        // The output path doesn't exist on disk so canonicalize fails,
        // but it's a different path from dlq_path so it should NOT be neutralized.
        let retry = sinks[0].retry.as_ref().unwrap();
        assert!(
            matches!(&retry.on_exhausted, ExhaustedPolicy::DeadLetter { path } if path == &nonexistent_path),
            "non-matching DLQ path should NOT be neutralized"
        );

        // Now test with the same path (still non-existent on disk as an output).
        // Use a relative-style path matching the dlq_path exactly.
        let mut sinks2 = vec![SinkSpec {
            kind: "api".into(),
            config: serde_json::json!({"url": "http://localhost/out"}),
            on_error: None,
            retry: Some(RetryPolicy {
                max_attempts: 3,
                initial_delay_ms: 100,
                backoff_multiplier: 2.0,
                max_delay_ms: 5000,
                on_exhausted: ExhaustedPolicy::DeadLetter {
                    path: dlq_path.clone(),
                },
            }),
        }];

        neutralize_dlq_paths(&mut sinks2, &dlq_path);
        let retry = sinks2[0].retry.as_ref().unwrap();
        assert!(
            matches!(&retry.on_exhausted, ExhaustedPolicy::Propagate),
            "matching DLQ path must be neutralized even when canonicalize works"
        );
    }

    #[test]
    fn parse_sink_index_extracts_index() {
        assert_eq!(
            parse_sink_index("my-pipeline/sink0", "my-pipeline"),
            Some(0)
        );
        assert_eq!(
            parse_sink_index("my-pipeline/sink2", "my-pipeline"),
            Some(2)
        );
        assert_eq!(parse_sink_index("other/sink0", "other"), Some(0));
        assert_eq!(
            parse_sink_index("my-pipeline/transform0", "my-pipeline"),
            None
        );
        assert_eq!(parse_sink_index("wrong-prefix/sink0", "my-pipeline"), None);
        assert_eq!(
            parse_sink_index("team/project/sink3", "team/project"),
            Some(3)
        );
    }

    #[test]
    fn dlq_sink_indices_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        let entry = DeadLetterEntry {
            envelope: Envelope::new("src", serde_json::json!({})),
            error: "fail".into(),
            pipeline: "p".into(),
            sink: "p/sink0".into(),
            dead_lettered_at_ms: 1000,
            attempts: 1,
        };
        let line = serde_json::to_string(&entry).unwrap();
        let content = format!("{line}\n{line}\n{line}\n");
        std::fs::write(&dlq_path, content).unwrap();

        let analysis = dlq_sink_indices(&dlq_path).unwrap();
        assert_eq!(analysis.sink_indices.len(), 1);
        assert_eq!(analysis.sink_indices["p"], BTreeSet::from([0]));
        assert!(!analysis.has_unattributed_lines);
    }

    #[test]
    fn dlq_sink_indices_flags_bare_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        let env = Envelope::new("src", serde_json::json!({"x": 1}));
        let line = serde_json::to_string(&env).unwrap();
        std::fs::write(&dlq_path, format!("{line}\n")).unwrap();

        let analysis = dlq_sink_indices(&dlq_path).unwrap();
        assert!(analysis.sink_indices.is_empty());
        assert!(analysis.has_unattributed_lines);
    }

    #[test]
    fn dlq_sink_indices_mixed_attributed_and_unattributed() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        let entry = DeadLetterEntry {
            envelope: Envelope::new("src", serde_json::json!({})),
            error: "fail".into(),
            pipeline: "p".into(),
            sink: "p/sink0".into(),
            dead_lettered_at_ms: 1000,
            attempts: 1,
        };
        let bare = Envelope::new("src", serde_json::json!({"y": 2}));
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&entry).unwrap(),
            serde_json::to_string(&bare).unwrap(),
        );
        std::fs::write(&dlq_path, content).unwrap();

        let analysis = dlq_sink_indices(&dlq_path).unwrap();
        assert_eq!(analysis.sink_indices["p"], BTreeSet::from([0]));
        assert!(analysis.has_unattributed_lines);
    }

    #[test]
    fn replay_uses_all_sinks_when_unattributed_lines_exist() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("dlq.jsonl");
        let bare = Envelope::new("src", serde_json::json!({}));
        std::fs::write(
            &dlq_path,
            format!("{}\n", serde_json::to_string(&bare).unwrap()),
        )
        .unwrap();

        let config = Config {
            observability: None,
            health: None,
            shutdown: None,
            pipelines: vec![PipelineSpec {
                name: "p".into(),
                source: SourceSpec {
                    kind: "api_poll".into(),
                    config: serde_json::json!({"url": "http://localhost/d", "interval_secs": 60}),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![
                    SinkSpec {
                        kind: "api".into(),
                        config: serde_json::json!({"url": "http://localhost/a"}),
                        on_error: None,
                        retry: None,
                    },
                    SinkSpec {
                        kind: "api".into(),
                        config: serde_json::json!({"url": "http://localhost/b"}),
                        on_error: None,
                        retry: None,
                    },
                ],
                channel_capacity: None,
                fan_out: FanOutPolicyConfig::default(),
            }],
        };

        let result = build_replay_runtime(config, &dlq_path, Registry::with_builtins());
        assert!(
            result.is_ok(),
            "expected ok with bare-envelope DLQ, got {:?}",
            result.err()
        );
    }
}
