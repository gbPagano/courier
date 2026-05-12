use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::config::{parse_config, redact_secret_path};
use crate::envelope::Envelope;
use crate::observability::NodeCtx;
use crate::pipeline::ErrorPolicy;
use crate::transforms::{BasicTransform, MapOne, Transform};

pub mod lua;
pub mod python;
pub mod rhai;

/// Marker error for "script exceeded its configured per-envelope timeout".
/// Engines return this so the transform loop's `record_failed` and the
/// engine-level `script_timeouts_total` counter both fire, and an operator
/// log can distinguish a timeout from a generic runtime failure.
#[derive(Debug, thiserror::Error)]
#[error("script transform exceeded timeout of {timeout_ms} ms")]
pub struct ScriptTimeoutError {
    pub timeout_ms: u64,
}

#[async_trait]
trait ScriptEngine: Send + Sync {
    async fn run(&self, env: Envelope) -> Result<Option<Envelope>>;

    /// Per-engine hook so engines can pre-build their own metric recorders.
    /// Default no-op for engines that don't record runtime-tagged metrics.
    fn set_node_ctx(&mut self, _ctx: NodeCtx) {}
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
        self.engine.run(env).await
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        self.engine.set_node_ctx(ctx);
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScriptRuntime {
    Rhai,
    Lua,
    Python,
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
    #[serde(default)]
    python_bin: Option<String>,
    /// Per-envelope timeout, in milliseconds. Applies to every script
    /// runtime. `None` (default) disables the timeout — backwards-
    /// compatible with the existing Rhai/Lua/Python configs.
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Maximum script operations (Rhai) or VM instructions (Lua).
    /// Rejected for `runtime = "python"`.
    #[serde(default)]
    max_operations: Option<u64>,
    #[serde(default)]
    max_call_levels: Option<usize>,
    #[serde(default)]
    max_expr_depth: Option<usize>,
    #[serde(default)]
    max_function_expr_depth: Option<usize>,
    #[serde(default)]
    max_variables: Option<usize>,
}

#[derive(Debug, Clone)]
struct PythonConfig {
    bin: String,
}

#[derive(Debug, Clone)]
struct LuaConfig {
    /// When `Some`, install an `mlua` hook that fires every N instructions
    /// and aborts the script if the cumulative count exceeds this budget
    /// (or if the timeout watchdog signals cancellation).
    max_operations: Option<u64>,
}

fn default_python_bin() -> String {
    "python3".into()
}

fn resolve_python_bin(bin: Option<String>) -> String {
    match bin {
        Some(bin) if !bin.trim().is_empty() => bin,
        _ => default_python_bin(),
    }
}

#[derive(Debug, Clone)]
struct RhaiConfig {
    max_operations: u64,
    max_call_levels: usize,
    max_expr_depth: usize,
    max_function_expr_depth: usize,
    max_variables: usize,
}

#[derive(Debug, Clone)]
struct ScriptTransformConfig {
    runtime: ScriptRuntime,
    script: String,
    entrypoint: String,
    /// Wall-clock budget for a single envelope. Enforced by each engine.
    /// `None` = disabled.
    timeout: Option<Duration>,
    python: Option<PythonConfig>,
    rhai: Option<RhaiConfig>,
    lua: Option<LuaConfig>,
}

impl RawScriptTransformConfig {
    fn resolve(self) -> Result<ScriptTransformConfig> {
        let RawScriptTransformConfig {
            runtime,
            script,
            script_file,
            entrypoint,
            python_bin,
            timeout_ms,
            max_operations,
            max_call_levels,
            max_expr_depth,
            max_function_expr_depth,
            max_variables,
        } = self;

        let script = match (script, script_file) {
            (Some(_), Some(_)) => {
                bail!("script transform: set either 'script' or 'script_file', not both")
            }
            (None, None) => {
                bail!("script transform: one of 'script' or 'script_file' is required")
            }
            (Some(script), None) => script,
            (None, Some(path)) => std::fs::read_to_string(&path).with_context(|| {
                format!("failed to read script_file '{}'", redact_secret_path(&path))
            })?,
        };

        let timeout = match timeout_ms {
            Some(0) => bail!("script transform: 'timeout_ms' must be > 0 when set"),
            Some(ms) => Some(Duration::from_millis(ms)),
            None => None,
        };

        // Knobs other than `max_operations` are Rhai-only — they map
        // to depth and variable budgets in the Rhai engine and have
        // no analogue in Lua or the Python worker.
        let has_rhai_only_limits = max_call_levels.is_some()
            || max_expr_depth.is_some()
            || max_function_expr_depth.is_some()
            || max_variables.is_some();

        let (python, lua, rhai) = match runtime {
            ScriptRuntime::Python => {
                if has_rhai_only_limits || max_operations.is_some() {
                    bail!(
                        "script transform: limits (max_operations, max_call_levels, max_expr_depth, max_function_expr_depth, max_variables) are not supported for runtime 'python'"
                    );
                }
                (
                    Some(PythonConfig {
                        bin: resolve_python_bin(python_bin),
                    }),
                    None,
                    None,
                )
            }
            ScriptRuntime::Lua => {
                if has_rhai_only_limits {
                    bail!(
                        "script transform: Rhai-only limits (max_call_levels, max_expr_depth, max_function_expr_depth, max_variables) are not supported for runtime 'lua'"
                    );
                }
                if python_bin.is_some() {
                    bail!("script transform: 'python_bin' is only supported for runtime 'python'");
                }
                if let Some(0) = max_operations {
                    bail!("script transform: 'max_operations' must be > 0 when set");
                }
                (None, Some(LuaConfig { max_operations }), None)
            }
            ScriptRuntime::Rhai => {
                if python_bin.is_some() {
                    bail!("script transform: 'python_bin' is only supported for runtime 'python'");
                }
                (
                    None,
                    None,
                    Some(RhaiConfig {
                        max_operations: max_operations.unwrap_or_else(default_max_operations),
                        max_call_levels: max_call_levels.unwrap_or_else(default_max_call_levels),
                        max_expr_depth: max_expr_depth.unwrap_or_else(default_max_expr_depth),
                        max_function_expr_depth: max_function_expr_depth
                            .unwrap_or_else(default_max_function_expr_depth),
                        max_variables: max_variables.unwrap_or_else(default_max_variables),
                    }),
                )
            }
        };

        Ok(ScriptTransformConfig {
            runtime,
            script,
            entrypoint,
            timeout,
            python,
            rhai,
            lua,
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
        ScriptRuntime::Lua => Box::new(
            BasicTransform::new(ScriptMapOne::new(id, lua::LuaEngine::new(&config)?))
                .with_error_policy(on_error),
        ),
        ScriptRuntime::Python => Box::new(
            BasicTransform::new(ScriptMapOne::new(id, python::PythonEngine::new(&config)?))
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
    fn factory_loads_lua_script_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transform.lua");
        std::fs::write(&path, "function transform(env) return env end").unwrap();

        let registry = Registry::with_builtins().unwrap();
        registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "lua",
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
    fn factory_resolves_lua_through_registry() {
        let registry = Registry::with_builtins().unwrap();
        registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "lua",
                        "script": "function transform(env) return env end",
                    }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                },
            )
            .unwrap();
    }

    #[test]
    fn factory_requires_runtime() {
        let registry = Registry::with_builtins().unwrap();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "script": "fn transform(env) { env }",
                    }),
                    on_error: None,
                },
            )
            .err()
            .expect("expected missing runtime error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid config for component type 'script'"),
            "{msg}"
        );
        assert!(msg.contains("runtime"), "{msg}");
    }

    #[test]
    fn factory_accepts_max_operations_for_lua() {
        // max_operations is shared with Lua (used as an instruction
        // budget via the mlua hook). The other Rhai knobs remain
        // Rhai-only — see `factory_rejects_rhai_only_limits_for_lua`.
        let registry = Registry::with_builtins().unwrap();
        registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "lua",
                        "script": "function transform(env) return env end",
                        "max_operations": 100_000,
                    }),
                    on_error: None,
                },
            )
            .unwrap();
    }

    #[test]
    fn factory_rejects_rhai_only_limits_for_lua() {
        let registry = Registry::with_builtins().unwrap();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "lua",
                        "script": "function transform(env) return env end",
                        "max_call_levels": 1,
                    }),
                    on_error: None,
                },
            )
            .err()
            .expect("expected Lua Rhai-only limit validation error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Rhai-only limits") && msg.contains("runtime 'lua'"),
            "{msg}"
        );
    }

    #[test]
    fn factory_resolves_python_through_registry() {
        let registry = Registry::with_builtins().unwrap();
        registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "python",
                        "script": "def transform(env):\n    return env\n",
                    }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                },
            )
            .unwrap();
    }

    #[test]
    fn factory_loads_python_script_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transform.py");
        std::fs::write(&path, "def transform(env):\n    return env\n").unwrap();

        let registry = Registry::with_builtins().unwrap();
        registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "python",
                        "script_file": path,
                    }),
                    on_error: None,
                },
            )
            .unwrap();
    }

    #[test]
    fn factory_rejects_limits_for_python() {
        let registry = Registry::with_builtins().unwrap();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "python",
                        "script": "def transform(env):\n    return env\n",
                        "max_variables": 1,
                    }),
                    on_error: None,
                },
            )
            .err()
            .expect("expected Python limit validation error");
        let msg = format!("{err:#}");
        assert!(msg.contains("not supported for runtime 'python'"), "{msg}");
    }

    #[test]
    fn factory_rejects_python_bin_for_non_python_runtime() {
        let registry = Registry::with_builtins().unwrap();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "lua",
                        "script": "function transform(env) return env end",
                        "python_bin": "python3",
                    }),
                    on_error: None,
                },
            )
            .err()
            .expect("expected python_bin validation error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("'python_bin' is only supported for runtime 'python'"),
            "{msg}"
        );
    }

    #[test]
    fn factory_rejects_zero_timeout_ms() {
        let registry = Registry::with_builtins().unwrap();
        for runtime in ["rhai", "lua", "python"] {
            let script = if runtime == "python" {
                "def transform(env):\n    return env\n"
            } else if runtime == "lua" {
                "function transform(env) return env end"
            } else {
                "fn transform(env) { env }"
            };
            let err = registry
                .build_transform(
                    "p/t0",
                    TransformSpec {
                        kind: "script".into(),
                        config: json!({
                            "runtime": runtime,
                            "script": script,
                            "timeout_ms": 0,
                        }),
                        on_error: None,
                    },
                )
                .err()
                .unwrap_or_else(|| {
                    panic!("expected timeout_ms=0 rejection for runtime '{runtime}'")
                });
            let msg = format!("{err:#}");
            assert!(
                msg.contains("'timeout_ms' must be > 0"),
                "runtime '{runtime}': {msg}"
            );
        }
    }

    #[test]
    fn factory_reports_invalid_runtime() {
        let registry = Registry::with_builtins().unwrap();
        let result = registry.build_transform(
            "p/t0",
            TransformSpec {
                kind: "script".into(),
                config: json!({
                    "runtime": "bogus",
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
