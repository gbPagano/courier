# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build                          # Build
cargo check                          # Fast type/borrow check
cargo clippy --all-targets           # Lint
cargo fmt                            # Format
cargo test                           # Run all tests
cargo test <test_name>               # Run a single test
```

Pipeline definitions are loaded at runtime from `config.toml` (override with the `COURIER_CONFIG` env var). `COURIER_CONFIG` may point at a single `.toml`/`.json` file or at a directory — in directory mode every `.toml`/`.json` file is parsed in sorted order and their `pipelines` concatenated. Edits take effect on binary restart — no rebuild required.

## Architecture

**Courier** is an async Rust framework for composable data pipelines. A pipeline is a DAG — `Source → Transform* → Sink[]` — connected by `tokio::mpsc` channels. The bounded channels provide backpressure naturally: a slow sink propagates upstream until the source stops pulling.

### Core types

- **`Envelope`** (`src/envelope.rs`) — the single wire type between all nodes. `Meta` (key, source_id, timestamp, headers) + `serde_json::Value` payload. Generics stop at the node boundary; strongly-typed payloads are opt-in via transforms that deserialize/re-serialize.
- **`Pipeline`** (`src/pipeline.rs`) — one source, zero or more transforms, one or more sinks, plus `channel_capacity` (mpsc buffer size per edge).
- **`Courier`** (`src/lib.rs`) — collection of pipelines. `run()` spawns each stage as its own task and installs a SIGINT/Ctrl+C handler that fires a shared `CancellationToken` for graceful drain.

### Traits

Each role has two traits: a **full-control** one that owns the channel loop, and an **ergonomic** one that handles a single item. The ergonomic side covers the common case; the full-control side is the escape hatch for stateful work (batching, flat-map, background retry).

- **`Source`** (`src/sources/mod.rs`) — `run(tx, cancel)`. No ergonomic variant: sources drive their own cadence (poll vs stream), which doesn't factor out.
- **`Transform`** + **`MapOne`** (`src/transforms/mod.rs`) — `MapOne::map(env) -> Option<Envelope>` (`None` filters). Wrap in `BasicTransform` to expose as a `Transform`.
- **`Sink`** + **`WriteOne`** (`src/sinks/mod.rs`) — `WriteOne::write(&env)`. Wrap in `ManagedSink` to expose as a `Sink`.

`BasicTransform` and `ManagedSink` own the recv loop, honor the `CancellationToken`, and apply the `ErrorPolicy` (`Drop` = log and continue; `FailPipeline` = cancel the whole pipeline). `ManagedSink` additionally supports an optional `RetryPolicy`.

### Retry and dead-letter (`src/retry.rs`)

`ManagedSink` can be configured with a `RetryPolicy` (max attempts, initial delay, multiplier, max delay). On exhaustion, `ExhaustedPolicy` decides what happens:

- `Propagate` — return the last error to `ManagedSink`, which then applies its `ErrorPolicy`.
- `DeadLetter { path }` — append the failed envelope as a JSON line to `path` and continue. A dead-letter write failure falls back to propagating the original error.

### Component registry (`src/registry.rs`)

`Registry` maps short `kind` strings (`"kafka"`, `"api_poll"`, …) to factories (`SourceFactory` / `TransformFactory` / `SinkFactory`) that build trait objects from a `serde_json::Value` spec. Each category is its own namespace, so `"kafka"` can be both a source and a sink without collision. Duplicate `kind`s within a category are rejected at registration time.

Plugin model:
1. **Built-ins** — registered via `register_builtin(&mut registry)` (or `Registry::with_builtins()`).
2. **Statically-linked plugin crates** — call the crate's own `register(&mut registry)` before building the `Courier`. This is the first-class native plugin mechanism.
3. **(Future) dynamic plugins** — factory traits are object-safe, so `libloading`/scripting is additive.

`Registry::build_courier(config)` mints hierarchical node ids (`{pipeline}/src`, `{pipeline}/t{i}`, `{pipeline}/sink{i}`) so logs and metrics trace back to the owning pipeline. Sink factories that wrap a `WriteOne` in `ManagedSink` receive the `on_error` and `retry` fields pre-extracted by the registry — no per-sink config parsing needed for those policies. Use `courier::config::parse_config` in factory implementations to get uniform "invalid config for component type '{kind}'" error messages.

### Built-in nodes

- Sources: `api_poll` (`ApiPollSource`), `kafka` (`KafkaSource`)
- Transforms: `set_key` (`SetKeyTransform`, sets `meta.key` from a payload field), `script` (Rhai runtime; set `script` inline or `script_file` to load from disk, not both)
- Sinks: `kafka` (`KafkaSink`, exposed through `ManagedSink`), `api` (`ApiSink`, HTTP push exposed through `ManagedSink`; `body = "payload"` sends `env.payload` as JSON, `body = "envelope"` sends the full envelope), `file` (`FileSink`, append-mode local file in `jsonl` or `csv` format; CSV columns are dotted paths into the envelope, e.g. `payload.id`, `meta.source_id`)

### Runtime

`spawn_pipeline` (in `src/pipeline.rs`) wires source → transforms → sinks with mpsc channels. When `sinks.len() > 1`, an implicit **broadcast splitter** is inserted that clones each envelope to every sink. Since the splitter is synchronous per sink, a slow sink applies backpressure to the whole pipeline — by design.

### Runtime config loading

`Config::load(path)` accepts either a single file (parser picked by `.toml`/`.json` extension) or a directory (every `.toml`/`.json` file merged in sorted order, duplicate pipeline names rejected). `Config::from_toml_str` and `Config::from_json_str` are the in-memory entry points. Both go through the same private `Raw*` layer that flattens arbitrary per-component fields (anything other than `type`, `on_error`, `retry`) into the component's `config: serde_json::Value` bucket; factories deserialize their own typed config through `parse_config`. TOML datetimes are stringified on the way through (no native JSON equivalent).

`src/main.rs` reads `COURIER_CONFIG` (default `config.toml`), calls `Config::load`, builds a `Registry::with_builtins()`, and hands the config to `registry.build_courier(...)`. Bad config fails at startup with path-annotated `anyhow` errors rather than blocking compilation.
