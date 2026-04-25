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

## Return semantics

- `return env` — emit the (possibly mutated) envelope downstream.
- `return nil` (or no `return`) — filter the envelope out.

## Limits

The Lua runtime does not expose an execution budget today. The Rhai-only limit fields (`max_operations`, `max_call_levels`, etc.) are **rejected** at config-load time when `runtime = "lua"` — the transform fails to register rather than silently ignoring them.

## `env` binding

| Field                    | Access                  |
| ------------------------ | ----------------------- |
| Logical key              | `env.meta.key`          |
| Source node id           | `env.meta.source_id`    |
| Producer timestamp (ms)  | `env.meta.timestamp_ms` |
| Headers map              | `env.meta.headers`      |
| Payload                  | `env.payload`           |

`env.payload` is exposed as a Lua table mirroring the JSON structure.
