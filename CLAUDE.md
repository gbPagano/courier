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

The build script generates `generated.rs` from `config.toml` at compile time — changes to `config.toml` or `build/` take effect on the next `cargo build`.

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
- **`Sink`** + **`WriteOne`** (`src/sinks/mod.rs`) — `WriteOne::write(&env)`. Wrap in `BasicSink` to expose as a `Sink`.

`BasicTransform` and `BasicSink` own the recv loop, honor the `CancellationToken`, and apply the `ErrorPolicy` (`Drop` = log and continue; `FailPipeline` = cancel the whole pipeline). Future wrappers (`RetryingSink`, `BatchingSink`, etc.) compose over `WriteOne`/`MapOne` without reimplementing the loop.

### Built-in nodes

- Sources: `ApiPollSource`, `KafkaSource`
- Transforms: `SetKeyTransform` (sets `meta.key` from a payload field)
- Sinks: `KafkaSink`

### Runtime

`spawn_pipeline` (in `src/pipeline.rs`) wires source → transforms → sinks with mpsc channels. When `sinks.len() > 1`, an implicit **broadcast splitter** is inserted that clones each envelope to every sink. Since the splitter is synchronous per sink, a slow sink applies backpressure to the whole pipeline — by design.

### Build-time code generation

`build/build.rs` reads `config.toml`, parses it via `build/config.rs`, and emits `generated.rs` through `build/codegen.rs`. The generated `courier_from_config()` function constructs the `Courier` from the declarative config. `src/main.rs` includes `generated.rs` and calls it.
