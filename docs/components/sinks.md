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

## `file`

Appends each envelope to a local file as a single line of JSON or a single CSV row. Useful for local development, debugging, audits, and lightweight ETL.

```toml
[[pipelines.sinks]]
type = "file"
path = "./out/users.jsonl"
format = "jsonl"   # default
body = "payload"   # default — only meaningful for jsonl
```

```toml
[[pipelines.sinks]]
type = "file"
path = "./out/users.csv"
format = "csv"
columns = ["payload.id", "payload.name", "meta.source_id"]
```

| Field     | Required        | Default     | Description |
| --------- | --------------- | ----------- | ----------- |
| `path`    | yes             | —           | Output file path. Parent directories are created automatically. |
| `format`  | no              | `"jsonl"`   | `"jsonl"` or `"csv"`. |
| `body`    | no (jsonl only) | `"payload"` | `"payload"` writes only `env.payload`; `"envelope"` writes the full envelope (`{ "meta": …, "payload": … }`). Ignored for CSV. |
| `columns` | yes for csv     | `[]`        | Dotted paths evaluated against the full envelope (`payload.id`, `meta.source_id`, `meta.headers.priority`, …). Required when `format = "csv"`. |

The file is opened in **append mode** and flushed after every write, so a restart resumes cleanly without truncating prior output. For CSV the header row is written only when the file is empty at open time — a restart against an existing file emits more rows but no extra header.

CSV cells follow RFC 4180: fields containing `,`, `"`, `\n`, or `\r` are quoted, and embedded `"` characters are doubled. Missing columns render as empty cells; nested objects and arrays are JSON-encoded so they remain machine-readable.

Streaming writes are not transactional. A retried write after a partial I/O failure may produce a duplicate row — the same trade-off that applies to every `WriteOne`-based sink.

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

## `sql`

Inserts one row per envelope into a SQL table. The public connector is generic over SQL drivers; this version supports `postgres` and `sqlite`.

```toml
[[pipelines.sinks]]
type = "sql"
driver = "postgres"
dsn = "postgres://user:pass@localhost/warehouse"
table = "users_snapshot"
mode = "insert"
columns = {
  id = "payload.id",
  email = "payload.email",
  updated_at = "payload.updated_at",
  source = "meta.source_id"
}
```

| Field     | Required | Default    | Description |
| --------- | -------- | ---------- | ----------- |
| `driver`  | yes      | —          | `postgres` or `sqlite`. |
| `dsn`     | yes      | —          | Driver-specific connection string. |
| `table`   | yes      | —          | Destination table. Simple identifiers and dotted schema-qualified names are supported. |
| `mode`    | no       | `"insert"` | Write mode. Only `insert` is included in this version. |
| `columns` | yes      | —          | Map of destination column name to dotted path in the full envelope. |

Column mappings are evaluated against the full envelope, so paths can read from `payload`, `meta.source_id`, `meta.key`, `meta.timestamp_ms`, or `meta.headers.*`. Missing paths are inserted as SQL `NULL`. Scalar JSON values are bound as native booleans, numbers, or strings where possible; arrays and objects are bound as JSON for Postgres and JSON strings for SQLite.

Upsert is not included in this first version. A future API should define conflict columns and update columns explicitly, because the SQL syntax and behavior differ by driver.

This sink implements `WriteOne` and is wrapped in `ManagedSink`, so non-transient database errors, retry, dead-letter, and `on_error` behavior are handled the same way as other built-in sinks.

## Retry & dead-letter

`on_error` and the `retry` block are extracted by the registry and wired into `ManagedSink` automatically — sink factories never parse those fields themselves. See [Error Handling & Retry](../configuration/error-handling.md) for the full schema.

## Writing your own sink

Implement `WriteOne` for the simple case — Courier wraps it in `ManagedSink` and you get retry, dead-letter, and `on_error` for free. For sinks that batch or maintain background connections, implement the full `Sink` trait directly. Register a `SinkFactory` against a unique `kind` — see [Contributing](../contributing.md).
