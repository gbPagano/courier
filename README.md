# Courier

> **Work in progress:** this project is under active development and may change without notice.

Courier is an async Rust framework for building composable data pipelines:

`Source → Transform* → Sink[]`

Each stage runs as its own Tokio task and communicates through bounded `tokio::mpsc` channels using a shared `Envelope` type.

## What it does

- Build pipelines from `config.toml`
- Connect sources, transforms, and sinks
- Support Kafka and API polling
- Fan out automatically to multiple sinks
- Apply backpressure through bounded channels
- Shut down gracefully with cancellation

## Architecture

Core runtime pieces:

- `Envelope` — shared message type for all nodes
- `Pipeline` — one source, optional transforms, one or more sinks
- `Courier` — runs all configured pipelines

## Built-in components

- Sources: `ApiPollSource`, `KafkaSource`
- Transforms: `SetKeyTransform`, `script` (Rhai)
- Sinks: `KafkaSink`

## Configuration

Pipelines are loaded at runtime from a TOML file. By default Courier reads `config.toml` from the working directory; override the path with the `COURIER_CONFIG` environment variable. Restart the binary to pick up edits — no recompile required.

Example:

```toml
[[pipelines]]
name = "api->kafka"

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 3

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
```

### Script transform

Courier includes a built-in `script` transform with multiple runtimes.

Supported config fields:
- `runtime` — required, one of `"rhai"` or `"lua"`
- `script` — inline source code for the selected runtime
- `script_file` — load source code from disk for the selected runtime
- `entrypoint` — optional, defaults to `"transform"`
- `max_operations` — Rhai-only optional execution budget, defaults to `100000`
- `max_call_levels` — Rhai-only optional, defaults to `32`
- `max_expr_depth` — Rhai-only optional, defaults to `64`
- `max_function_expr_depth` — Rhai-only optional, defaults to `32`
- `max_variables` — Rhai-only optional, defaults to `64`

The script receives an `env` object with:
- `env.meta.key`
- `env.meta.source_id`
- `env.meta.timestamp_ms`
- `env.meta.headers`
- `env.payload`

Return behavior:
- return `env` to emit a transformed envelope
- Rhai: return `()` to filter the envelope out
- Lua: return `nil` to filter the envelope out
- runtime errors follow the transform `on_error` policy

Lua rejects the Rhai-only limit fields instead of silently ignoring them.

Rhai example:

```toml
[[pipelines]]
name = "api->kafka-with-script"

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 3

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

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
```

Lua example:

```toml
[[pipelines.transforms]]
type = "script"
runtime = "lua"
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

Lua `script_file` example:

```toml
[[pipelines.transforms]]
type = "script"
runtime = "lua"
script_file = "./transforms/enrich.lua"
```

Example `transforms/enrich.lua`:

```lua
function transform(env)
  env.payload.processed = true
  return env
end
```

Example:

```toml
[[pipelines]]
name = "api->kafka-with-script"

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 3

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

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
```

## Notes

- Logging uses `env_logger`
- Shutdown uses `tokio_util::sync::CancellationToken`
- Current APIs should be treated as provisional
