---
icon: lucide/radio-tower
---

# Sources

A pipeline has exactly one source. Sources own their own cadence — they decide whether to poll, stream, or otherwise produce envelopes — and write them onto the pipeline's first mpsc channel.

## Built-in sources

| Kind | Description |
| ---- | ----------- |
| [`api_poll`](../configuration/sources.md#api_poll) | Polls an HTTP endpoint on a fixed interval. |
| [`http_webhook`](../configuration/sources.md#http_webhook) | Listens for incoming HTTP POST requests. |
| [`kafka`](../configuration/sources.md#kafka) | Consumes from a Kafka topic via `rdkafka`. |
| [`sql_query_poll`](../configuration/sources.md#sql_query_poll) | Polls a SQL query on a fixed interval, one envelope per row. |

## Writing your own source

Implement `Source::run(tx, cancel)` and register a `SourceFactory` against a unique `kind`. See [Architecture](../architecture.md) and [Development](../contributing/guides/development.md).
