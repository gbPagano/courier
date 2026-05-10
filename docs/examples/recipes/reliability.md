---
icon: lucide/shield-alert
---

# Reliability examples

Use retry, dead-letter, and fan-out behavior when a pipeline needs to keep useful work moving despite slow or failing sinks.

## API to file and Kafka

Write the same envelope to a local CSV file and a Kafka topic. Courier inserts a broadcast splitter automatically when a pipeline has multiple sinks.

```toml
[[pipelines]]
name = "api->csv+kafka"

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/users/1"
interval_secs = 10

[[pipelines.sinks]]
type = "file"
path = "./out/users.csv"
format = "csv"
columns = ["payload.id", "payload.name", "payload.email", "meta.source_id"]

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
```

See [Backpressure](../../concepts/backpressure.md) for how a slow sink affects the rest of the pipeline.

## Sink retry with dead-letter

Retry transient sink failures with exponential backoff and write exhausted envelopes to a JSONL dead-letter file.

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

Use `dead_letter` when an operator needs to inspect failed envelopes. Re-ingest them later using `courier replay-dlq ./dlq.jsonl -c config.toml`, which strips transforms (envelopes have already been transformed), routes entries only to the sink that originally failed, and neutralizes any dead-letter path that would write back into the replay source file.

The `jsonl_file` source can also be used directly in a pipeline config for more control over replay:

```toml
[[pipelines]]
name = "replay"

[pipelines.source]
type = "jsonl_file"
path = "./dlq.jsonl"

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
```

Each dead-letter line contains the full envelope alongside routing metadata (`pipeline`, `sink`), a timestamp (`dead_lettered_at_ms`), and the retry count (`attempts`), so operators can filter and route entries before replay.
