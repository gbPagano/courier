---
icon: lucide/route
---

# Pipeline examples

These recipes show complete source-to-sink pipelines with minimal extra behavior.

## API to Kafka

Poll an HTTP endpoint and forward the response payload to Kafka.

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

## Webhook to Kafka

Accept JSON webhook requests and publish the request body to Kafka.

```toml
[[pipelines]]
name = "webhook->kafka"

[pipelines.source]
type = "http_webhook"
bind = "0.0.0.0:8080"
path = "/webhooks/events"

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "incoming-events"
```

Requests must use `POST` on the configured path, and the body must be valid JSON.

## API to local CSV file

Poll a JSON API and project selected fields into a local CSV file. Courier writes the header when the file is created and appends rows on later runs.

```toml
[[pipelines]]
name = "api->csv"

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/users/1"
interval_secs = 10

[[pipelines.sinks]]
type = "file"
path = "./out/users.csv"
format = "csv"
columns = ["payload.id", "payload.name", "payload.email", "meta.source_id"]
```
