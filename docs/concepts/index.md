---
icon: lucide/network
---

# Concepts

These pages give you the mental model behind Courier's runtime. They are short by design — read them before writing your own components.

- **[Architecture](architecture.md)** — how a pipeline is wired up, the role of each trait, and how the registry resolves component kinds.
- **[Envelope](envelope.md)** — the single message type that flows between every node.
- **[Backpressure](backpressure.md)** — how bounded channels and the broadcast splitter shape end-to-end flow control.
