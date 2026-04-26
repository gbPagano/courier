# Courier

[![Status: Beta](https://img.shields.io/badge/status-beta-orange.svg)](https://github.com/gbPagano/courier)
[![CI](https://github.com/gbPagano/courier/actions/workflows/ci.yml/badge.svg)](https://github.com/gbPagano/courier/actions/workflows/ci.yml)
[![Docs](https://img.shields.io/badge/docs-gbpagano.github.io%2Fcourier-blue.svg)](https://gbpagano.github.io/courier/)

Async Rust framework for composable data pipelines:

`Source → Transform* → Sink[]`

Each stage runs as its own Tokio task and communicates through bounded `tokio::mpsc` channels using a shared `Envelope` type. Backpressure, graceful shutdown, retries, and dead-letter handling are built in.

> **Beta:** APIs are provisional and may change without notice.

## Documentation

Full documentation lives at **<https://gbpagano.github.io/courier/>**.

Jump in:

- [Quickstart](https://gbpagano.github.io/courier/getting-started/quickstart/) — first pipeline in a few minutes
- [Configuration](https://gbpagano.github.io/courier/configuration/) — `config.toml` reference
- [Components](https://gbpagano.github.io/courier/components/) — built-in sources, transforms, sinks
- [Scripting](https://gbpagano.github.io/courier/scripting/) — Rhai, Lua, and Python transforms
- [Architecture](https://gbpagano.github.io/courier/concepts/architecture/) — internals and design

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
cargo run -- run
```

Pipelines are loaded at runtime from `config.toml` (override with `COURIER_CONFIG`). Restart the binary to pick up edits — no recompile required.

## Contributing

See the [contributing guide](https://gbpagano.github.io/courier/contributing/).
