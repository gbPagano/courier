use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::task;

use crate::config::redact_secret;
use crate::envelope::Envelope;

use super::{ScriptEngine, ScriptTransformConfig};

const PYTHON_BOOTSTRAP: &str = r#"
import json
import os
import sys
import traceback

entrypoint_name = sys.argv[1]
namespace = {}

try:
    exec(os.environ['COURIER_PYTHON_SCRIPT'], namespace)
    entrypoint = namespace.get(entrypoint_name)
    if not callable(entrypoint):
        raise RuntimeError(f"missing Python entrypoint '{entrypoint_name}'")
    sys.stdout.write(json.dumps({"ok": True, "ready": True}) + "\n")
    sys.stdout.flush()
except Exception:
    sys.stdout.write(json.dumps({"ok": False, "error": traceback.format_exc()}) + "\n")
    sys.stdout.flush()
    raise SystemExit(1)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        env = json.loads(line)
        result = entrypoint(env)
        if result is None:
            response = {"ok": True, "filtered": True}
        else:
            response = {"ok": True, "filtered": False, "env": result}
    except Exception:
        response = {"ok": False, "error": traceback.format_exc()}

    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#;

pub struct PythonEngine {
    entrypoint: String,
    worker: Arc<Mutex<PythonWorker>>,
}

struct PythonWorker {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

#[derive(Deserialize)]
struct PythonInitResponse {
    ok: bool,
    error: Option<String>,
}

#[derive(Deserialize)]
struct PythonRunResponse {
    ok: bool,
    filtered: Option<bool>,
    env: Option<Value>,
    error: Option<String>,
}

#[async_trait]
impl ScriptEngine for PythonEngine {
    async fn run(&self, env: Envelope) -> Result<Option<Envelope>> {
        let worker = Arc::clone(&self.worker);
        let entrypoint = self.entrypoint.clone();

        task::spawn_blocking(move || run_python_worker(&worker, &entrypoint, env))
            .await
            .context("Python runtime task failed")?
    }
}

impl PythonEngine {
    pub(super) fn new(config: &ScriptTransformConfig) -> Result<Self> {
        let python = config
            .python
            .as_ref()
            .expect("Python config missing for Python runtime");

        let mut child = Command::new(&python.bin)
            .arg("-u")
            .arg("-c")
            .arg(PYTHON_BOOTSTRAP)
            .arg(&config.entrypoint)
            .env("COURIER_PYTHON_SCRIPT", &config.script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn Python interpreter '{}'",
                    redact_secret(&python.bin)
                )
            })?;

        let stdin = child
            .stdin
            .take()
            .context("failed to capture Python stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to capture Python stdout")?;
        let mut stdout = BufReader::new(stdout);

        let mut line = String::new();
        let bytes = stdout
            .read_line(&mut line)
            .context("failed to read Python bootstrap response")?;
        if bytes == 0 {
            bail!("Python bootstrap exited before initialization completed");
        }

        let init: PythonInitResponse = serde_json::from_str(line.trim_end())
            .context("failed to parse Python bootstrap response")?;
        if !init.ok {
            bail!(
                "failed to initialize Python runtime: {}",
                init.error.unwrap_or_else(|| "unknown error".into())
            );
        }

        Ok(Self {
            entrypoint: config.entrypoint.clone(),
            worker: Arc::new(Mutex::new(PythonWorker {
                child,
                stdin: BufWriter::new(stdin),
                stdout,
            })),
        })
    }

    #[cfg(test)]
    fn run(&self, env: Envelope) -> Result<Option<Envelope>> {
        run_python_worker(&self.worker, &self.entrypoint, env)
    }
}

fn run_python_worker(
    worker: &Mutex<PythonWorker>,
    entrypoint: &str,
    env: Envelope,
) -> Result<Option<Envelope>> {
    let mut worker = worker
        .lock()
        .map_err(|_| anyhow!("Python worker lock poisoned"))?;

    serde_json::to_writer(&mut worker.stdin, &env)
        .context("failed to encode envelope for Python runtime")?;
    worker
        .stdin
        .write_all(b"\n")
        .context("failed to write Python request delimiter")?;
    worker
        .stdin
        .flush()
        .context("failed to flush Python request")?;

    let mut line = String::new();
    let bytes = worker
        .stdout
        .read_line(&mut line)
        .context("failed to read Python runtime response")?;
    if bytes == 0 {
        bail!(
            "Python entrypoint '{}' exited before returning a response",
            entrypoint
        );
    }

    let response: PythonRunResponse =
        serde_json::from_str(line.trim_end()).context("failed to parse Python runtime response")?;
    if !response.ok {
        bail!(
            "Python entrypoint '{}' failed: {}",
            entrypoint,
            response.error.unwrap_or_else(|| "unknown error".into())
        );
    }

    if response.filtered.unwrap_or(false) {
        return Ok(None);
    }

    let env = response
        .env
        .context("Python runtime did not return an envelope")?;
    serde_json::from_value(env)
        .map(Some)
        .map_err(|err| anyhow!(err).context("failed to convert Python return value into envelope"))
}

impl Drop for PythonEngine {
    fn drop(&mut self) {
        if let Ok(mut worker) = self.worker.lock() {
            let _ = worker.child.kill();
            let _ = worker.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::PythonEngine;
    use crate::envelope::Envelope;
    use crate::transforms::script::{PythonConfig, ScriptRuntime, ScriptTransformConfig};

    fn config_with_entrypoint(script: &str, entrypoint: &str) -> ScriptTransformConfig {
        ScriptTransformConfig {
            runtime: ScriptRuntime::Python,
            script: script.into(),
            entrypoint: entrypoint.into(),
            python: Some(PythonConfig {
                bin: "python3".into(),
            }),
            rhai: None,
        }
    }

    fn config(script: &str) -> ScriptTransformConfig {
        config_with_entrypoint(script, "transform")
    }

    #[test]
    fn mutates_payload() {
        let engine = PythonEngine::new(&config(
            r#"

def transform(env):
    env["payload"]["processed"] = True
    return env
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
        let engine = PythonEngine::new(&config(
            r#"

def transform(env):
    env["meta"]["headers"]["script_runtime"] = "python"
    return env
"#,
        ))
        .unwrap();

        let out = engine
            .run(Envelope::new("src", json!({})))
            .unwrap()
            .unwrap();
        assert_eq!(
            out.meta.headers.get("script_runtime").map(String::as_str),
            Some("python")
        );
    }

    #[test]
    fn none_return_filters_envelope() {
        let engine = PythonEngine::new(&config(
            r#"

def transform(env):
    return None
"#,
        ))
        .unwrap();

        let out = engine
            .run(Envelope::new("src", json!({ "skip": true })))
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn compile_error_fails_build() {
        let err = PythonEngine::new(&config("def transform(env):\n    if\n"))
            .err()
            .expect("expected compile error");
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to initialize Python runtime"), "{msg}");
    }

    #[test]
    fn supports_custom_entrypoint() {
        let engine = PythonEngine::new(&config_with_entrypoint(
            r#"

def process(env):
    env["payload"]["processed"] = True
    return env
"#,
            "process",
        ))
        .unwrap();

        let out = engine
            .run(Envelope::new("src", json!({ "value": 1 })))
            .unwrap()
            .unwrap();
        assert_eq!(out.payload, json!({ "value": 1, "processed": true }));
    }

    #[test]
    fn missing_entrypoint_fails_build() {
        let err = PythonEngine::new(&config("def other(env):\n    return env\n"))
            .err()
            .expect("expected missing entrypoint error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing Python entrypoint 'transform'"),
            "{msg}"
        );
    }

    #[test]
    fn invalid_return_shape_fails_run() {
        let engine = PythonEngine::new(&config(
            r#"

def transform(env):
    return 42
"#,
        ))
        .unwrap();

        let err = engine.run(Envelope::new("src", json!({}))).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to convert Python return value into envelope"),
            "{msg}"
        );
    }

    #[test]
    fn runtime_exception_fails_run() {
        let engine = PythonEngine::new(&config(
            r#"

def transform(env):
    raise RuntimeError("boom")
"#,
        ))
        .unwrap();

        let err = engine.run(Envelope::new("src", json!({}))).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Python entrypoint 'transform' failed"),
            "{msg}"
        );
    }

    #[test]
    fn missing_interpreter_fails_build() {
        let mut config = config("def transform(env):\n    return env\n");
        config.python = Some(PythonConfig {
            bin: "__courier_missing_python__".into(),
        });

        let err = PythonEngine::new(&config)
            .err()
            .expect("expected missing interpreter error");
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to spawn Python interpreter"), "{msg}");
    }
}
