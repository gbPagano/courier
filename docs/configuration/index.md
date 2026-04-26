---
icon: lucide/settings
---

# Configuration

Courier loads its pipeline definitions at runtime from a configuration file — there is no need to recompile when topology changes. The config file path defaults to `config.toml` in the working directory and can be overridden with the `COURIER_CONFIG` environment variable.

`COURIER_CONFIG` may point at:

- a single `.toml` or `.json` file, or
- a directory — every `.toml`/`.json` file inside it is parsed in sorted order, and their `pipelines` lists are concatenated. Duplicate pipeline names across files are rejected at load time.

Bad configuration fails at startup with a path-annotated error.

## Validation phases

Courier validates configuration after parsing and interpolation, before building runtime tasks:

1. **Load/merge validation** parses TOML or JSON, applies per-file defaults, and merges directory-mode files. Directory mode rejects duplicate pipeline names across files.
2. **Core validation** checks Courier-owned fields: non-empty and unique pipeline names, `channel_capacity > 0`, at least one sink per pipeline, non-empty component `type` values, retry bounds, and practical dead-letter path checks.
3. **Component validation** runs each registered source, transform, and sink factory. Built-ins validate their own domain rules, such as script shape, URL syntax, SQL driver/DSN pairing, Kafka topic/group fields, and webhook bind/path values.

`courier run` and `courier validate` use the same validation and build path, so a config accepted in CI is the same config shape accepted at startup. Validation does not prove that remote systems are reachable or that credentials are accepted; network connectivity, database permissions, Kafka broker availability, and receiver-side HTTP failures remain runtime checks.

## CLI checks

Courier can validate configuration and inspect the installed runtime without starting pipelines.

```bash
courier validate --config config.toml
```

`validate` loads the same config path rules as `run` (`--config`, then `COURIER_CONFIG`, then `config.toml`), parses the file or directory, registers built-ins, validates core settings, and builds the runtime graph without spawning pipelines. It exits non-zero with a path-annotated error if parsing, validation, or component construction fails. Use it as a CI or pre-deploy gate:

```yaml
- name: Validate Courier config
  run: courier validate --config ./config.toml
```

List the component kinds available in a binary:

```bash
courier list-components
```

Start pipelines with the explicit `run` command:

```bash
courier run
courier run --config config.toml
```

During local development with Cargo, pass arguments after `--`:

```bash
cargo run -- validate --config config.toml
cargo run -- list-components
cargo run -- run --config config.toml
```

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
