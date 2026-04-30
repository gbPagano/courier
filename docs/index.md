---
template: home.html
title: Courier — async Rust pipelines
hide:
  - navigation
  - toc
---

## What it does

- Build pipelines declaratively from a TOML or JSON config file
- Connect sources, transforms, and sinks through bounded channels
- Fan out automatically to multiple sinks with a built-in broadcast splitter
- Apply [backpressure](concepts/backpressure.md) end-to-end through bounded channels
- Retry failed sink writes with exponential backoff and a dead-letter destination
- Embed scripted [transforms](scripting/index.md) in Rhai, Lua, or Python
- Emit structured logs, metrics, and W3C-propagated traces over [OpenTelemetry](concepts/observability.md) (OTLP)
- Shut down gracefully on SIGINT via a shared `CancellationToken`

## Where to start

<div class="grid cards" markdown>

- :material-clock-fast: **[Getting Started](getting-started/index.md)** — install Courier and run your first pipeline.
- :material-cog: **[Configuration](configuration.md)** — write `config.toml` files and tune error handling.
- :material-graph-outline: **[Architecture](architecture.md)** — understand the runtime, components, envelope, and backpressure.
- :material-script-text: **[Scripting](scripting/index.md)** — write transforms in Rhai, Lua, or Python.
- :material-book-open-page-variant: **[Examples](examples/)** — end-to-end pipeline recipes.

</div>

## Project status

Courier is at **v0.1.0-beta.1**. Treat the public API, configuration schema, and on-disk formats (including the dead-letter file format) as provisional. Breaking changes will be called out in the [contributing guide](contributing/contribute/) and release notes.
