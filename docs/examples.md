---
icon: lucide/book-open
---

# Examples

End-to-end pipeline recipes. Drop them into a `config.toml` and run `cargo run` — no recompile required between edits.

## API → Kafka

The simplest useful pipeline. Polls an HTTP endpoint and forwards the response to a Kafka topic.

```toml
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

## API → Kafka, with a partition key

Use the [`set_key`](components/transforms.md#set_key) transform to partition records by a payload field.

```toml
[[pipelines]]
name = "api->kafka-keyed"

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 3

[[pipelines.transforms]]
type = "set_key"
from_field = "userId"

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
```

## API → Kafka, with a Rhai transform

Tag envelopes from a specific user as high-priority.

```toml
[[pipelines]]
name = "api->kafka-rhai"

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

## API → HTTP webhook

Forward polled events to an external HTTP endpoint. By default the [`api`](components/sinks.md#api) sink POSTs `env.payload` as the JSON body.

```toml
[[pipelines]]
name = "api->webhook"

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 5

[[pipelines.sinks]]
type = "api"
url = "https://internal.example.com/webhooks/posts"
method = "POST"
headers = { Authorization = "Bearer ${API_TOKEN}" }
```

Set `body = "envelope"` if the receiver also needs `meta`. Non-2xx responses become sink errors and flow through the configured `on_error` and `retry` policies — the [retry/dead-letter recipe](#sink-with-retry-and-dead-letter) below applies unchanged.

## Fan-out to multiple sinks

When you list more than one sink, Courier inserts an implicit broadcast splitter that clones each envelope to every sink. The splitter is synchronous per sink — see [Backpressure](concepts/backpressure.md).

```toml
[[pipelines]]
name = "api->fanout"

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 3

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1-mirror"
```

## Sink with retry and dead-letter

Retry transient failures with exponential backoff; persist exhausted envelopes to a JSON-lines file.

```toml
[[pipelines]]
name = "api->kafka-retry"

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 3

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
kind = "dead_letter"
path = "./dlq.jsonl"
```

## Lua transform from a file

Keep the script in version-controlled source instead of inline TOML:

```toml
[[pipelines.transforms]]
type = "script"
runtime = "lua"
script_file = "./transforms/enrich.lua"
```

```lua title="transforms/enrich.lua"
function transform(env)
  env.payload.processed = true
  return env
end
```

## Python transform with a virtualenv

Point `python_bin` at a virtualenv if your script needs third-party packages.

```toml
[[pipelines.transforms]]
type = "script"
runtime = "python"
script_file = "./transforms/enrich.py"
python_bin = "./.venv/bin/python"
```

```python title="transforms/enrich.py"
import sys

def transform(env):
    print(f"processing key={env['meta']['key']}", file=sys.stderr)
    env["payload"]["processed"] = True
    return env
```
