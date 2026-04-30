---
icon: lucide/rocket
---

# Quickstart

This page walks you through a minimal pipeline that polls a public HTTP endpoint and writes each response to a Kafka topic.

## 1. Bring up Kafka

Any local Kafka broker on `localhost:9092` works. The fastest path is the official Docker Compose recipe published by Confluent or Bitnami; pick one and ensure a topic named `topic1` exists.

## 2. Write a `config.toml`

Create the file at the repository root:

```toml title="config.toml"
[[pipelines]]
name = "api->kafka"
channel_capacity = 64

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 3

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
```

A few things to notice:

- `channel_capacity` is the buffer size for every mpsc edge in the pipeline. Smaller numbers tighten [backpressure](../concepts/backpressure.md).
- `pipelines.sinks` is a list — Courier inserts a broadcast splitter automatically when there is more than one sink.
- `type = "api_poll"` and `type = "kafka"` resolve to built-in factories from the [component registry](../architecture.md#component-registry).

## 3. Run

Validate the config before starting the pipeline:

```bash
courier validate --config config.toml
```

Then run Courier:

```bash
courier run
```

You should see structured logs as each poll cycle pushes an envelope through the pipeline. Press <kbd>Ctrl</kbd>+<kbd>C</kbd> — Courier installs a SIGINT handler that drains gracefully via a shared `CancellationToken`.

## 4. Add a transform

Add a [scripted transform](../scripting/index.md) between the source and the sink to attach a header:

```toml
[[pipelines.transforms]]
type = "script"
runtime = "rhai"
script = """
fn transform(env) {
  env.meta.headers["source"] = "quickstart";
  env
}
"""
```

Restart the binary — there is no compile step.

## Next steps

- [Configuration](../configuration.md) — full schema for pipelines, error policies, and retry.
- [Components](../components/index.md) — reference for every built-in source, transform, and sink.
- [Examples](../examples/) — end-to-end recipes covering more topologies.
