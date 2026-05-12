---
icon: lucide/shield
---

# Script Security

This page documents the security model for each script runtime, the guardrails Courier enforces, how `script_file` paths are resolved, and recommended configuration for production deployments.

## Runtime security models

### Rhai

Rhai runs **fully in-process and sandboxed**. The engine exposes no I/O, filesystem, network, or OS primitives by default. Scripts cannot spawn processes, read files, or make network calls.

In addition, Courier enables a configurable execution budget to guard against infinite loops and runaway recursion:

| Limit                     | Default    | Effect when exceeded         |
| ------------------------- | ---------- | ---------------------------- |
| `max_operations`          | `100 000`  | Operation budget exhausted → runtime error |
| `max_call_levels`         | `32`       | Stack overflow → runtime error |
| `max_expr_depth`          | `64`       | Expression too deeply nested → compile error |
| `max_function_expr_depth` | `32`       | Function body too deeply nested → compile error |
| `max_variables`           | `64`       | Too many variables in scope → runtime error |

Budget exhaustion and other script errors follow the transform's `on_error` policy (`drop` or `fail_pipeline`).

**Recommendation:** set `timeout_ms` in production. Even with the operation budget, a script calling built-ins in a tight loop can stay within the budget while spending wall-clock time. The timeout is a second line of defence.

### Lua

Lua runs **in-process via `mlua`** with the `ALL_SAFE` standard library set, which **includes `io`, `os`, and `package`**. That means a Lua script today has the same broad capabilities as Python: it can open files (`io.open`), run subprocesses (`os.execute`), read environment variables (`os.getenv`), and load shared libraries (`package.loadlib`). Only `debug` and `ffi` are excluded.

Because Lua is **in-process** rather than a separate subprocess, there is no kill-the-PID escape hatch — a misbehaving script is contained only by the execution hook, the timeout watchdog, and the host process boundary.

Lua does support a static operation budget via `max_operations`: when set, Courier installs an `mlua` hook that fires every 1000 VM instructions and aborts the script once the cumulative count exceeds the budget. The other Rhai-only knobs (`max_call_levels`, `max_expr_depth`, `max_function_expr_depth`, `max_variables`) are rejected at config-load time when `runtime = "lua"`.

**Recommendation:** Treat Lua with the same caution as Python.

- Always set `timeout_ms` in production — the primary guard against scripts that spin indefinitely.
- Set `max_operations` as a secondary guard. The default is no budget; pick a value generous enough for your transform logic but tight enough to cap a runaway loop (e.g. `1_000_000` instructions ≈ a few milliseconds).
- Run Courier as a low-privilege user and consider container restrictions (`--cap-drop=ALL`, read-only mounts, network policy) if accepting untrusted Lua scripts.

### Python

Python runs in a **`python3` subprocess** — it is **not sandboxed**. A Python script has the same capabilities as any `python3` process running as the Courier user: filesystem access, network access, subprocess spawning, environment variable reading, and package imports.

Courier manages the subprocess lifecycle:

- The subprocess is started once and reused across envelopes (amortised startup cost).
- It is **killed** when the pipeline's cancellation token fires (graceful shutdown or `FailPipeline`).
- `stdout` is reserved for the worker protocol. Scripts must send logs and debug output to **`stderr`** (e.g. `print(..., file=sys.stderr)`).

**Recommendation:** Always set `timeout_ms` for Python transforms in production — it is the only mechanism that limits how long a single envelope invocation can run. Additionally:

- Run Courier as a low-privilege user (e.g. `systemd User=courier` or a non-root container user).
- If accepting untrusted or user-supplied Python scripts, run Courier inside a container with restricted capabilities (`--cap-drop=ALL`, read-only filesystem mounts, network policy).
- Pin `python_bin` to a controlled interpreter rather than relying on `PATH`.

## Size guardrails

Two optional config fields cap the serialized payload size before and after the engine runs:

| Field                   | Direction | Effect                                         |
| ----------------------- | --------- | ---------------------------------------------- |
| `max_payload_bytes_in`  | inbound   | Payload too large → error *before* engine runs |
| `max_payload_bytes_out` | outbound  | Engine output too large → error *after* return |

Both default to `None` (disabled). When triggered they fire `on_error` just like any other transform error.

`max_payload_bytes_in` protects the engine from spending CPU on an absurdly large input. `max_payload_bytes_out` protects downstream channels from a script that inflates its input (e.g. a data-copying loop).

## `script_file` path resolution

When `script_file` is a relative path, it is resolved relative to the directory of the config file that declares it:

```
/etc/courier/
  config.toml         ← declares script_file = "./transforms/enrich.rhai"
  transforms/
    enrich.rhai       ← resolved to /etc/courier/transforms/enrich.rhai
```

Absolute paths are passed through unchanged:

```toml
script_file = "/opt/courier/scripts/enrich.rhai"   # used as-is
```

When the config is loaded from an in-memory string (e.g. via the Rust API `Config::from_toml_str`), there is no base directory and relative paths are resolved against the process working directory at script-load time.

In directory mode (`COURIER_CONFIG=/etc/courier/conf.d/`), each file in the directory uses its own parent directory as the base — there is no cross-file inheritance.

## Metrics

Two script-specific counters are exported via OTLP:

| Metric                                   | Labels                              | Meaning |
| ---------------------------------------- | ----------------------------------- | ------- |
| `courier_script_timeouts_total`          | `runtime`, `node_id`                | Invocations aborted by `timeout_ms` |
| `courier_script_payload_too_large_total` | `runtime`, `node_id`, `direction`   | Envelopes rejected by a size guardrail |

`direction` is `"in"` (rejected before the engine ran) or `"out"` (rejected after the engine returned).

If `courier_script_timeouts_total` starts growing, the script is taking longer than `timeout_ms` per envelope — either tighten the script logic, raise the timeout budget, or reduce the incoming envelope rate. If timeouts are systematic (not just occasional spikes), the pipeline will drop or fail envelopes depending on `on_error`, so the upstream source will back up.

If `courier_script_payload_too_large_total` grows for `direction = "in"`, the envelopes arriving at the transform are larger than expected — review the upstream source or reduce `max_payload_bytes_in` conservatively if the source data is legitimately large. For `direction = "out"`, the script is inflating the payload beyond `max_payload_bytes_out` — review the transform logic for accidental data duplication or unbounded accumulation.
