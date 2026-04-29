---
icon: lucide/package
---

# Envelope

The `Envelope` is the single message type that flows between every Courier node. It lives in `src/envelope.rs` and is a small, deliberately generic shape:

```text
Envelope
├── meta: Meta
│   ├── key: Option<String>
│   ├── source_id: String
│   ├── timestamp_ms: i64
│   └── headers: HashMap<String, String>
└── payload: serde_json::Value
```

Keeping a single type at the boundary lets the runtime stay generic-free at the channel layer. The trade-off is that strongly-typed payloads are opt-in — a transform that wants typed data deserializes the `payload` into a struct, operates on it, and re-serializes on the way out.

## Field roles

| Field            | Role |
| ---------------- | ---- |
| `meta.key`       | Logical partitioning key — used by sinks like Kafka to choose the destination partition. The built-in [`set_key`](../components/transforms.md#set_key) transform sets this from a payload field. |
| `meta.source_id` | Hierarchical id of the producing node, e.g. `pipeline-name/src`. Useful for observability and for transforms that want to behave differently per source. |
| `meta.timestamp_ms` | Producer-assigned timestamp. Sources stamp it; transforms can overwrite. |
| `meta.headers`   | Free-form string headers. Conventionally used for routing hints and metadata that should not live in the payload. |
| `payload`        | Arbitrary JSON. Most transforms operate here. |

## Scripted access

Script transforms expose the same shape under whichever runtime is running. See [Scripting](../scripting/index.md) for the full table of binding shapes per runtime.
