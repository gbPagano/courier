---
icon: lucide/terminal
---

# Development

This page covers the local codebase layout and the extension points used when adding or changing Courier components.

## Project layout

| Path              | Role |
| ----------------- | ---- |
| `src/lib.rs`      | `Courier`, the top-level runtime and SIGINT handling. |
| `src/pipeline.rs` | `Pipeline`, `spawn_pipeline`, and the broadcast splitter. |
| `src/envelope.rs` | `Envelope` and `Meta`. |
| `src/sources/`    | `Source` trait and built-in sources. |
| `src/transforms/` | `Transform`, `MapOne`, `BasicTransform`, and built-in transforms. |
| `src/sinks/`      | `Sink`, `WriteOne`, `ManagedSink`, and built-in sinks. |
| `src/retry.rs`    | `RetryPolicy`, `ExhaustedPolicy`, and dead-letter writing. |
| `src/registry.rs` | `Registry`, the kind-to-factory mapping. |
| `src/config.rs`   | TOML/JSON loaders and `parse_config`. |
| `src/main.rs`     | CLI entry point. |

## Component work

The fastest path is to implement the ergonomic trait for the role and let Courier's wrappers handle channel loops, cancellation, and policies:

| Role | Preferred path |
| ---- | -------------- |
| Source | Implement `Source::run(tx, cancel)` directly. Sources own their own cadence. |
| Transform | Implement `MapOne` and wrap it in `BasicTransform`. |
| Sink | Implement `WriteOne` and wrap it in `ManagedSink`. This gives you retry, dead-letter, and `on_error` handling. |

Register the component factory against a unique `kind` string. Use `courier::config::parse_config` inside the factory so config errors get a uniform `"invalid config for component type '{kind}'"` message.

External components can live in their own crate and expose a single registration function:

```rust
let mut registry = Registry::with_builtins();
my_plugin::register(&mut registry);
let courier = registry.build_courier(config)?;
```

The factory traits are object-safe, so dynamic loading with `libloading` or scripting layers can be added later without changing the core API.

## Documentation deployment

The `Documentation` GitHub Actions workflow (`.github/workflows/docs.yml`) builds the site on every push to `main` and publishes to GitHub Pages.
