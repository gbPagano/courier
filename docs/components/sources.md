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

## `http_webhook`

Listens for incoming HTTP requests and emits one envelope per accepted request.

```toml
[pipelines.source]
type = "http_webhook"
bind = "0.0.0.0:8080"
path = "/webhooks/events"
```

| Field  | Required | Description |
| ------ | -------- | ----------- |
| `bind` | yes      | Socket address to listen on, for example `"0.0.0.0:8080"` or `"127.0.0.1:9000"`. |
| `path` | yes      | Exact request path to accept. Must start with `/`. |

Only `POST` requests are accepted. The request body must be valid JSON. Courier parses the raw JSON request body and uses it directly as `payload`; it does not wrap the body in an additional object. Request headers with UTF-8 values are copied into `meta.headers` as `http.header.<header-name>`, using the lower-case header name as normalized by the HTTP stack. `meta.source_id` is set to the pipeline's source node id and `meta.timestamp_ms` is stamped when the request is accepted.

Invalid requests return client errors without emitting an envelope:

| Case | Response |
| ---- | -------- |
| Wrong path | `404 Not Found` |
| Non-POST method | `405 Method Not Allowed` |
| Invalid JSON body | `400 Bad Request` |

The source sends the envelope into the pipeline channel before responding `202 Accepted`, so a full downstream channel applies backpressure to the HTTP request. If the pipeline is no longer accepting events, the source returns `503 Service Unavailable`.

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
