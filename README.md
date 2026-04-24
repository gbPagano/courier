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

Courier includes a built-in `script` transform powered by **Rhai**.

Supported config fields:
- `runtime` — currently only `"rhai"`
- `script` — inline Rhai source code
- `entrypoint` — optional, defaults to `"transform"`
- `max_operations` — optional execution budget, defaults to `100000`
- `max_call_levels` — optional, defaults to `32`
- `max_expr_depth` — optional, defaults to `64`
- `max_function_expr_depth` — optional, defaults to `32`
- `max_variables` — optional, defaults to `64`

The script receives an `env` object with:
- `env.meta.key`
- `env.meta.source_id`
- `env.meta.timestamp_ms`
- `env.meta.headers`
- `env.payload`

Return behavior:
- return `env` to emit a transformed envelope
- return `()` to filter the envelope out
- runtime errors follow the transform `on_error` policy

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
