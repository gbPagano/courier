---
icon: lucide/puzzle
---

# Components

Courier ships with a small set of built-in components. Every one of them is registered against the [component registry](../concepts/architecture.md#component-registry) under a short `kind` string that you reference in `config.toml`.

| Role      | Built-in `kind`s |
| --------- | ---------------- |
| Source    | `api_poll`, `http_webhook`, `kafka`, `sql_query_poll` |
| Transform | `set_key`, `script` |
| Sink      | `api`, `file`, `kafka`, `sql` |

Each role has its own namespace, so `"kafka"` is both a source and a sink without collision.

## In this section

- **[Sources](sources.md)** — `api_poll`, `http_webhook`, `kafka`, `sql_query_poll`.
- **[Transforms](transforms.md)** — `set_key`, `script` (Rhai / Lua / Python).
- **[Sinks](sinks.md)** — `api`, `file`, `kafka`, `sql`, plus the `ManagedSink` retry/dead-letter wrapper.

## Adding your own

External components live in their own crate and call a `register(&mut registry)` function before `Courier` is built. See [Contributing](../contributing.md) for the full plugin walkthrough.
