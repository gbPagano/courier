---
icon: lucide/puzzle
---

# Components

Courier ships with a small set of built-in components. Every one of them is registered against the [component registry](../concepts/architecture.md#component-registry) under a short `kind` string that you reference in `config.toml`.

| Role      | Built-in `kind`s |
| --------- | ---------------- |
| Source    | `api_poll`, `kafka` |
| Transform | `set_key`, `script` |
| Sink      | `kafka` |

Each role has its own namespace, so `"kafka"` is both a source and a sink without collision.

## In this section

- **[Sources](sources.md)** — `api_poll`, `kafka`.
- **[Transforms](transforms.md)** — `set_key`, `script` (Rhai / Lua / Python).
- **[Sinks](sinks.md)** — `kafka`, plus the `ManagedSink` retry/dead-letter wrapper.

## Adding your own

External components live in their own crate and call a `register(&mut registry)` function before `Courier` is built. See [Contributing](../contributing.md) for the full plugin walkthrough.
