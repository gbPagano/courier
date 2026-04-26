# Sinks

A pipeline has one or more sinks. With more than one sink, Courier inserts an implicit broadcast splitter — every envelope is cloned to every sink, and the splitter is synchronous per sink (a slow sink applies backpressure to the whole pipeline). See [Backpressure](../concepts/backpressure.md).

The simplest sink implements `WriteOne::write(&env)` and is wrapped in `ManagedSink`, which owns the recv loop, honors the `CancellationToken`, and applies the configured [`on_error`](../configuration/error-handling.md) and [retry](../configuration/error-handling.md#retry-on-sinks) policies.

## `api`

Sends each envelope to an HTTP endpoint as a JSON request. Useful for webhook integrations, REST forwarding, and posting to internal services.

```toml
[[pipelines.sinks]]
type = "api"
url = "https://internal.example.com/webhooks/users"
method = "POST"            # default
body = "payload"           # default — send only env.payload
headers = { Authorization = "Bearer token" }
timeout_secs = 30          # optional
```

| Field          | Required | Default     | Description |
| -------------- | -------- | ----------- | ----------- |
| `url`          | yes      | —           | Endpoint to send the request to. |
| `method`       | no       | `"POST"`    | Any HTTP method understood by `reqwest::Method` (`POST`, `PUT`, `PATCH`, `DELETE`, …). |
| `headers`      | no       | `{}`        | String map appended to every request. |
| `body`         | no       | `"payload"` | `"payload"` sends `env.payload` as the JSON body; `"envelope"` sends the full envelope (`{ "meta": …, "payload": … }`). |
| `timeout_secs` | no       | none        | Per-request timeout. Omit to use the underlying `reqwest` default (no timeout). |

Any non-2xx response, network error, or timeout is reported as a sink failure and flows through the configured [`on_error`](../configuration/error-handling.md) and [retry](../configuration/error-handling.md#retry-on-sinks) policies. The response body, when present on a failure, is included in the error message so it surfaces in logs and dead-letter entries.

## `kafka`

Produces records to a Kafka topic via `rdkafka`.

```toml
[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
on_error = "drop"

[pipelines.sinks.retry]
max_attempts = 5
initial_delay_ms = 100
backoff_multiplier = 2.0
max_delay_ms = 5000

[pipelines.sinks.retry.on_exhausted]
kind = "propagate"
```

| Field     | Required | Description |
| --------- | -------- | ----------- |
| `brokers` | yes      | Comma-separated bootstrap broker list. |
| `topic`   | yes      | Destination topic. |

`meta.key` is used as the record key; `payload` is serialized as the record value.

## Retry & dead-letter

`on_error` and the `retry` block are extracted by the registry and wired into `ManagedSink` automatically — sink factories never parse those fields themselves. See [Error Handling & Retry](../configuration/error-handling.md) for the full schema.

## Writing your own sink

Implement `WriteOne` for the simple case — Courier wraps it in `ManagedSink` and you get retry, dead-letter, and `on_error` for free. For sinks that batch or maintain background connections, implement the full `Sink` trait directly. Register a `SinkFactory` against a unique `kind` — see [Contributing](../contributing.md).
