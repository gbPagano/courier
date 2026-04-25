# Sources

A pipeline has exactly one source. Sources own their own cadence — they decide whether to poll, stream, or otherwise produce envelopes — and write them onto the pipeline's first mpsc channel.

## `api_poll`

Polls an HTTP endpoint on a fixed interval and emits the response body as an envelope payload.

```toml
[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 3
```

| Field           | Required | Description |
| --------------- | -------- | ----------- |
| `url`           | yes      | The endpoint to poll. |
| `interval_secs` | yes      | Seconds between successive polls. |

The response body is parsed as JSON and used as `payload`. `meta.source_id` is set to the pipeline's source node id; `meta.timestamp_ms` is stamped at fetch time.

## `kafka`

Consumes from a Kafka topic via `rdkafka` and emits one envelope per record.

```toml
[pipelines.source]
type = "kafka"
brokers = "localhost:9092"
group_id = "courier-quickstart"
topics = ["topic1"]
```

| Field      | Required | Description |
| ---------- | -------- | ----------- |
| `brokers`  | yes      | Comma-separated bootstrap broker list. |
| `group_id` | yes      | Consumer group id. |
| `topics`   | yes      | List of topics to subscribe to. |

Record key (when present) is copied to `meta.key`; Kafka topic, partition, and offset are copied to `meta.headers["kafka.topic"]`, `meta.headers["kafka.partition"]`, and `meta.headers["kafka.offset"]`; the record value is parsed as JSON into `payload`.

## Writing your own source

Implement `Source::run(tx, cancel)` and register a `SourceFactory` against a unique `kind`. See [Architecture](../concepts/architecture.md) and [Contributing](../contributing.md).
