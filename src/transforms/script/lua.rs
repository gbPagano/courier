use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use mlua::{Function, Lua, LuaSerdeExt, MultiValue, Value};

use crate::config::redact_secret;
use crate::envelope::Envelope;

use super::{ScriptEngine, ScriptTransformConfig};

pub struct LuaEngine {
    lua: Lua,
    entrypoint: String,
}

#[async_trait]
impl ScriptEngine for LuaEngine {
    async fn run(&self, env: Envelope) -> Result<Option<Envelope>> {
        self.run_inner(env)
    }
}

impl LuaEngine {
    pub(super) fn new(config: &ScriptTransformConfig) -> Result<Self> {
        let lua = Lua::new();
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
            lua,
            entrypoint: config.entrypoint.clone(),
        })
    }

    #[cfg(test)]
    fn run(&self, env: Envelope) -> Result<Option<Envelope>> {
        self.run_inner(env)
    }

    fn run_inner(&self, env: Envelope) -> Result<Option<Envelope>> {
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::LuaEngine;
    use crate::envelope::Envelope;
    use crate::transforms::script::{ScriptRuntime, ScriptTransformConfig};

    fn config(script: &str) -> ScriptTransformConfig {
        ScriptTransformConfig {
            runtime: ScriptRuntime::Lua,
            script: script.into(),
            entrypoint: "transform".into(),
            python: None,
            rhai: None,
        }
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
            .run(Envelope::new("src", json!({ "value": 1 })))
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
            .run(Envelope::new("src", json!({})))
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
            .run(Envelope::new("src", json!({ "skip": true })))
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

        let err = engine.run(Envelope::new("src", json!({}))).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to convert Lua return value into envelope"),
            "{msg}"
        );
    }

    #[test]
    fn runtime_exception_fails_run() {
        let engine = LuaEngine::new(&config("function transform(env) error('boom') end")).unwrap();

        let err = engine.run(Envelope::new("src", json!({}))).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Lua entrypoint 'transform' failed"), "{msg}");
    }
}
