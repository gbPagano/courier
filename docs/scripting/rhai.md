# Rhai

[Rhai](https://rhai.rs) is a small embedded scripting language for Rust. It runs in-process, supports a configurable execution budget, and is the default choice for short transforms.

## Minimal example

```toml
[[pipelines.transforms]]
type = "script"
runtime = "rhai"
on_error = "drop"
script = """
fn transform(env) {
  if env.payload["userId"] == 1 {
    env.meta.headers["priority"] = "high";
  }

  env.payload["processed"] = true;
  env
}
"""
```

## `script_file`

Load the script from disk instead of inlining it:

```toml
[[pipelines.transforms]]
type = "script"
runtime = "rhai"
script_file = "./transforms/enrich.rhai"
```

`script` and `script_file` are mutually exclusive — set exactly one.

## Return semantics

- `return env` — emit the (possibly mutated) envelope downstream.
- `return ()` (or no return value) — filter the envelope out.

## Execution limits

Rhai exposes a number of safety knobs. All are optional and have sensible defaults:

| Field                     | Default  | Purpose |
| ------------------------- | -------- | ------- |
| `max_operations`          | `100000` | Operation budget per call. Prevents infinite loops. |
| `max_call_levels`         | `32`     | Max function call depth. |
| `max_expr_depth`          | `64`     | Max expression nesting in top-level scope. |
| `max_function_expr_depth` | `32`     | Max expression nesting inside function bodies. |
| `max_variables`           | `64`     | Max number of variables in scope. |

A budget exhaustion is reported as a runtime error and follows the transform's `on_error` policy.

These fields are Rhai-only. Setting them under `runtime = "lua"` or `runtime = "python"` is rejected at config-load time.

## `env` binding

| Field                    | Access                  |
| ------------------------ | ----------------------- |
| Logical key              | `env.meta.key`          |
| Source node id           | `env.meta.source_id`    |
| Producer timestamp (ms)  | `env.meta.timestamp_ms` |
| Headers map              | `env.meta.headers`      |
| Payload                  | `env.payload`           |

`env.payload` is the parsed JSON value — index into it with the usual `[]` syntax.
