# Concepts

These pages give you the mental model behind Courier's runtime. They are short by design — read them before writing your own components.

- **[Envelope](envelope.md)** — the single message type that flows between every node.
- **[Backpressure](backpressure.md)** — how bounded channels and the broadcast splitter shape end-to-end flow control.
- **[Observability](observability.md)** — structured logs, metrics, and W3C-propagated traces exported via OTLP.
- **[Lifecycle, health probes, and shutdown](lifecycle.md)** — pipeline states, health endpoints, graceful drain, and exit codes.
- **[Components](../components/index.md)** — the built-in sources, transforms, sinks, and extension points.
