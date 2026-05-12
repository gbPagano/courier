use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::task;
use tokio::time;

use crate::config::redact_secret;
use crate::envelope::Envelope;
use crate::observability::{NodeCtx, ScriptTimeoutRecorder};

use super::{ScriptEngine, ScriptTimeoutError, ScriptTransformConfig};

const PYTHON_BOOTSTRAP: &str = r#"
import json
import sys
import traceback

entrypoint_name = sys.argv[1]
namespace = {}

try:
    script = json.loads(sys.stdin.readline())
    exec(script, namespace)
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
    spawn_cfg: Arc<PythonSpawnConfig>,
    /// IO half of the worker. Locked by every `run` call. The watchdog
    /// never touches this — on timeout it goes after `child` instead, so
    /// kill-from-watchdog never blocks on the in-flight call's lock.
    io: Arc<Mutex<Option<PythonIo>>>,
    /// Child handle, kept separate from the IO so the timeout watchdog
    /// can call `Child::kill()` while a `run` is still holding the IO
    /// lock waiting on stdout.
    child: Arc<Mutex<Option<Child>>>,
    timeout: Option<Duration>,
    timeout_recorder: OnceLock<ScriptTimeoutRecorder>,
}

struct PythonSpawnConfig {
    bin: String,
    script: String,
    entrypoint: String,
}

struct PythonIo {
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
        let spawn_cfg = Arc::clone(&self.spawn_cfg);
        let io = Arc::clone(&self.io);
        let child = Arc::clone(&self.child);
        let entrypoint = self.spawn_cfg.entrypoint.clone();

        let mut jh = task::spawn_blocking(move || {
            ensure_worker(&spawn_cfg, &io, &child)?;
            let mut guard = io
                .lock()
                .map_err(|_| anyhow!("Python worker IO lock poisoned"))?;
            let worker_io = guard
                .as_mut()
                .ok_or_else(|| anyhow!("Python worker missing after ensure"))?;
            run_python_call(worker_io, &entrypoint, env)
        });

        let Some(timeout) = self.timeout else {
            return jh.await.context("Python runtime task failed")?;
        };
        let timeout_ms = timeout.as_millis() as u64;
        match time::timeout(timeout, &mut jh).await {
            Ok(joined) => joined.context("Python runtime task failed")?,
            Err(_elapsed) => {
                // Kill the subprocess via the dedicated child handle.
                // The in-flight `run_python_call` will then observe EOF
                // on stdout and return with an error; we still await the
                // JoinHandle so the next call starts from a clean slate.
                kill_child(&self.child);
                let _ = (&mut jh).await;
                // Drop the IO; next call will respawn.
                if let Ok(mut g) = self.io.lock() {
                    *g = None;
                }
                if let Some(rec) = self.timeout_recorder.get() {
                    rec.record();
                }
                tracing::warn!(
                    runtime = "python",
                    timeout_ms,
                    "Python script exceeded timeout; subprocess killed"
                );
                Err(ScriptTimeoutError { timeout_ms }.into())
            }
        }
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        let _ = self
            .timeout_recorder
            .set(ctx.script_timeout_recorder("python"));
    }
}

impl PythonEngine {
    pub(super) fn new(config: &ScriptTransformConfig) -> Result<Self> {
        let python = config
            .python
            .as_ref()
            .expect("Python config missing for Python runtime");

        let spawn_cfg = Arc::new(PythonSpawnConfig {
            bin: python.bin.clone(),
            script: config.script.clone(),
            entrypoint: config.entrypoint.clone(),
        });

        // Spawn eagerly so a bad interpreter / compile error fails the
        // factory at build time, not on the first envelope.
        let (io, child) = spawn_worker(&spawn_cfg)?;

        Ok(Self {
            spawn_cfg,
            io: Arc::new(Mutex::new(Some(io))),
            child: Arc::new(Mutex::new(Some(child))),
            timeout: config.timeout,
            timeout_recorder: OnceLock::new(),
        })
    }

    #[cfg(test)]
    fn run_sync(&self, env: Envelope) -> Result<Option<Envelope>> {
        ensure_worker(&self.spawn_cfg, &self.io, &self.child)?;
        let mut guard = self
            .io
            .lock()
            .map_err(|_| anyhow!("Python worker IO lock poisoned"))?;
        let worker_io = guard
            .as_mut()
            .ok_or_else(|| anyhow!("Python worker missing after ensure"))?;
        run_python_call(worker_io, &self.spawn_cfg.entrypoint, env)
    }
}

fn ensure_worker(
    spawn_cfg: &PythonSpawnConfig,
    io: &Mutex<Option<PythonIo>>,
    child: &Mutex<Option<Child>>,
) -> Result<()> {
    {
        let guard = io
            .lock()
            .map_err(|_| anyhow!("Python worker IO lock poisoned"))?;
        if guard.is_some() {
            return Ok(());
        }
    }
    let (new_io, new_child) = spawn_worker(spawn_cfg)?;
    let mut io_guard = io
        .lock()
        .map_err(|_| anyhow!("Python worker IO lock poisoned"))?;
    let mut child_guard = child
        .lock()
        .map_err(|_| anyhow!("Python worker child lock poisoned"))?;
    *io_guard = Some(new_io);
    *child_guard = Some(new_child);
    Ok(())
}

fn spawn_worker(cfg: &PythonSpawnConfig) -> Result<(PythonIo, Child)> {
    let mut child = Command::new(&cfg.bin)
        .arg("-u")
        .arg("-c")
        .arg(PYTHON_BOOTSTRAP)
        .arg(&cfg.entrypoint)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "failed to spawn Python interpreter '{}'",
                redact_secret(&cfg.bin)
            )
        })?;

    let mut stdin = child
        .stdin
        .take()
        .context("failed to capture Python stdin")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to capture Python stdout")?;
    serde_json::to_writer(&mut stdin, &cfg.script)
        .context("failed to encode Python script for bootstrap")?;
    stdin
        .write_all(b"\n")
        .context("failed to write Python bootstrap script delimiter")?;
    stdin
        .flush()
        .context("failed to flush Python bootstrap script")?;
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

    Ok((
        PythonIo {
            stdin: BufWriter::new(stdin),
            stdout,
        },
        child,
    ))
}

fn run_python_call(io: &mut PythonIo, entrypoint: &str, env: Envelope) -> Result<Option<Envelope>> {
    serde_json::to_writer(&mut io.stdin, &env)
        .context("failed to encode envelope for Python runtime")?;
    io.stdin
        .write_all(b"\n")
        .context("failed to write Python request delimiter")?;
    io.stdin.flush().context("failed to flush Python request")?;

    let mut line = String::new();
    let bytes = io
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

fn kill_child(child: &Mutex<Option<Child>>) {
    if let Ok(mut guard) = child.lock()
        && let Some(mut c) = guard.take()
    {
        let _ = c.kill();
        let _ = c.wait();
    }
}

impl Drop for PythonEngine {
    fn drop(&mut self) {
        kill_child(&self.child);
        // Best-effort cleanup of the IO half so its descriptors close
        // and Python sees EOF if it somehow outlived the kill.
        if let Ok(mut g) = self.io.lock() {
            *g = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::PythonEngine;
    use crate::envelope::Envelope;
    use crate::transforms::script::{
        PythonConfig, ScriptRuntime, ScriptTimeoutError, ScriptTransformConfig,
    };

    fn config_with_entrypoint(script: &str, entrypoint: &str) -> ScriptTransformConfig {
        ScriptTransformConfig {
            runtime: ScriptRuntime::Python,
            script: script.into(),
            entrypoint: entrypoint.into(),
            timeout: None,
            max_payload_bytes_in: None,
            max_payload_bytes_out: None,
            python: Some(PythonConfig {
                bin: "python3".into(),
            }),
            rhai: None,
            lua: None,
        }
    }

    fn config(script: &str) -> ScriptTransformConfig {
        config_with_entrypoint(script, "transform")
    }

    fn config_with_timeout(script: &str, timeout: Duration) -> ScriptTransformConfig {
        let mut c = config(script);
        c.timeout = Some(timeout);
        c
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
            .run_sync(Envelope::new("src", json!({ "value": 1 })))
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
            .run_sync(Envelope::new("src", json!({})))
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
            .run_sync(Envelope::new("src", json!({ "skip": true })))
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn script_is_not_exposed_to_python_child_processes_as_env() {
        let engine = PythonEngine::new(&config(
            r#"
import subprocess
import sys

def transform(env):
    out = subprocess.check_output([
        sys.executable,
        "-c",
        "import os; print(os.environ.get('COURIER_PYTHON_SCRIPT', ''))",
    ], text=True)
    env["payload"]["inherited_script"] = out.strip()
    return env
"#,
        ))
        .unwrap();

        let out = engine
            .run_sync(Envelope::new("src", json!({})))
            .unwrap()
            .unwrap();
        assert_eq!(out.payload, json!({ "inherited_script": "" }));
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
            .run_sync(Envelope::new("src", json!({ "value": 1 })))
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

        let err = engine
            .run_sync(Envelope::new("src", json!({})))
            .unwrap_err();
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

        let err = engine
            .run_sync(Envelope::new("src", json!({})))
            .unwrap_err();
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn infinite_loop_times_out_and_respawns() {
        use super::ScriptEngine;
        let engine = PythonEngine::new(&config_with_timeout(
            r#"
import time

def transform(env):
    if env["payload"].get("hang"):
        while True:
            time.sleep(0.01)
    env["payload"]["processed"] = True
    return env
"#,
            Duration::from_millis(200),
        ))
        .unwrap();

        let started = std::time::Instant::now();
        let err = engine
            .run(Envelope::new("src", json!({ "hang": true })))
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            err.downcast_ref::<ScriptTimeoutError>().is_some(),
            "expected ScriptTimeoutError, got: {err:#}"
        );

        // Next call must respawn the worker and succeed.
        let out = engine
            .run(Envelope::new("src", json!({ "hang": false })))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.payload, json!({ "hang": false, "processed": true }));
    }
}
