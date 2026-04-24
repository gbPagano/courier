use anyhow::{Context, Result, anyhow, bail};
use rhai::serde::{from_dynamic, to_dynamic};
use rhai::{AST, Dynamic, Engine, Scope};

use crate::envelope::Envelope;

use super::{ScriptEngine, ScriptTransformConfig};

pub struct RhaiEngine {
    engine: Engine,
    ast: AST,
    entrypoint: String,
}

impl ScriptEngine for RhaiEngine {
    fn run(&self, env: Envelope) -> Result<Option<Envelope>> {
        self.run_inner(env)
    }
}

impl RhaiEngine {
    pub(super) fn new(config: &ScriptTransformConfig) -> Result<Self> {
        let limits = config
            .rhai
            .as_ref()
            .expect("Rhai config missing for Rhai runtime");

        let mut engine = Engine::new();
        engine
            .set_max_operations(limits.max_operations)
            .set_max_call_levels(limits.max_call_levels)
            .set_max_expr_depths(limits.max_expr_depth, limits.max_function_expr_depth)
            .set_max_variables(limits.max_variables);

        let ast = engine
            .compile(&config.script)
            .context("failed to compile Rhai script")?;

        let mut entrypoint_ast = ast.clone();
        entrypoint_ast
            .retain_functions(|_, _, name, params| name == config.entrypoint && params == 1);
        let has_entrypoint = entrypoint_ast.has_functions();
        if !has_entrypoint {
            bail!(
                "missing Rhai entrypoint '{}' with exactly one parameter",
                config.entrypoint
            );
        }

        Ok(Self {
            engine,
            ast,
            entrypoint: config.entrypoint.clone(),
        })
    }

    fn run_inner(&self, env: Envelope) -> Result<Option<Envelope>> {
        let arg = to_dynamic(env).context("failed to convert envelope into Rhai value")?;
        let mut scope = Scope::new();
        let out: Dynamic = self
            .engine
            .call_fn(&mut scope, &self.ast, &self.entrypoint, (arg,))
            .with_context(|| format!("Rhai entrypoint '{}' failed", self.entrypoint))?;

        if out.is_unit() {
            return Ok(None);
        }

        from_dynamic(&out.flatten()).map(Some).map_err(|err| {
            anyhow!(err).context("failed to convert Rhai return value into envelope")
        })
    }

    #[cfg(test)]
    fn run(&self, env: Envelope) -> Result<Option<Envelope>> {
        self.run_inner(env)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::RhaiEngine;
    use crate::envelope::Envelope;
    use crate::transforms::script::ScriptTransformConfig;

    fn config(script: &str) -> ScriptTransformConfig {
        ScriptTransformConfig {
            runtime: super::super::ScriptRuntime::Rhai,
            script: script.into(),
            entrypoint: "transform".into(),
            python: None,
            rhai: Some(super::super::RhaiConfig {
                max_operations: 100_000,
                max_call_levels: 32,
                max_expr_depth: 64,
                max_function_expr_depth: 32,
                max_variables: 64,
            }),
        }
    }

    #[test]
    fn mutates_payload() {
        let engine = RhaiEngine::new(&config(
            r#"
                fn transform(env) {
                    env.payload["processed"] = true;
                    env
                }
            "#,
        ))
        .unwrap();

        let out = engine
            .run(Envelope::new("src", json!({ "value": 1 })))
            .unwrap()
            .unwrap();
        assert_eq!(out.payload, json!({ "value": 1, "processed": true }));
    }

    #[test]
    fn mutates_metadata() {
        let engine = RhaiEngine::new(&config(
            r#"
                fn transform(env) {
                    env.meta.headers["script_runtime"] = "rhai";
                    env
                }
            "#,
        ))
        .unwrap();

        let out = engine
            .run(Envelope::new("src", json!({})))
            .unwrap()
            .unwrap();
        assert_eq!(
            out.meta.headers.get("script_runtime").map(String::as_str),
            Some("rhai")
        );
    }

    #[test]
    fn unit_return_filters_envelope() {
        let engine = RhaiEngine::new(&config("fn transform(env) { () }")).unwrap();

        let out = engine
            .run(Envelope::new("src", json!({ "skip": true })))
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn compile_error_fails_build() {
        let err = RhaiEngine::new(&config("fn transform(env) { let = }"))
            .err()
            .expect("expected compile error");
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to compile Rhai script"), "{msg}");
    }

    #[test]
    fn missing_entrypoint_fails_build() {
        let err = RhaiEngine::new(&config("fn other(env) { env }"))
            .err()
            .expect("expected missing entrypoint error");
        let msg = format!("{err:#}");
        assert!(msg.contains("missing Rhai entrypoint 'transform'"), "{msg}");
    }

    #[test]
    fn invalid_return_shape_fails_run() {
        let engine = RhaiEngine::new(&config("fn transform(env) { 42 }")).unwrap();

        let err = engine.run(Envelope::new("src", json!({}))).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to convert Rhai return value into envelope"),
            "{msg}"
        );
    }

    #[test]
    fn runtime_exception_fails_run() {
        let engine = RhaiEngine::new(&config("fn transform(env) { throw \"boom\"; }")).unwrap();

        let err = engine.run(Envelope::new("src", json!({}))).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Rhai entrypoint 'transform' failed"), "{msg}");
    }
}
