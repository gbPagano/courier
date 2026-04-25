# Transforms

Transforms operate on a stream of envelopes between the source and the sinks. They are ordered — `pipelines.transforms[0]` runs first.

The simplest transform implements `MapOne::map(env) -> Option<Envelope>` (returning `None` filters the envelope out) and is wrapped in a `BasicTransform` that handles the recv loop, cancellation, and `on_error`. See [Architecture](../concepts/architecture.md#traits).

## `set_key`

Copies a payload field into `meta.key`. Useful for setting Kafka partition keys without writing a script.

```toml
[[pipelines.transforms]]
type = "set_key"
from_field = "userId"
```

| Field        | Required | Description |
| ------------ | -------- | ----------- |
| `from_field` | yes      | Top-level payload field whose value becomes `meta.key`. String values are copied as-is; other JSON values are stringified. |

If the field is missing or the payload is not an object, the transform leaves `meta.key` unchanged. `on_error` only applies when the transform itself returns an error.

## `script`

Runs a user-provided script per envelope. Three runtimes are supported:

| Runtime  | `runtime` value | Notes |
| -------- | --------------- | ----- |
| Rhai     | `"rhai"`        | Embedded sandboxed runtime, configurable execution budget. |
| Lua      | `"lua"`         | Embedded via `mlua`. |
| Python   | `"python"`      | Runs in a `python3` subprocess; not sandboxed. |

```toml
[[pipelines.transforms]]
type = "script"
runtime = "rhai"
on_error = "drop"
script = """
fn transform(env) {
  env.payload["processed"] = true;
  env
}
"""
```

| Field                     | Required | Description |
| ------------------------- | -------- | ----------- |
| `runtime`                 | yes      | One of `"rhai"`, `"lua"`, `"python"`. |
| `script`                  | one of   | Inline source code. Mutually exclusive with `script_file`. |
| `script_file`             | one of   | Path to a script on disk. |
| `entrypoint`              | no       | Function name to call. Defaults to `"transform"`. |
| `python_bin`              | python   | Interpreter path. Defaults to `"python3"`. |
| `max_operations`          | rhai     | Operation budget. Default `100000`. |
| `max_call_levels`         | rhai     | Call stack depth. Default `32`. |
| `max_expr_depth`          | rhai     | Expression nesting depth. Default `64`. |
| `max_function_expr_depth` | rhai     | Per-function expression depth. Default `32`. |
| `max_variables`           | rhai     | Max variables in scope. Default `64`. |

Lua and Python reject the Rhai-only limit fields rather than silently ignoring them.

See [Scripting](../scripting/index.md) for runtime-specific guides — including the exact `env` binding shape, return semantics, and per-runtime caveats.

## Writing your own transform

The simplest path is to implement `MapOne` and wrap it in `BasicTransform`. For stateful transforms (batching, flat-map, background work), implement the full `Transform` trait directly. Register a `TransformFactory` against a unique `kind` — see [Contributing](../contributing.md).
