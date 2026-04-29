---
icon: lucide/workflow
---

# Transforms

Transforms operate on a stream of envelopes between the source and the sinks. They are ordered — `pipelines.transforms[0]` runs first.

The simplest transform implements `MapOne::map(env) -> Option<Envelope>` (returning `None` filters the envelope out) and is wrapped in a `BasicTransform` that handles the recv loop, cancellation, and `on_error`. See [Architecture](../architecture.md#traits).

## Built-in transforms

| Kind | Description |
| ---- | ----------- |
| [`set_key`](../configuration/transforms.md#set_key) | Copies a payload field into `meta.key`. |
| [`filter`](../configuration/transforms.md#filter) | Drops envelopes that do not match a predicate expression. |
| [`batch`](../configuration/transforms.md#batch) | Groups envelopes into batches by count and/or time window. |
| [`mutate`](../configuration/transforms.md#mutate) | Adds, removes, renames, or casts fields without a scripting runtime. |
| [`script`](../configuration/transforms.md#script) | Runs a Rhai, Lua, or Python script per envelope. |

## Writing your own transform

The simplest path is to implement `MapOne` and wrap it in `BasicTransform`. For stateful transforms (batching, flat-map, background work), implement the full `Transform` trait directly. Register a `TransformFactory` against a unique `kind` — see [Development](../contributing/guides/development.md).
