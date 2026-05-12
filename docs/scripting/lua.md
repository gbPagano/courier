---
icon: lucide/moon
---

# Lua

The Lua runtime is embedded in-process via [`mlua`](https://github.com/mlua-rs/mlua). Use it when you want familiar Lua syntax for small transforms.

## Minimal example

```toml
[[pipelines.transforms]]
type = "script"
runtime = "lua"
on_error = "drop"
script = """
function transform(env)
  if env.payload.userId == 1 then
    env.meta.headers.priority = "high"
  end

  env.payload.processed = true
  return env
end
"""
```

## `script_file`

```toml
[[pipelines.transforms]]
type = "script"
runtime = "lua"
script_file = "./transforms/enrich.lua"
```

```lua title="transforms/enrich.lua"
function transform(env)
  env.payload.processed = true
  return env
end
```

`script` and `script_file` are mutually exclusive — set exactly one.
Relative `script_file` paths are resolved from the config file's directory.

## Return semantics

- `return env` — emit the (possibly mutated) envelope downstream.
- `return nil` (or no `return`) — filter the envelope out.

## Limits

Set `max_operations` to cap how many Lua VM instructions a single envelope can execute. Courier installs an `mlua` hook that fires every 1000 instructions and aborts the script once the cumulative count exceeds the budget. Unset by default (no budget).

The other Rhai-only limit fields (`max_call_levels`, `max_expr_depth`, `max_function_expr_depth`, `max_variables`) are **rejected** at config-load time when `runtime = "lua"` — they have no analogue in the Lua engine, so the transform fails to register rather than silently ignoring them.

See [Script Security](security.md) for the broader security model — Lua is **not** sandboxed by default (`io`, `os`, `package` are exposed). Pair `max_operations` with `timeout_ms` and OS-level isolation for untrusted scripts.

## `env` binding

| Field                    | Access                  |
| ------------------------ | ----------------------- |
| Logical key              | `env.meta.key`          |
| Source node id           | `env.meta.source_id`    |
| Producer timestamp (ms)  | `env.meta.timestamp_ms` |
| Headers map              | `env.meta.headers`      |
| Payload                  | `env.payload`           |

`env.payload` is exposed as a Lua table mirroring the JSON structure.
