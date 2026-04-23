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
- Transforms: `SetKeyTransform`
- Sinks: `KafkaSink`

## Configuration

Courier currently uses build-time code generation:

- `config.toml` defines pipelines
- `build/` parses config and generates `generated.rs`
- `src/main.rs` starts the generated `Courier`

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

## Notes

- Logging uses `env_logger`
- Shutdown uses `tokio_util::sync::CancellationToken`
- Current APIs should be treated as provisional
