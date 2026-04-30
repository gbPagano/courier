---
icon: lucide/download
---

# Installation

## Prerequisites

- A recent stable Rust toolchain — install via [rustup](https://rustup.rs/).
- A C toolchain (Courier links against `rdkafka` for the Kafka source/sink).
- Optional, for the [Python script transform](../scripting/python.md): a `python3` interpreter reachable on `PATH` or via the `python_bin` config field.

## Install from crates.io

Install the current beta release with Cargo:

```bash
cargo install data-courier --version 0.1.0-beta.1
```

This installs the `courier` binary.

## Build from source

```bash
git clone https://github.com/gbPagano/courier
cd courier
cargo build --release
```

The release binary is produced at `target/release/courier`.

## Verify the build

A debug build is enough for local development:

```bash
cargo check          # fast type/borrow check
cargo test           # run the test suite
cargo clippy --all-targets
```

## Run with a config

By default, Courier reads `config.toml` from the working directory. Override the path with the `COURIER_CONFIG` environment variable — it can point at a single `.toml`/`.json` file or at a directory containing several:

```bash
COURIER_CONFIG=./pipelines.d courier run
```

In directory mode, every `.toml`/`.json` file in the directory is parsed in sorted order and the resulting `pipelines` lists are concatenated.

Continue to the [Quickstart](quickstart.md) for a runnable minimal config.
