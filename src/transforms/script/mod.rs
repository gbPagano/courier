use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::config::parse_config;
use crate::envelope::Envelope;
use crate::pipeline::ErrorPolicy;
use crate::transforms::{BasicTransform, MapOne, Transform};

pub mod rhai;

trait ScriptEngine: Send + Sync {
    fn run(&self, env: Envelope) -> Result<Option<Envelope>>;
}

struct ScriptMapOne<E: ScriptEngine> {
    id: String,
    engine: E,
}

impl<E: ScriptEngine> ScriptMapOne<E> {
    fn new(id: impl Into<String>, engine: E) -> Self {
        Self {
            id: id.into(),
            engine,
        }
    }
}

#[async_trait]
impl<E: ScriptEngine> MapOne for ScriptMapOne<E> {
    fn id(&self) -> &str {
        &self.id
    }

    async fn map(&self, env: Envelope) -> Result<Option<Envelope>> {
        self.engine.run(env)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScriptRuntime {
    Rhai,
}

#[derive(Debug, Clone, Deserialize)]
struct RawScriptTransformConfig {
    runtime: ScriptRuntime,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    script_file: Option<PathBuf>,
    #[serde(default = "default_entrypoint")]
    entrypoint: String,
    #[serde(default = "default_max_operations")]
    max_operations: u64,
    #[serde(default = "default_max_call_levels")]
    max_call_levels: usize,
    #[serde(default = "default_max_expr_depth")]
    max_expr_depth: usize,
    #[serde(default = "default_max_function_expr_depth")]
    max_function_expr_depth: usize,
    #[serde(default = "default_max_variables")]
    max_variables: usize,
}

#[derive(Debug, Clone)]
struct ScriptTransformConfig {
    runtime: ScriptRuntime,
    script: String,
    entrypoint: String,
    max_operations: u64,
    max_call_levels: usize,
    max_expr_depth: usize,
    max_function_expr_depth: usize,
    max_variables: usize,
}

impl RawScriptTransformConfig {
    fn resolve(self) -> Result<ScriptTransformConfig> {
        let script = match (self.script, self.script_file) {
            (Some(_), Some(_)) => {
                bail!("script transform: set either 'script' or 'script_file', not both")
            }
            (None, None) => {
                bail!("script transform: one of 'script' or 'script_file' is required")
            }
            (Some(script), None) => script,
            (None, Some(path)) => std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read script_file '{}'", path.display()))?,
        };
        Ok(ScriptTransformConfig {
            runtime: self.runtime,
            script,
            entrypoint: self.entrypoint,
            max_operations: self.max_operations,
            max_call_levels: self.max_call_levels,
            max_expr_depth: self.max_expr_depth,
            max_function_expr_depth: self.max_function_expr_depth,
            max_variables: self.max_variables,
        })
    }
}

fn default_entrypoint() -> String {
    "transform".into()
}

fn default_max_operations() -> u64 {
    100_000
}

fn default_max_call_levels() -> usize {
    32
}

fn default_max_expr_depth() -> usize {
    64
}

fn default_max_function_expr_depth() -> usize {
    32
}

fn default_max_variables() -> usize {
    64
}

/// Registry factory for script-backed transforms. Registered by
/// `courier::registry::register_builtin` under kind `"script"`.
pub fn script_transform_factory(
    id: &str,
    config: Value,
    on_error: ErrorPolicy,
) -> Result<Box<dyn Transform>> {
    let config: RawScriptTransformConfig = parse_config("script", config)?;
    let config = config.resolve()?;

    let transform: Box<dyn Transform> = match config.runtime {
        ScriptRuntime::Rhai => Box::new(
            BasicTransform::new(ScriptMapOne::new(id, rhai::RhaiEngine::new(&config)?))
                .with_error_policy(on_error),
        ),
    };

    Ok(transform)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Registry;
    use crate::config::{ErrorPolicyConfig, TransformSpec};

    #[test]
    fn factory_resolves_through_registry() {
        let registry = Registry::with_builtins().unwrap();
        registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "rhai",
                        "script": "fn transform(env) { env }",
                    }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                },
            )
            .unwrap();
    }

    #[test]
    fn factory_loads_script_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transform.rhai");
        std::fs::write(&path, "fn transform(env) { env }").unwrap();

        let registry = Registry::with_builtins().unwrap();
        registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "rhai",
                        "script_file": path,
                    }),
                    on_error: None,
                },
            )
            .unwrap();
    }

    #[test]
    fn factory_rejects_missing_script_source() {
        let registry = Registry::with_builtins().unwrap();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({ "runtime": "rhai" }),
                    on_error: None,
                },
            )
            .err()
            .expect("expected factory error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("one of 'script' or 'script_file' is required"),
            "{msg}"
        );
    }

    #[test]
    fn factory_rejects_both_script_and_script_file() {
        let registry = Registry::with_builtins().unwrap();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "rhai",
                        "script": "fn transform(env) { env }",
                        "script_file": "/tmp/does-not-matter.rhai",
                    }),
                    on_error: None,
                },
            )
            .err()
            .expect("expected factory error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("set either 'script' or 'script_file', not both"),
            "{msg}"
        );
    }

    #[test]
    fn factory_reports_missing_script_file() {
        let registry = Registry::with_builtins().unwrap();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "rhai",
                        "script_file": "/nonexistent/script.rhai",
                    }),
                    on_error: None,
                },
            )
            .err()
            .expect("expected factory error");
        let msg = format!("{err:#}");
        assert!(msg.contains("/nonexistent/script.rhai"), "{msg}");
    }

    #[test]
    fn factory_reports_invalid_runtime() {
        let registry = Registry::with_builtins().unwrap();
        let result = registry.build_transform(
            "p/t0",
            TransformSpec {
                kind: "script".into(),
                config: json!({
                    "runtime": "lua",
                    "script": "fn transform(env) { env }",
                }),
                on_error: None,
            },
        );
        let err = result.err().expect("expected invalid runtime error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid config for component type 'script'"),
            "{msg}"
        );
    }
}
