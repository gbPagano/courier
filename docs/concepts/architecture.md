# Architecture

A Courier **pipeline** is a directed acyclic graph:

```text
Source → Transform* → Sink[]
```

Each node runs as its own Tokio task and communicates with its neighbors through bounded `tokio::mpsc` channels. The runtime has no global scheduler — once `Courier::run()` spawns the tasks, flow control is governed entirely by channel capacity.

## Core types

| Type        | Lives in              | Role |
| ----------- | --------------------- | ---- |
| `Envelope`  | `src/envelope.rs`     | Single wire type between nodes — see [Envelope](envelope.md). |
| `Pipeline`  | `src/pipeline.rs`     | One source, zero or more transforms, one or more sinks, plus a `channel_capacity`. |
| `Courier`   | `src/lib.rs`          | Collection of pipelines. `run()` spawns the tasks and installs a SIGINT handler that fires a shared `CancellationToken`. |

Generics stop at the node boundary. Strongly-typed payloads are opt-in: a transform can deserialize from `serde_json::Value`, do its work in a typed struct, and re-serialize on the way out.

## Traits

Each role has two traits — a **full-control** one that owns the channel loop, and an **ergonomic** one that handles a single item. The ergonomic side covers the common case; the full-control side is the escape hatch for stateful work like batching, flat-map, or background retry.

| Role      | Full-control trait | Ergonomic trait | Wrapper |
| --------- | ------------------ | ---------------- | ------- |
| Source    | `Source`           | —                | — (sources drive their own cadence; poll vs stream doesn't factor out). |
| Transform | `Transform`        | `MapOne`         | `BasicTransform` |
| Sink      | `Sink`             | `WriteOne`       | `ManagedSink` |

`MapOne::map(env) -> Result<Option<Envelope>>` makes filtering implicit — return `Ok(None)` to drop. `WriteOne::write(&env)` keeps sinks focused on the side effect.

`BasicTransform` and `ManagedSink` own the recv loop, honor the `CancellationToken`, and apply the configured [`ErrorPolicy`](../configuration/error-handling.md). `ManagedSink` additionally drives the optional retry policy and the dead-letter destination.

## Component registry

`Registry` maps short `kind` strings (`"kafka"`, `"api_poll"`, `"script"`, …) to factories that build trait objects from a `serde_json::Value` spec. Each role has its own namespace, so `"kafka"` can be both a source and a sink without collision. Duplicate `kind`s within a role are rejected at registration time.

The plugin model has three tiers:

1. **Built-ins** — registered via `register_builtin(&mut registry)` (or `Registry::with_builtins()`).
2. **Statically-linked plugin crates** — call the crate's own `register(&mut registry)` before building the `Courier`. This is the first-class native plugin mechanism today.
3. **(Future) dynamic plugins** — the factory traits are object-safe, so `libloading` or scripting layers are additive.

`Registry::build_courier(config)` mints hierarchical node ids of the form `{pipeline}/src`, `{pipeline}/t{i}`, `{pipeline}/sink{i}` so log lines and metrics trace back to the owning pipeline. Sink factories that wrap a `WriteOne` in `ManagedSink` receive `on_error` and `retry` pre-extracted by the registry — they never parse those fields themselves.

## Runtime

`spawn_pipeline` (in `src/pipeline.rs`) wires `source → transforms → sinks` with bounded mpsc channels. When `sinks.len() > 1`, an implicit broadcast splitter is inserted that clones each envelope to every sink. The splitter is synchronous per sink — a slow sink applies backpressure to the whole pipeline by design. See [Backpressure](backpressure.md).

`Courier::run()`:

1. Spawns each stage of each pipeline as a Tokio task.
2. Installs a SIGINT/Ctrl+C handler that fires the shared `CancellationToken`.
3. Awaits graceful drain.
