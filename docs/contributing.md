---
icon: lucide/heart-handshake
---

# Contributing

Courier is pre-1.0 and welcomes contributions — bug reports, design feedback, and pull requests. This page covers the local development workflow and the docs site itself.

## Development workflow

Standard cargo commands:

```bash
cargo build                          # build
cargo check                          # fast type/borrow check
cargo clippy --all-targets           # lint
cargo fmt                            # format
cargo test                           # run all tests
cargo test <test_name>               # run a single test
```

CI runs `clippy` and `cargo test` on every push. Please run them locally before opening a pull request.

## Project layout

A short tour of the `src/` tree:

| Path                    | Role |
| ----------------------- | ---- |
| `src/lib.rs`            | `Courier` — top-level runtime, SIGINT handling. |
| `src/pipeline.rs`       | `Pipeline`, `spawn_pipeline`, broadcast splitter. |
| `src/envelope.rs`       | `Envelope` and `Meta`. |
| `src/sources/`          | `Source` trait and built-ins. |
| `src/transforms/`       | `Transform`, `MapOne`, `BasicTransform`, built-ins. |
| `src/sinks/`            | `Sink`, `WriteOne`, `ManagedSink`, built-ins. |
| `src/retry.rs`          | `RetryPolicy`, `ExhaustedPolicy`, dead-letter writer. |
| `src/registry.rs`       | `Registry` — kind → factory mapping. |
| `src/config.rs`         | TOML/JSON loaders and `parse_config`. |
| `src/main.rs`           | Binary entry point. Reads `COURIER_CONFIG`, builds a default registry, runs. |

## Writing a new component

The fastest path is to implement the **ergonomic** trait for the role and let Courier's wrappers handle the channel loop, cancellation, and policies:

- **Transform** → implement `MapOne`, wrap in `BasicTransform`.
- **Sink** → implement `WriteOne`, wrap in `ManagedSink` (you get retry, dead-letter, and `on_error` for free).
- **Source** → implement `Source::run(tx, cancel)` directly. Sources own their own cadence.

Then register a factory against a unique `kind` string. Use `courier::config::parse_config` inside the factory so config errors get a uniform `"invalid config for component type '{kind}'"` message.

## Plugin crates

External components live in their own crate and expose a single `register(&mut Registry)` function. Call it before building the `Courier`:

```rust
let mut registry = Registry::with_builtins();
my_plugin::register(&mut registry);
let courier = registry.build_courier(config)?;
```

The factory traits are object-safe, so dynamic loading (`libloading`, scripting layers) is additive — it does not require API changes.

## Documentation site

The documentation site lives in `docs/` and is built with [Zensical](https://zensical.org).

### Local preview

The simplest way to run Zensical locally is through [`uv`](https://docs.astral.sh/uv/) — no global install, no virtualenv to manage:

```bash
uvx zensical serve         # live-reload preview at http://127.0.0.1:8000
uvx zensical build --clean # static build into ./site
```

!!! note "Template overrides leak into `site/`"
    `docs/overrides/` holds Jinja template overrides used by the home hero.
    Zensical copies every non-Markdown file under `docs/` verbatim into the
    output, so a stray `site/overrides/home.html` ends up in the build. It is
    harmless (no link or sitemap entry points to it), but the CI workflow
    strips it before deploy. Run `rm -rf site/overrides` after a local build
    if you want to mirror that behavior.

If you prefer a regular install:

```bash
pip install zensical
zensical serve
```

`zensical.toml` at the repo root is the source of truth for site name, navigation, theme, and Markdown extensions.

### Adding a page

1. Create the Markdown file under `docs/` — typically inside an existing section directory.
2. Add it to the `nav` array in `zensical.toml`. Keep the order intentional; the sidebar mirrors `nav` exactly.
3. Run `zensical serve` and verify the page renders, the sidebar entry shows up, and any cross-links resolve.

### Style

- Lead each page with a one-paragraph summary of what the page covers.
- Prefer tables over prose for reference material (config fields, traits, runtimes).
- Cross-link liberally — `[Architecture](concepts/architecture.md)` is more discoverable than "see the architecture page".
- Keep code examples minimal and copy-pasteable.

### Deployment

The `Documentation` GitHub Actions workflow (`.github/workflows/docs.yml`) builds the site on every push to `main` and publishes to GitHub Pages.

## Reporting bugs

Open an issue on GitHub with:

- The `config.toml` (or relevant pipeline excerpt) that reproduces the problem.
- The Courier git SHA you are running.
- Any logs from `RUST_LOG=courier=debug` if the failure is at runtime.
