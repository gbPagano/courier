---
icon: lucide/settings
---

# Configuration

Courier loads its pipeline definitions at runtime from a configuration file — there is no need to recompile when topology changes. The config file path defaults to `config.toml` in the working directory and can be overridden with the `COURIER_CONFIG` environment variable.

`COURIER_CONFIG` may point at:

- a single `.toml` or `.json` file, or
- a directory — every `.toml`/`.json` file inside it is parsed in sorted order, and their `pipelines` lists are concatenated. Duplicate pipeline names across files are rejected at load time.

Bad configuration fails at startup with a path-annotated error.

## In this section

- **[Pipelines](pipelines.md)** — the configuration schema for sources, transforms, sinks, and channels.
- **[Error Handling & Retry](error-handling.md)** — `on_error`, retry policies, and dead-letter routing.

## Minimal example

```toml title="config.toml"
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

See [Pipelines](pipelines.md) for the full set of fields.
