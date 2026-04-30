---
icon: lucide/settings
---

# Configuration

Courier loads its pipeline definitions at runtime from a configuration file — there is no need to recompile when topology changes. The config file path defaults to `config.toml` in the working directory and can be overridden with the `COURIER_CONFIG` environment variable.

`COURIER_CONFIG` may point at:

- a single `.toml` or `.json` file, or
- a directory — every `.toml`/`.json` file inside it is parsed in sorted order, and their `pipelines` lists are concatenated. Duplicate pipeline names across files are rejected at load time.

Bad configuration fails at startup with a path-annotated error. String values may reference environment variables or files; unresolved references fail during this load phase rather than becoming empty strings.

## Validation phases

Courier validates configuration after parsing and interpolation, before building runtime tasks:

1. **Load/merge validation** parses TOML or JSON, interpolates string values, applies per-file defaults, and merges directory-mode files. Directory mode rejects duplicate pipeline names across files.
2. **Core validation** checks Courier-owned fields: non-empty and unique pipeline names, `channel_capacity > 0`, at least one sink per pipeline, non-empty component `type` values, retry bounds, and practical dead-letter path checks.
3. **Component validation** runs each registered source, transform, and sink factory. Built-ins validate their own domain rules, such as script shape, URL syntax, SQL driver/DSN pairing, Kafka topic/group fields, and webhook bind/path values.

`courier run` and `courier validate` use the same validation and build path, so a config accepted in CI is the same config shape accepted at startup. Validation does not prove that remote systems are reachable or that credentials are accepted; network connectivity, database permissions, Kafka broker availability, and receiver-side HTTP failures remain runtime checks.

## Interpolation and secrets

Interpolation runs after TOML/JSON parsing and before Courier deserializes typed config. It applies to every string value, including top-level Courier fields, observability fields, and component-specific fields. Every reference is namespaced — operators must spell out whether a value is plain configuration or a secret.

```toml
[[pipelines]]
name = "${env:PIPELINE_NAME:orders}"

[pipelines.source]
type = "api_poll"
url = "${env:ORDERS_URL}"
token = "Bearer ${secret:ORDERS_API_TOKEN}"

[[pipelines.sinks]]
type = "kafka"
brokers = "${env:KAFKA_BROKERS}"
sasl_password = "${file:/etc/courier/kafka-password}"
```

Supported references:

- `${env:NAME}` reads `NAME` from the process environment. Plaintext: the resolved value remains visible in logs and `Debug` output.
- `${env:NAME:default}` uses `default` when `NAME` is unset. Defaults are plaintext too; if the value is sensitive, use `${secret:...}` or `${file:...}` instead.
- `${secret:NAME}` reads `NAME` from the process environment and classifies the resolved value as a secret. Defaults are not allowed — a secret with a plaintext fallback is almost always a misconfiguration.
- `${file:path/to/secret}` reads UTF-8 file contents and classifies them as a secret. Relative paths are resolved from the config file's directory; in directory mode, from the file that contains the reference. File references must occupy the whole string value — embedding `${file:...}` in a larger string is rejected with a clear error. A single trailing newline (`\n` or `\r\n`) is stripped, so `echo my-token > /etc/courier/token` produces the same value as a file written without a trailing newline.

Resolution order: parse the selected TOML/JSON file or sorted directory files; resolve `${file:...}` whole-value references; resolve `${env:...}` and `${secret:...}` references; apply Courier defaults; merge directory-mode files.

Missing references (no env var set, file missing, no allowed default) are startup failures with the failing field path, for example `pipelines[0].source.token`. Courier never substitutes missing values with empty strings.

Use a backslash to keep a literal template. In TOML literal strings:

```toml
value = '\${env:NOT_INTERPOLATED}'
```

In TOML basic strings or JSON strings, escape the backslash:

```json
{ "value": "\\${env:NOT_INTERPOLATED}" }
```

Only values resolved from `${secret:...}` and `${file:...}` are classified as secrets. Courier redacts those values exactly — both the raw resolved value and the enclosing string they were interpolated into — in `Config` `Debug` output and in the log/error sites that wrap user-controlled fields. Values from `${env:...}` (with or without a default) and literal strings stay visible. Redaction is exact-match: derived strings such as composite IDs are not automatically redacted, so do not place secrets in fields like pipeline names.

## CLI checks

Courier can validate configuration and inspect the installed runtime without starting pipelines.

```bash
courier validate --config config.toml
```

`validate` loads the same config path rules as `run` (`--config`, then `COURIER_CONFIG`, then `config.toml`), parses the file or directory, registers built-ins, validates core settings, and builds the runtime graph without spawning pipelines. It exits non-zero with a path-annotated error if parsing, validation, or component construction fails. Use it as a CI or pre-deploy gate:

```yaml
- name: Validate Courier config
  run: courier validate --config ./config.toml
```

List the component kinds available in a binary:

```bash
courier list-components
```

Start pipelines with the explicit `run` command:

```bash
courier run
courier run --config config.toml
```


## In this section

- **[Pipelines](pipelines.md)** — the configuration schema for sources, transforms, sinks, and channels.
- **[Error Handling & Retry](error-handling.md)** — `on_error`, retry policies, and dead-letter routing.
- **[Sources](sources.md)** — source configuration reference.
- **[Transforms](transforms.md)** — transform configuration reference.
- **[Sinks](sinks.md)** — sink configuration reference.

## Minimal example

```toml title="config.toml"
[[pipelines]]
name = "api->kafka"
channel_capacity = 64

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 3

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
```

See [Pipelines](pipelines.md) for the full set of fields.
