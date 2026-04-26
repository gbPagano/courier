# Pipeline configuration

A configuration file declares a list of pipelines. Each pipeline has exactly one **source**, zero or more **transforms** in order, and one or more **sinks**.

## Top-level shape

```toml
[[pipelines]]
name = "api->kafka"          # required, must be unique across the whole config
channel_capacity = 64        # optional, default applied per edge

[pipelines.source]           # required, exactly one
type = "api_poll"
# ...source-specific fields

[[pipelines.transforms]]     # optional, ordered
type = "set_key"
# ...transform-specific fields

[[pipelines.sinks]]          # required, at least one
type = "kafka"
# ...sink-specific fields
```

| Field              | Required | Description |
| ------------------ | -------- | ----------- |
| `name`             | yes      | Non-empty unique identifier; appears in log/metric node ids. |
| `channel_capacity` | no       | Buffer size for each `tokio::mpsc` edge inside the pipeline. Must be greater than `0`. Smaller values tighten [backpressure](../concepts/backpressure.md). |
| `source`           | yes      | Exactly one source. See [Sources](../components/sources.md). |
| `transforms`       | no       | Ordered list of transforms. See [Transforms](../components/transforms.md). |
| `sinks`            | yes      | One or more sinks. With more than one, Courier inserts a broadcast splitter — every envelope is cloned to every sink, and a slow sink applies backpressure to the whole pipeline. |

## Component shape

Every source, transform, and sink shares this shape:

```toml
type = "<kind>"              # required — looked up in the component registry
on_error = "drop"            # optional, transforms and sinks only — see error handling
# (sinks only) optional retry table
[<component>.retry]
max_attempts = 5
initial_delay_ms = 100
backoff_multiplier = 2.0
max_delay_ms = 5000

[<component>.retry.on_exhausted]
kind = "propagate"           # or "dead_letter" with a `path`

# any remaining fields are passed to the component-specific factory
```

The `type` field is matched against the registered `kind` in the [component registry](../concepts/architecture.md#component-registry). Each category (source / transform / sink) has its own namespace, so `"kafka"` can be both a source and a sink without collision.

## Multiple files (directory mode)

When `COURIER_CONFIG` points at a directory, Courier reads every `.toml`/`.json` file in sorted order and concatenates their `pipelines` lists. This makes it straightforward to keep one pipeline per file:

```text
pipelines.d/
├── 10-orders.toml
├── 20-events.toml
└── 90-debug.json
```

Duplicate pipeline names across files are rejected at load time. Duplicate names inside one parsed config are rejected by the core validation pass.

## JSON form

The same schema works in JSON. TOML datetimes are stringified internally so the two formats accept the same component fields:

```json title="pipelines.d/10-orders.json"
{
  "pipelines": [
    {
      "name": "api->kafka",
      "source": {
        "type": "api_poll",
        "url": "https://jsonplaceholder.typicode.com/posts/1",
        "interval_secs": 3
      },
      "sinks": [
        { "type": "kafka", "brokers": "localhost:9092", "topic": "topic1" }
      ]
    }
  ]
}
```

Continue to [Error Handling & Retry](error-handling.md) for `on_error` and retry semantics.
