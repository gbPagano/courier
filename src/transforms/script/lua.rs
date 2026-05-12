use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, MultiValue, Value, VmState};
use tokio::task;
use tokio::time;

use crate::config::redact_secret;
use crate::envelope::Envelope;
use crate::observability::{NodeCtx, ScriptTimeoutRecorder};

use super::{ScriptEngine, ScriptTimeoutError, ScriptTransformConfig};

/// Instructions per hook tick. Smaller = tighter timeout/budget
/// granularity at higher overhead. 1000 keeps the per-tick cost in the
/// low microseconds and still allows millisecond-scale timeouts to fire
/// well within their bound.
const LUA_HOOK_EVERY_N: u32 = 1000;

pub struct LuaEngine {
    inner: Arc<LuaInner>,
    timeout: Option<Duration>,
    cancel: Arc<AtomicBool>,
    ops_counter: Arc<AtomicU64>,
    timeout_recorder: OnceLock<ScriptTimeoutRecorder>,
}

struct LuaInner {
    lua: Lua,
    entrypoint: String,
}

#[async_trait]
impl ScriptEngine for LuaEngine {
    async fn run(&self, env: Envelope) -> Result<Option<Envelope>> {
        self.cancel.store(false, Ordering::Relaxed);
        self.ops_counter.store(0, Ordering::Relaxed);

        let inner = Arc::clone(&self.inner);
        let mut jh = task::spawn_blocking(move || inner.run_sync(env));

        let Some(timeout) = self.timeout else {
            return jh.await.context("Lua runtime task failed")?;
        };
        let timeout_ms = timeout.as_millis() as u64;
        match time::timeout(timeout, &mut jh).await {
            Ok(joined) => joined.context("Lua runtime task failed")?,
            Err(_elapsed) => {
                self.cancel.store(true, Ordering::Relaxed);
                let _ = (&mut jh).await;
                if let Some(rec) = self.timeout_recorder.get() {
                    rec.record();
                }
                tracing::warn!(runtime = "lua", timeout_ms, "Lua script exceeded timeout");
                Err(ScriptTimeoutError { timeout_ms }.into())
            }
        }
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        let _ = self
            .timeout_recorder
            .set(ctx.script_timeout_recorder("lua"));
    }
}

impl LuaEngine {
    pub(super) fn new(config: &ScriptTransformConfig) -> Result<Self> {
        let lua_cfg = config
            .lua
            .as_ref()
            .expect("Lua config missing for Lua runtime");

        let lua = Lua::new();

        let cancel = Arc::new(AtomicBool::new(false));
        let ops_counter = Arc::new(AtomicU64::new(0));
        let ops_budget = lua_cfg.max_operations;

        // Only install the hook when at least one of the two things it
        // checks is configured. A no-op hook still costs ~one atomic
        // load every 1000 instructions — small, but not free.
        if config.timeout.is_some() || ops_budget.is_some() {
            let cancel_for_hook = Arc::clone(&cancel);
            let ops_for_hook = Arc::clone(&ops_counter);
            let budget = ops_budget;
            lua.set_hook(
                HookTriggers::new().every_nth_instruction(LUA_HOOK_EVERY_N),
                move |_, _| {
                    if cancel_for_hook.load(Ordering::Relaxed) {
                        return Err(mlua::Error::external(LuaAbort::Timeout));
                    }
                    if let Some(budget) = budget {
                        let used = ops_for_hook
                            .fetch_add(LUA_HOOK_EVERY_N as u64, Ordering::Relaxed)
                            + LUA_HOOK_EVERY_N as u64;
                        if used > budget {
                            return Err(mlua::Error::external(LuaAbort::BudgetExceeded));
                        }
                    }
                    Ok(VmState::Continue)
                },
            )
            .context("failed to install Lua execution hook")?;
        }

        lua.load(&config.script)
            .exec()
            .context("failed to compile Lua script")?;

        let globals = lua.globals();
        let _: Function = globals.get(config.entrypoint.as_str()).with_context(|| {
            format!(
                "missing Lua entrypoint '{}'",
                redact_secret(&config.entrypoint)
            )
        })?;

        Ok(Self {
            inner: Arc::new(LuaInner {
                lua,
                entrypoint: config.entrypoint.clone(),
            }),
            timeout: config.timeout,
            cancel,
            ops_counter,
            timeout_recorder: OnceLock::new(),
        })
    }

    #[cfg(test)]
    fn run_sync(&self, env: Envelope) -> Result<Option<Envelope>> {
        self.ops_counter.store(0, Ordering::Relaxed);
        self.cancel.store(false, Ordering::Relaxed);
        self.inner.run_sync(env)
    }
}

impl LuaInner {
    fn run_sync(&self, env: Envelope) -> Result<Option<Envelope>> {
        let globals = self.lua.globals();
        let entrypoint: Function = globals.get(self.entrypoint.as_str()).with_context(|| {
            format!(
                "missing Lua entrypoint '{}'",
                redact_secret(&self.entrypoint)
            )
        })?;
        let arg = self
            .lua
            .to_value(&env)
            .context("failed to convert envelope into Lua value")?;
        let out: MultiValue = entrypoint.call((arg,)).with_context(|| {
            format!(
                "Lua entrypoint '{}' failed",
                redact_secret(&self.entrypoint)
            )
        })?;

        let mut values = out.into_vec();
        let value = match values.len() {
            0 => return Ok(None),
            1 => values.pop().expect("single return value expected"),
            _ => bail!(
                "Lua entrypoint '{}' returned multiple values",
                redact_secret(&self.entrypoint)
            ),
        };

        match value {
            Value::Nil => Ok(None),
            other => self.lua.from_value(other).map(Some).map_err(|err| {
                anyhow!(err).context("failed to convert Lua return value into envelope")
            }),
        }
    }
}

/// Sentinel error type the hook returns to abort the running script.
/// Carrying a typed marker (rather than a stringly-typed error) lets
/// upstream code distinguish "script aborted because of our hook" from
/// a real script error if we ever need to.
#[derive(Debug, thiserror::Error)]
enum LuaAbort {
    #[error("courier.script.timeout")]
    Timeout,
    #[error("courier.script.budget_exceeded")]
    BudgetExceeded,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::LuaEngine;
    use crate::envelope::Envelope;
    use crate::transforms::script::{
        LuaConfig, ScriptRuntime, ScriptTimeoutError, ScriptTransformConfig,
    };

    fn config(script: &str) -> ScriptTransformConfig {
        ScriptTransformConfig {
            runtime: ScriptRuntime::Lua,
            script: script.into(),
            entrypoint: "transform".into(),
            timeout: None,
            python: None,
            rhai: None,
            lua: Some(LuaConfig {
                max_operations: None,
            }),
        }
    }

    fn config_with_timeout(script: &str, timeout: Duration) -> ScriptTransformConfig {
        let mut c = config(script);
        c.timeout = Some(timeout);
        c
    }

    fn config_with_budget(script: &str, budget: u64) -> ScriptTransformConfig {
        let mut c = config(script);
        c.lua = Some(LuaConfig {
            max_operations: Some(budget),
        });
        c
    }

    #[test]
    fn mutates_payload() {
        let engine = LuaEngine::new(&config(
            r#"
                function transform(env)
                    env.payload.processed = true
                    return env
                end
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
        let engine = LuaEngine::new(&config(
            r#"
                function transform(env)
                    env.meta.headers.script_runtime = "lua"
                    return env
                end
            "#,
        ))
        .unwrap();

        let out = engine
            .run_sync(Envelope::new("src", json!({})))
            .unwrap()
            .unwrap();
        assert_eq!(
            out.meta.headers.get("script_runtime").map(String::as_str),
            Some("lua")
        );
    }

    #[test]
    fn nil_return_filters_envelope() {
        let engine = LuaEngine::new(&config("function transform(env) return nil end")).unwrap();

        let out = engine
            .run_sync(Envelope::new("src", json!({ "skip": true })))
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn compile_error_fails_build() {
        let err = LuaEngine::new(&config("function transform(env) local = end"))
            .err()
            .expect("expected compile error");
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to compile Lua script"), "{msg}");
    }

    #[test]
    fn missing_entrypoint_fails_build() {
        let err = LuaEngine::new(&config("function other(env) return env end"))
            .err()
            .expect("expected missing entrypoint error");
        let msg = format!("{err:#}");
        assert!(msg.contains("missing Lua entrypoint 'transform'"), "{msg}");
    }

    #[test]
    fn invalid_return_shape_fails_run() {
        let engine = LuaEngine::new(&config("function transform(env) return 42 end")).unwrap();

        let err = engine
            .run_sync(Envelope::new("src", json!({})))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to convert Lua return value into envelope"),
            "{msg}"
        );
    }

    #[test]
    fn runtime_exception_fails_run() {
        let engine = LuaEngine::new(&config("function transform(env) error('boom') end")).unwrap();

        let err = engine
            .run_sync(Envelope::new("src", json!({})))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Lua entrypoint 'transform' failed"), "{msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn infinite_loop_times_out() {
        use super::ScriptEngine;
        let engine = LuaEngine::new(&config_with_timeout(
            "function transform(env) while true do end return env end",
            Duration::from_millis(50),
        ))
        .unwrap();

        let started = std::time::Instant::now();
        let err = engine
            .run(Envelope::new("src", json!({})))
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            err.downcast_ref::<ScriptTimeoutError>().is_some(),
            "expected ScriptTimeoutError, got: {err:#}"
        );
    }

    #[test]
    fn instruction_budget_aborts_tight_loop() {
        let engine = LuaEngine::new(&config_with_budget(
            "function transform(env) while true do end return env end",
            10_000,
        ))
        .unwrap();

        let err = engine
            .run_sync(Envelope::new("src", json!({})))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("budget_exceeded"), "{msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_does_not_break_subsequent_calls() {
        use super::ScriptEngine;
        let engine = LuaEngine::new(&config_with_timeout(
            r#"
                function transform(env)
                    if env.payload.hang then
                        while true do end
                    end
                    env.payload.processed = true
                    return env
                end
            "#,
            Duration::from_millis(50),
        ))
        .unwrap();

        let err = engine
            .run(Envelope::new("src", json!({ "hang": true })))
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<ScriptTimeoutError>().is_some());

        let out = engine
            .run(Envelope::new("src", json!({ "hang": false })))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.payload, json!({ "hang": false, "processed": true }));
    }
}
