use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use rhai::serde::{from_dynamic, to_dynamic};
use rhai::{AST, Dynamic, Engine, Scope};
use tokio::task;
use tokio::time;

use crate::config::redact_secret;
use crate::envelope::Envelope;
use crate::observability::{NodeCtx, ScriptTimeoutRecorder};

use super::{ScriptEngine, ScriptTimeoutError, ScriptTransformConfig};

pub struct RhaiEngine {
    inner: Arc<RhaiInner>,
    timeout: Option<Duration>,
    cancel: Arc<AtomicBool>,
    timeout_recorder: OnceLock<ScriptTimeoutRecorder>,
}

struct RhaiInner {
    engine: Engine,
    ast: AST,
    entrypoint: String,
}

#[async_trait]
impl ScriptEngine for RhaiEngine {
    async fn run(&self, env: Envelope) -> Result<Option<Envelope>> {
        // Reset the per-call cancel flag before spawning. BasicTransform
        // serializes map() calls per node, so there is at most one
        // in-flight Rhai invocation at a time and this is safe.
        self.cancel.store(false, Ordering::Relaxed);
        let inner = Arc::clone(&self.inner);
        let mut jh = task::spawn_blocking(move || inner.run_sync(env));

        let Some(timeout) = self.timeout else {
            return jh.await.context("Rhai runtime task failed")?;
        };
        let timeout_ms = timeout.as_millis() as u64;
        match time::timeout(timeout, &mut jh).await {
            Ok(joined) => joined.context("Rhai runtime task failed")?,
            Err(_elapsed) => {
                // Signal the script to abort at the next on_progress
                // tick, then wait for the blocking thread to honor it
                // before returning. Waiting keeps subsequent calls
                // sequenced behind this one — the per-call cancel
                // reset on the next invocation must not run concurrently
                // with the still-aborting prior call.
                self.cancel.store(true, Ordering::Relaxed);
                let _ = (&mut jh).await;
                if let Some(rec) = self.timeout_recorder.get() {
                    rec.record();
                }
                tracing::warn!(runtime = "rhai", timeout_ms, "Rhai script exceeded timeout");
                Err(ScriptTimeoutError { timeout_ms }.into())
            }
        }
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        let _ = self
            .timeout_recorder
            .set(ctx.script_timeout_recorder("rhai"));
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

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_progress = Arc::clone(&cancel);
        engine.on_progress(move |_| {
            if cancel_for_progress.load(Ordering::Relaxed) {
                // Returning `Some` aborts the script with a token value
                // that surfaces as a runtime error from `call_fn`.
                Some(Dynamic::from("courier.script.timeout"))
            } else {
                None
            }
        });

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
                redact_secret(&config.entrypoint)
            );
        }

        Ok(Self {
            inner: Arc::new(RhaiInner {
                engine,
                ast,
                entrypoint: config.entrypoint.clone(),
            }),
            timeout: config.timeout,
            cancel,
            timeout_recorder: OnceLock::new(),
        })
    }

    #[cfg(test)]
    fn run_sync(&self, env: Envelope) -> Result<Option<Envelope>> {
        self.inner.run_sync(env)
    }
}

impl RhaiInner {
    fn run_sync(&self, env: Envelope) -> Result<Option<Envelope>> {
        let arg = to_dynamic(env).context("failed to convert envelope into Rhai value")?;
        let mut scope = Scope::new();
        let out: Dynamic = self
            .engine
            .call_fn(&mut scope, &self.ast, &self.entrypoint, (arg,))
            .with_context(|| {
                format!(
                    "Rhai entrypoint '{}' failed",
                    redact_secret(&self.entrypoint)
                )
            })?;

        if out.is_unit() {
            return Ok(None);
        }

        from_dynamic(&out.flatten()).map(Some).map_err(|err| {
            anyhow!(err).context("failed to convert Rhai return value into envelope")
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::RhaiEngine;
    use crate::envelope::Envelope;
    use crate::transforms::script::{ScriptTimeoutError, ScriptTransformConfig};

    fn config(script: &str) -> ScriptTransformConfig {
        ScriptTransformConfig {
            runtime: super::super::ScriptRuntime::Rhai,
            script: script.into(),
            entrypoint: "transform".into(),
            timeout: None,
            python: None,
            rhai: Some(super::super::RhaiConfig {
                max_operations: 100_000,
                max_call_levels: 32,
                max_expr_depth: 64,
                max_function_expr_depth: 32,
                max_variables: 64,
            }),
            lua: None,
        }
    }

    fn config_with_timeout(script: &str, timeout: Duration) -> ScriptTransformConfig {
        let mut c = config(script);
        c.timeout = Some(timeout);
        // Allow the runaway loop to run long enough that the watchdog,
        // not the operations cap, is what aborts it.
        c.rhai = Some(super::super::RhaiConfig {
            max_operations: 10_000_000_000,
            max_call_levels: 32,
            max_expr_depth: 64,
            max_function_expr_depth: 32,
            max_variables: 64,
        });
        c
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
            .run_sync(Envelope::new("src", json!({ "value": 1 })))
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
            .run_sync(Envelope::new("src", json!({})))
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
            .run_sync(Envelope::new("src", json!({ "skip": true })))
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

        let err = engine
            .run_sync(Envelope::new("src", json!({})))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to convert Rhai return value into envelope"),
            "{msg}"
        );
    }

    #[test]
    fn runtime_exception_fails_run() {
        let engine = RhaiEngine::new(&config("fn transform(env) { throw \"boom\"; }")).unwrap();

        let err = engine
            .run_sync(Envelope::new("src", json!({})))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Rhai entrypoint 'transform' failed"), "{msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn infinite_loop_times_out() {
        use super::ScriptEngine;
        let engine = RhaiEngine::new(&config_with_timeout(
            "fn transform(env) { while true {} env }",
            Duration::from_millis(50),
        ))
        .unwrap();

        let started = std::time::Instant::now();
        let err = engine
            .run(Envelope::new("src", json!({})))
            .await
            .unwrap_err();
        // Watchdog should fire well before the operations cap would,
        // and well before any reasonable test budget.
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            err.downcast_ref::<ScriptTimeoutError>().is_some(),
            "expected ScriptTimeoutError, got: {err:#}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_does_not_break_subsequent_calls() {
        use super::ScriptEngine;
        let engine = RhaiEngine::new(&config_with_timeout(
            r#"
                fn transform(env) {
                    if env.payload["hang"] == true {
                        while true {}
                    }
                    env.payload["processed"] = true;
                    env
                }
            "#,
            Duration::from_millis(50),
        ))
        .unwrap();

        let err = engine
            .run(Envelope::new("src", json!({ "hang": true })))
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<ScriptTimeoutError>().is_some());

        // Engine must keep working after a timeout — the cancel flag
        // is reset per call.
        let out = engine
            .run(Envelope::new("src", json!({ "hang": false })))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.payload, json!({ "hang": false, "processed": true }));
    }
}
