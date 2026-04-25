# Python

The Python runtime runs your script in a `python3` subprocess and communicates over a small worker protocol on stdin/stdout. Use it when you need access to the Python ecosystem from a transform.

!!! warning "Caveats"

    - **Not sandboxed.** A Python script can do anything `python3` can do on the host.
    - **Not managed.** Courier does not create virtualenvs or install packages — that is your responsibility.
    - **stdout is reserved.** The worker protocol uses stdout. Send logs to **stderr** (e.g. `print(..., file=sys.stderr)`).

## Minimal example

```toml
[[pipelines.transforms]]
type = "script"
runtime = "python"
on_error = "drop"
script = """
def transform(env):
  if env["payload"]["userId"] == 1:
    env["meta"]["headers"]["priority"] = "high"

  env["payload"]["processed"] = True
  return env
"""
```

## `script_file`

```toml
[[pipelines.transforms]]
type = "script"
runtime = "python"
script_file = "./transforms/enrich.py"
python_bin = "python3"
```

```python title="transforms/enrich.py"
def transform(env):
    env["payload"]["processed"] = True
    return env
```

`script` and `script_file` are mutually exclusive — set exactly one.

## Choosing the interpreter

By default the runtime invokes `python3` from `PATH`. Override with `python_bin`:

```toml
python_bin = "/opt/python3.12/bin/python3"
```

This is the canonical way to point at a virtualenv:

```toml
python_bin = "./.venv/bin/python"
```

## Return semantics

- `return env` — emit the (possibly mutated) envelope downstream.
- `return None` (or no `return`) — filter the envelope out.

A subprocess crash, an uncaught exception, or a stdout/stderr protocol violation is reported as a runtime error and follows the transform's `on_error` policy.

## Limits

The Python runtime does not expose an execution budget. The Rhai-only limit fields are **rejected** at config-load time when `runtime = "python"`.

## `env` binding

The same envelope shape, exposed as a plain Python dict:

| Field                    | Access                          |
| ------------------------ | ------------------------------- |
| Logical key              | `env["meta"]["key"]`            |
| Source node id           | `env["meta"]["source_id"]`      |
| Producer timestamp (ms)  | `env["meta"]["timestamp_ms"]`   |
| Headers map              | `env["meta"]["headers"]`        |
| Payload                  | `env["payload"]`                |
