# Courier

[![Status: Beta](https://img.shields.io/badge/status-beta-orange.svg)](https://github.com/gbPagano/courier)
[![CI](https://github.com/gbPagano/courier/actions/workflows/ci.yml/badge.svg)](https://github.com/gbPagano/courier/actions/workflows/ci.yml)
[![Docs](https://img.shields.io/badge/docs-courier.gbpagano.dev-blue.svg)](https://courier.gbpagano.dev/)

Async Rust framework for composable data pipelines:

`Source → Transform* → Sink[]`

Each stage runs as its own Tokio task and communicates through bounded `tokio::mpsc` channels using a shared `Envelope` type. Backpressure, graceful shutdown, retries, and dead-letter handling are built in.

> **Beta:** APIs are provisional and may change without notice.

## Documentation

Full documentation lives at **<https://courier.gbpagano.dev/>**.

Jump in:

- [Quickstart](https://courier.gbpagano.dev/getting-started/quickstart/) — first pipeline in a few minutes
- [Configuration](https://courier.gbpagano.dev/configuration/) — `config.toml` reference
- [Components](https://courier.gbpagano.dev/components/) — built-in sources, transforms, sinks
- [Scripting](https://courier.gbpagano.dev/scripting/) — Rhai, Lua, and Python transforms
- [Architecture](https://courier.gbpagano.dev/architecture/) — internals and design
- [Observability](https://courier.gbpagano.dev/concepts/observability/) — logs, metrics, and traces over OTLP (sample [Collector config](examples/otel-collector.yaml), [Compose stack](examples/docker-compose.observability.yml), and [Grafana dashboard](examples/dashboards/courier.json))

## Install

```bash
cargo install data-courier --version 0.1.0-beta.4
```

This installs the `courier` binary. See the [installation guide](https://courier.gbpagano.dev/getting-started/installation/) for prerequisites (Rust toolchain, C toolchain for `rdkafka`, optional `python3` for the Python script transform) and build-from-source instructions.

## Quick example

```toml
[[pipelines]]
name = "api->kafka"

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 3

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
```

Run it:

```bash
courier run
```

Pipelines are loaded at runtime from `config.toml` (override with `COURIER_CONFIG`). Restart the binary to pick up edits — no recompile required.

## Contributing

See the [contributing guide](https://courier.gbpagano.dev/contributing/contribute/).
