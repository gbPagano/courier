---
icon: lucide/radio-tower
---

# Source configuration

A pipeline has exactly one source. The source owns the input cadence and writes envelopes onto the pipeline's first channel. Use the table to jump to the configuration reference for each supported source.

| Kind | Use when |
| ---- | -------- |
| [`api_poll`](#api_poll) | Polling an HTTP endpoint on a fixed interval. |
| [`http_webhook`](#http_webhook) | Receiving incoming HTTP POST requests. |
| [`kafka`](#kafka) | Consuming records from a Kafka topic. |
| [`sql_query_poll`](#sql_query_poll) | Polling a SQL query and emitting one envelope per row. |

## api_poll

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

## http_webhook

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

## kafka

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

Record key, when present, is copied to `meta.key`; Kafka topic, partition, and offset are copied to `meta.headers["kafka.topic"]`, `meta.headers["kafka.partition"]`, and `meta.headers["kafka.offset"]`; the record value is parsed as JSON into `payload`.

## sql_query_poll

Polls a SQL query on a fixed interval and emits one envelope per returned row. Supports `postgres` and `sqlite` drivers.

```toml
[pipelines.source]
type = "sql_query_poll"
driver = "postgres"
dsn = "postgres://user:pass@localhost/app"
query = "SELECT id, email, updated_at FROM users ORDER BY updated_at"
poll_interval_secs = 30
```

SQLite uses the same shape:

```toml
[pipelines.source]
type = "sql_query_poll"
driver = "sqlite"
dsn = "sqlite:///var/lib/app.db"
query = "SELECT id, email FROM users ORDER BY id"
poll_interval_secs = 30
```

| Field                | Required | Description |
| -------------------- | -------- | ----------- |
| `driver`             | yes      | `postgres` or `sqlite`. |
| `dsn`                | yes      | Driver-specific connection string. |
| `query`              | yes      | SQL query to execute every poll. Must not be empty. |
| `poll_interval_secs` | yes      | Seconds between successive polls. Must be greater than `0`. |

Each returned row becomes an envelope whose `payload` is a JSON object keyed by column name. SQL booleans, integers, floats, JSON, text, and common timestamp types are converted to JSON values. `meta.source_id` is set to the pipeline's source node id and `meta.timestamp_ms` is stamped when the row is emitted.

Polling is stateless. Courier does not persist checkpoints or remember the last row across restarts. If you need incremental behavior, put the filtering in your SQL query or query a table/view that already represents the desired window.
