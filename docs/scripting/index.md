---
icon: lucide/code
---

# Scripting

The built-in `script` transform lets you write per-envelope logic without recompiling Courier. Three runtimes ship out of the box:

- **[Rhai](rhai.md)** — small, sandboxed embedded runtime with a configurable execution budget. Best default for short, untrusted-ish snippets.
- **[Lua](lua.md)** — embedded via `mlua`. Familiar syntax, no subprocess overhead.
- **[Python](python.md)** — runs in a `python3` subprocess. Not sandboxed; unlocks the full Python ecosystem at the cost of process boundary overhead.

## Choosing a runtime

| Concern                          | Rhai | Lua | Python |
| -------------------------------- | :--: | :-: | :----: |
| Embedded (no subprocess)         | ✅   | ✅  | ❌     |
| Sandbox / execution budget       | ✅   | ❌  | ❌     |
| External libraries available     | ❌   | ❌  | ✅     |
| Cold-start cost                  | low  | low | higher |

When in doubt, start with Rhai. Move to Lua for slightly more familiar syntax. Reach for Python when you need libraries Courier can't reasonably ship (e.g. heavy data manipulation, ML inference).

## Common shape

All three runtimes share the same component config — just the `runtime` field changes. Common fields:

| Field         | Description |
| ------------- | ----------- |
| `runtime`     | One of `"rhai"`, `"lua"`, `"python"`. |
| `script`      | Inline source. Mutually exclusive with `script_file`. |
| `script_file` | Path to a script on disk. Relative paths are resolved from the config file's directory. |
| `entrypoint`  | Function name to call. Defaults to `"transform"`. |

Runtime-specific fields (Rhai limit knobs, `python_bin`) are documented on each runtime's page.

## The `env` binding

Each runtime exposes the [Envelope](../concepts/envelope.md) under a binding called `env`. Field access syntax differs slightly per runtime:

| Field                    | Rhai / Lua                | Python                     |
| ------------------------ | ------------------------- | -------------------------- |
| Logical key              | `env.meta.key`            | `env["meta"]["key"]`       |
| Source node id           | `env.meta.source_id`      | `env["meta"]["source_id"]` |
| Producer timestamp (ms)  | `env.meta.timestamp_ms`   | `env["meta"]["timestamp_ms"]` |
| Headers map              | `env.meta.headers`        | `env["meta"]["headers"]`   |
| Payload                  | `env.payload`             | `env["payload"]`           |

## Return semantics

Each runtime maps a "drop this envelope" outcome to its idiomatic empty value:

| Runtime | Emit transformed envelope | Filter envelope out |
| ------- | ------------------------- | ------------------- |
| Rhai    | `return env`              | `return ()`         |
| Lua     | `return env`              | `return nil`        |
| Python  | `return env`              | `return None`       |

Runtime errors (parse failures, exceptions, budget exhaustion, subprocess crashes) follow the transform's `on_error` policy. See [Error handling](../configuration/error-handling.md).
