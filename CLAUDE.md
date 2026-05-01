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

The crate is published as `data-courier`; the library and binary are both named `courier`. The `courier` binary is a `clap` CLI with three subcommands:

- `courier run [-c <path>]` — load config, install observability, spawn pipelines, block until SIGINT/Ctrl+C.
- `courier validate [-c <path>]` — load config and `dry_run_build` (registry checks every factory) without running anything; file sinks are not opened.
- `courier list-components` — print every registered source / transform / sink `kind`.

Pipeline definitions are loaded at runtime from `config.toml` (override with `-c` or the `COURIER_CONFIG` env var). `COURIER_CONFIG` may point at a single `.toml`/`.json` file or at a directory — in directory mode every `.toml`/`.json` file is parsed in sorted order and their `pipelines` concatenated. Edits take effect on binary restart — no rebuild required.

## Architecture

**Courier** is an async Rust framework for composable data pipelines. A pipeline is a DAG — `Source → Transform* → Sink[]` — connected by `tokio::mpsc` channels. The bounded channels provide backpressure naturally: a slow sink propagates upstream until the source stops pulling.

### Core types

- **`Envelope`** (`src/envelope.rs`) — the single wire type between all nodes. `Meta` (key, source_id, timestamp, headers) + `serde_json::Value` payload. Generics stop at the node boundary; strongly-typed payloads are opt-in via transforms that deserialize/re-serialize. W3C `traceparent` / `tracestate` ride in `Meta.headers` so spans cross the channel boundary.
- **`Pipeline`** (`src/pipeline.rs`) — one source, zero or more transforms, one or more sinks, plus `channel_capacity` (mpsc buffer size per edge) and an optional `ObsHandle`.
- **`Courier`** (`src/lib.rs`) — collection of pipelines plus parsed `ObservabilityConfig`. `run()` spawns each stage as its own task and installs a SIGINT/Ctrl+C handler that fires a shared `CancellationToken` and force-flushes the metrics / traces / logs providers before exit.

### Traits

Each role has two traits: a **full-control** one that owns the channel loop, and an **ergonomic** one that handles a single item. The ergonomic side covers the common case; the full-control side is the escape hatch for stateful work (batching, flat-map, background retry).

- **`Source`** (`src/sources/mod.rs`) — `run(tx, cancel)`. No ergonomic variant: sources drive their own cadence (poll vs stream), which doesn't factor out.
- **`Transform`** + **`MapOne`** (`src/transforms/mod.rs`) — `MapOne::map(env) -> Option<Envelope>` (`None` filters). Wrap in `BasicTransform` to expose as a `Transform`.
- **`Sink`** + **`WriteOne`** (`src/sinks/mod.rs`) — `WriteOne::write(&env)`. Wrap in `ManagedSink` to expose as a `Sink`.

`BasicTransform` and `ManagedSink` own the recv loop, honor the `CancellationToken`, and apply the `ErrorPolicy` (`Drop` = log and continue; `FailPipeline` = cancel the whole pipeline). `ManagedSink` additionally supports an optional `RetryPolicy`. Every node trait also has a `set_node_ctx(NodeCtx)` hook the runtime calls between build and run; managed wrappers store it for metrics/spans, custom impls can leave it as the default no-op.

### Retry

Sinks and polling sources both use `RetryPolicy` (`src/retry.rs`: max attempts, initial delay, multiplier, max delay) but apply it differently — they have different things to decide.

- **Sinks (`ManagedSink`)** — on exhaustion, `ExhaustedPolicy` decides:
  - `Propagate` — return the last error, then `ErrorPolicy` applies.
  - `DeadLetter { path }` — append the failed envelope as a JSON line to `path` and continue. A dead-letter write failure falls back to propagating the original error.
- **Polling sources (`src/sources/retry.rs`)** — there's no envelope to dead-letter and no downstream to propagate to, so retry only decides *when* to attempt the next poll. The wait is `min(interval, backoff_for_failure_n)`: `interval` is a ceiling, retry can only schedule sooner, never later. After `max_attempts` failures the scheduler falls back to `interval` until the next success resets the counter. This is why source factories accept `Option<RetryPolicy>` while push-based sources (`kafka`, `http_webhook`) reject it at factory time — retry would be meaningless there.

### Component registry (`src/registry.rs`)

`Registry` maps short `kind` strings (`"kafka"`, `"api_poll"`, …) to factories (`SourceFactory` / `TransformFactory` / `SinkFactory`) that build trait objects from a `serde_json::Value` spec. Each category is its own namespace, so `"kafka"` can be both a source and a sink without collision. Duplicate `kind`s within a category are rejected at registration time.

Plugin model:
1. **Built-ins** — registered via `register_builtin(&mut registry)` (or `Registry::with_builtins()`).
2. **Statically-linked plugin crates** — call the crate's own `register(&mut registry)` before building the `Courier`. This is the first-class native plugin mechanism.
3. **(Future) dynamic plugins** — factory traits are object-safe, so `libloading`/scripting is additive.

`Registry::build_courier(config)` mints hierarchical node ids (`{pipeline}/src`, `{pipeline}/t{i}`, `{pipeline}/sink{i}`) so logs and metrics trace back to the owning pipeline. Factories that wrap a `WriteOne` in `ManagedSink` (or a `MapOne` in `BasicTransform`) receive `on_error` and `retry` pre-extracted by the registry — no per-component config parsing is needed for those policies. Use `courier::config::parse_config` in factory implementations to get uniform "invalid config for component type '{kind}'" error messages. `Registry::dry_run_build` builds and discards everything, used by `courier validate` to surface factory errors without side effects (file sinks are not opened, sockets not bound).

### Built-in nodes

- **Sources**: `api_poll` (HTTP poll), `http_webhook` (push), `kafka` (push), `sql_query_poll` (postgres/sqlite query, polling).
- **Transforms**: `batch` (size/time window, N→1), `filter` (Rhai/Lua/Python predicate), `mutate` (set/remove paths), `set_key` (sets `meta.key` from a payload field), `script` (full transform — set `script` inline or `script_file` to load from disk, not both; `runtime = "rhai" | "lua" | "python"`).
- **Sinks**: `api` (HTTP push; `body = "payload"` sends `env.payload`, `body = "envelope"` sends the full envelope), `file` (append-mode local file in `jsonl` or `csv` format; CSV columns are dotted paths into the envelope, e.g. `payload.id`, `meta.source_id`), `kafka`, `sql` (postgres/sqlite insert).

`script` and `filter` runtimes are not equivalent: Rhai and Lua are embedded (`mlua` with `lua54`/`vendored`/`send`), Python is **out-of-process** — each invocation execs `python_bin` (defaulting to system `python3`). Pick `rhai` or `lua` for hot paths.

### Runtime

`spawn_pipeline` (in `src/pipeline.rs`) wires source → transforms → sinks with mpsc channels. When `sinks.len() > 1`, an implicit **broadcast splitter** is inserted that clones each envelope to every sink. Since the splitter is synchronous per sink, a slow sink applies backpressure to the whole pipeline — by design.

### Runtime config loading (`src/config/`)

`Config::load(path)` accepts either a single file (parser picked by `.toml`/`.json` extension) or a directory (every `.toml`/`.json` file merged in sorted order, duplicate pipeline names rejected). `Config::from_toml_str` and `Config::from_json_str` are the in-memory entry points. Both parse into a JSON-shaped value, run a custom interpolation pass over every string (`${env:NAME}`, `${env:NAME:default}`, `${secret:NAME}`, whole-value `${file:path}`; `\$`/`\\` escape; bare `${NAME}` is rejected), then go through the same private `Raw*` layer that flattens arbitrary per-component fields (anything other than `type`, `on_error`, `retry`) into the component's `config: serde_json::Value` bucket; factories deserialize their own typed config through `parse_config`. TOML datetimes are stringified on the way through (no native JSON equivalent). Only `${secret:...}` and `${file:...}` resolutions are tagged as secrets in a process-global `RwLock<HashSet<String>>`; `redact_secret`/`redact_secret_path` consult that set with exact-match and elide values in `Debug` and in log call-sites that wrap user-controlled fields.

A top-level `[defaults]` block (`RawDefaults` in `src/config/raw.rs`) fills `on_error` / `retry` slots that components leave blank. Merge is shallow (component value wins entirely) and per-file (in directory mode each file's defaults stay scoped to that file, so load order can't change behavior).

`Config::validate()` runs structural checks (e.g. duplicate pipeline names, mutually exclusive `script`/`script_file`) without invoking any factory — `main.rs` calls it before initializing observability so config-shape errors hit stderr cleanly. `Registry::build_courier` runs the factories.

### Observability (`src/observability/`)

`[observability]` in config drives logs (text/JSON to stdout, OTLP), metrics (OTLP), and traces (OTLP). `init_from_config` installs the global `tracing-subscriber` plus the `log → tracing` bridge; legacy `log::` call-sites flow through the same pipeline. `init_default_logging` is the no-OTLP fallback used by `validate` / `list-components`. Pipelines get an `ObsHandle`; each node gets a `NodeCtx` (`set_node_ctx`) pre-bound with counters/histograms and the node id, so node-level metrics don't need lookup on the hot path. `trace_context.rs` round-trips W3C `traceparent` / `tracestate` through `Envelope.meta.headers`, which is what makes spans connect across mpsc edges.
