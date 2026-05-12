---
icon: lucide/workflow
---

# Transform configuration

Transforms run between the source and sinks. They are evaluated in the order listed in `pipelines.transforms`. Use the table to jump to the configuration reference for each supported transform.

| Kind | Use when |
| ---- | -------- |
| [`set_key`](#set_key) | Copying a payload field into `meta.key`. |
| [`filter`](#filter) | Dropping envelopes that do not match an expression. |
| [`batch`](#batch) | Grouping envelopes by count and/or time window. |
| [`mutate`](#mutate) | Adding, removing, renaming, or casting fields without a script. |
| [`script`](#script) | Running Rhai, Lua, or Python per envelope. |

## set_key

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

## filter

Drops envelopes that do not match a predicate. The predicate is a small expression language evaluated natively against each envelope, so no scripting runtime is required.

```toml
[[pipelines.transforms]]
type = "filter"
predicate = "payload.status == \"ok\""
```

Supported expressions:

- **Field paths**: `payload.userId`, `meta.headers.priority`, `meta.key`
- **Literals**: strings (`"high"`), numbers (`42`, `3.14`), booleans (`true`, `false`), `null`
- **Comparison**: `==`, `!=`, `<`, `<=`, `>`, `>=`
- **Logical**: `&&`, `||`, `!`
- **Grouping**: `(expr)`
- **Existence**: `exists payload.optionalField`

| Field       | Required | Description |
| ----------- | -------- | ----------- |
| `predicate` | yes      | Expression string. Unparseable predicates are rejected at config load. |

Examples:

```toml
# Keep only high-priority items from prod
predicate = "meta.headers.priority == \"high\" && meta.headers.env == \"prod\""

# Drop envelopes missing a required field
predicate = "exists payload.userId && payload.userId != null"

# Numeric range filter
predicate = "payload.score >= 0.5 && payload.score <= 1.0"
```

## batch

Groups envelopes into batches by maximum count and/or maximum time window, emitting a single envelope downstream containing an array of the original payloads.

```toml
[[pipelines.transforms]]
type = "batch"
max_size = 100
max_delay_ms = 5000
```

| Field             | Required | Description |
| ----------------- | -------- | ----------- |
| `max_size`        | yes      | Maximum number of envelopes per batch. Must be > 0. |
| `max_delay_ms`    | no       | Maximum time to wait before emitting a partial batch. |
| `payload_key`     | no       | Key under which the batch array is stored. Default `"items"`. |
| `flush_on_cancel` | no       | Whether to emit a partial batch on shutdown. Default `true`. |

The emitted envelope has:

- `meta` copied from the first envelope in the batch, with `key` set to `batch-{timestamp}`
- `payload` shaped as `{ "items": [...], "_batch_count": N, "_batch_first_timestamp_ms": T }`

Because channels are the acknowledgement boundary, the batched envelope is acknowledged only when downstream work completes, which in turn acknowledges every constituent envelope.

## mutate

Performs structural changes to the envelope without a scripting runtime. Operations are applied in order.

```toml
[[pipelines.transforms]]
type = "mutate"
on_missing = "strict"

[[pipelines.transforms.operations]]
type = "add_field"
path = "processed_at"
value = "2024-01-01T00:00:00Z"

[[pipelines.transforms.operations]]
type = "remove_field"
path = "temp_field"

[[pipelines.transforms.operations]]
type = "rename_field"
from = "old_name"
to = "new_name"

[[pipelines.transforms.operations]]
type = "cast"
path = "count"
to = "int"
```

Supported operations:

| Operation      | Fields          | Description |
| -------------- | --------------- | ----------- |
| `add_field`    | `path`, `value` | Insert or overwrite a field. Creates intermediate objects as needed. |
| `remove_field` | `path`          | Delete a field. |
| `rename_field` | `from`, `to`    | Move a value from one path to another. |
| `cast`         | `path`, `to`    | Convert a scalar to `string`, `int`, `float`, `bool`, or `json`. |

| Field        | Required | Description |
| ------------ | -------- | ----------- |
| `operations` | yes      | Array of operation tables. |
| `on_missing` | no       | `"strict"` (default) errors on missing fields; `"lenient"` skips the operation. |

Paths are dotted and relative to `payload`, for example `user.id`.

## script

Runs a user-provided script per envelope. Three runtimes are supported:

| Runtime | `runtime` value | Notes |
| ------- | --------------- | ----- |
| Rhai    | `"rhai"`        | Embedded sandboxed runtime, configurable execution budget. |
| Lua     | `"lua"`         | Embedded via `mlua`; not sandboxed (`io`, `os`, `package` exposed). Supports `max_operations` as an instruction budget. |
| Python  | `"python"`      | Runs in a `python3` subprocess; not sandboxed. |

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
| `script_file`             | one of   | Path to a script on disk. Relative paths are resolved from the config file's directory. |
| `entrypoint`              | no       | Function name to call. Defaults to `"transform"`. |
| `python_bin`              | python   | Interpreter path. Defaults to `"python3"`. |
| `max_operations`          | rhai     | Operation budget. Default `100000`. |
| `max_call_levels`         | rhai     | Call stack depth. Default `32`. |
| `max_expr_depth`          | rhai     | Expression nesting depth. Default `64`. |
| `max_function_expr_depth` | rhai     | Per-function expression depth. Default `32`. |
| `max_variables`           | rhai     | Max variables in scope. Default `64`. |

Lua and Python reject the Rhai-only limit fields rather than silently ignoring them.

See [Scripting](../../scripting/index.md) for runtime-specific guides, including the exact `env` binding shape, return semantics, and per-runtime caveats.
