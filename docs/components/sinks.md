---
icon: lucide/send
---

# Sinks

A pipeline has one or more sinks. With more than one sink, Courier inserts an implicit broadcast splitter — every envelope is cloned to every sink, and the splitter is synchronous per sink (a slow sink applies backpressure to the whole pipeline). See [Backpressure](../concepts/backpressure.md).

The simplest sink implements `WriteOne::write(&env)` and is wrapped in `ManagedSink`, which owns the recv loop, honors the `CancellationToken`, and applies the configured [`on_error`](../configuration/error-handling.md) and [retry](../configuration/error-handling.md#retry-on-sinks) policies.

## Built-in sinks

| Kind | Description |
| ---- | ----------- |
| [`api`](../configuration/sinks.md#api) | Sends each envelope to an HTTP endpoint as a JSON request. |
| [`file`](../configuration/sinks.md#file) | Appends each envelope to a local file in JSONL or CSV format. |
| [`kafka`](../configuration/sinks.md#kafka) | Produces records to a Kafka topic via `rdkafka`. |
| [`sql`](../configuration/sinks.md#sql) | Inserts one row per envelope into a SQL table. |

## Writing your own sink

Implement `WriteOne` for the simple case — Courier wraps it in `ManagedSink` and you get retry, dead-letter, and `on_error` for free. For sinks that batch or maintain background connections, implement the full `Sink` trait directly. Register a `SinkFactory` against a unique `kind` — see [Development](../contributing/guides/development.md).
