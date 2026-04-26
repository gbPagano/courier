---
icon: lucide/settings
---

# Configuration

Courier loads its pipeline definitions at runtime from a configuration file — there is no need to recompile when topology changes. The config file path defaults to `config.toml` in the working directory and can be overridden with the `COURIER_CONFIG` environment variable.

`COURIER_CONFIG` may point at:

- a single `.toml` or `.json` file, or
- a directory — every `.toml`/`.json` file inside it is parsed in sorted order, and their `pipelines` lists are concatenated. Duplicate pipeline names across files are rejected at load time.

Bad configuration fails at startup with a path-annotated error.

## CLI checks

Courier can validate configuration and inspect the installed runtime without starting pipelines.

```bash
courier validate --config config.toml
```

`validate` loads the same config path rules as `run` (`--config`, then `COURIER_CONFIG`, then `config.toml`), parses the file or directory, registers built-ins, and builds the runtime graph without spawning pipelines. It exits non-zero with a path-annotated error if parsing or component construction fails. Use it as a CI or pre-deploy gate:

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
