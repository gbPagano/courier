use anyhow::Result;
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
struct ScriptTransformConfig {
    runtime: ScriptRuntime,
    script: String,
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
    let config: ScriptTransformConfig = parse_config("script", config)?;

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
