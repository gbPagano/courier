# Observability

Courier ships a single, opt-in observability layer that produces structured logs (`tracing`), metrics, and W3C-propagated traces from the same in-process call sites. Every signal is exported via **OTLP** to an [OpenTelemetry Collector](https://opentelemetry.io/docs/collector/); Courier itself does **not** host a Prometheus scrape endpoint or a UI.

```text
Courier ──OTLP/gRPC──▶ OTel Collector ──▶ Prometheus / Tempo / Datadog / …
```

This keeps Courier focused, lets operators pick any backend the Collector supports, and avoids a hard dependency on `opentelemetry-prometheus`. A reference [Collector config](#sample-collector) and [Compose stack](#sample-compose-stack) ship with the repository.

## What you get for free

Built-in components and any third-party node that uses `BasicTransform` / `ManagedSink` are instrumented automatically:

- a structured **log line** for every error and retry, with `node_id`, `pipeline`, `error` fields — never the payload or the key by default.
- six **counters** and two **histograms** per node, plus a **channel-depth gauge** per mpsc edge.
- one **span** per source / transform / sink iteration, linked end-to-end through `Meta.headers["traceparent"]`.

No instrumentation work is required to publish a custom transform or sink — just implement `MapOne` or `WriteOne`.

## Configuration

Observability is driven entirely by an optional `[observability]` block in your config file. An empty or absent block keeps Courier silent: text logs to stderr, no metrics, no traces. Setting an `otlp_endpoint` is what activates each signal.

```toml title="config.toml"
[observability]
log_format = "json"            # "text" (default) or "json"
log_level  = "info"            # default filter when RUST_LOG is unset
log_keys   = false             # allow meta.key in span fields (off by default)

[observability.metrics]
otlp_endpoint     = "http://collector:4317"
export_interval_ms = 15000     # OTLP push interval (default 15 s)

[observability.tracing]
otlp_endpoint = "http://collector:4317"
sample_ratio  = 0.1            # parent-based sampling (0.0–1.0)
service_name  = "courier"      # OTel resource service.name
```

Precedence rules:

- `RUST_LOG` always wins over `log_level`, matching Courier's prior `env_logger` behavior.
- `metrics.otlp_endpoint = ""` (or unset) disables metric export — counters and histograms still exist in-process but observations are dropped to a private no-op meter.
- `tracing.otlp_endpoint = ""` (or unset) disables span export the same way.

Validation runs at startup: `sample_ratio` must be a finite value in `[0.0, 1.0]`, `service_name` must be non-empty, `export_interval_ms` must be greater than zero, and `log_level` must be a valid `EnvFilter` directive. Bad values fail `courier validate` and `courier run` with a path-annotated error.

## Logs

Courier installs a `tracing-subscriber` registry on startup. The format is `text` by default (one line per event, written to **stderr** so stdout stays clean) and can be switched to structured JSON with `log_format = "json"`. Existing `log::` call sites are bridged through `tracing-log::LogTracer`, so log level filtering still works the way it always did.

Every span and event carries the same canonical fields:

| Field                  | Meaning                                                        |
| ---------------------- | -------------------------------------------------------------- |
| `pipeline`             | Pipeline name from config.                                     |
| `node_id`              | Hierarchical id: `{pipeline}/src`, `…/t0`, `…/sink0`, `…/edge/…`. |
| `node_kind`            | One of `source`, `transform`, `sink`, `splitter`, `edge`.      |
| `envelope.source_id`   | `Meta.source_id` of the envelope being processed.              |
| `envelope.key`         | `Meta.key` — only emitted when `log_keys = true`.              |
| `error`                | Stringified error chain on failure / retry / dead-letter logs. |

Payloads and arbitrary header values are never logged. This is enforced as a privacy default: `tracing` calls in the runtime never reference `env.payload`, and `meta.key` only flows into a span when an operator opts in.

## Metrics

Every metric carries the labels `pipeline`, `node_id`, `node_kind`. No other labels are added by the runtime: payload-derived values like `meta.key` or `source_id` are span fields only, never metric attributes, so cardinality stays bounded by your topology.

| Name                                       | Type      | Unit | Reported by              | Meaning |
| ------------------------------------------ | --------- | ---- | ------------------------ | ------- |
| `courier_envelopes_processed_total`        | counter   | 1    | transforms, sinks        | Envelopes that completed successfully. Sinks only count writes that returned `Written` (not dead-lettered). |
| `courier_envelopes_filtered_total`         | counter   | 1    | transforms               | Envelopes intentionally dropped by `MapOne::map` returning `Ok(None)`. **Not** counted as `processed`. |
| `courier_envelopes_failed_total`           | counter   | 1    | transforms, sinks        | Envelopes that errored after any retries were exhausted. Includes envelopes routed to dead-letter (they are **both** `failed` and `dead_lettered`). |
| `courier_retries_total`                    | counter   | 1    | sinks                    | One increment per retry attempt — the first try is not counted. |
| `courier_dead_lettered_total`              | counter   | 1    | sinks                    | Envelopes successfully appended to a dead-letter file after retries were exhausted. |
| `courier_stage_duration_milliseconds`      | histogram | ms   | transforms, sinks        | Wall-clock time for one envelope through the node, including any retries. |
| `courier_end_to_end_latency_milliseconds`  | histogram | ms   | sinks                    | `now − env.meta.timestamp_ms` at write completion. Skipped when the source did not stamp a timestamp or the clock looks skewed. |
| `courier_channel_capacity_used`            | histogram | 1    | per mpsc edge (sampled)  | `channel_capacity − sender.capacity()` — items currently in flight on that edge. Sampled every 300 ms; carries `node_kind = "edge"` and `node_id = "{pipeline}/edge/{src}->{dst}"`. |

A few semantic rules worth knowing:

- **Filtered ≠ failed.** A transform that filters does not bump `processed`, `failed`, or `retries`. Dashboards summing "throughput" should add `processed + filtered` if they want input volume.
- **Dead-letter accounting is double-stamped on purpose.** A successfully dead-lettered envelope increments both `failed` and `dead_lettered`, so an operator alerting on `failed` always sees the event, while a dead-letter dashboard gets a precise sub-count. The dead-letter file write itself is best-effort: if appending fails, the original error is propagated to the configured `on_error` policy and `dead_lettered_total` is **not** incremented.
- **Channel-depth is a gauge sampled as a histogram.** The 300 ms cadence is fixed — fine enough to catch sustained backpressure, coarse enough to avoid metric spam on busy pipelines.

Metrics are pushed via OTLP/gRPC on the interval configured by `export_interval_ms`. The SIGINT handler force-flushes the `MeterProvider` and `TracerProvider` before joining task handles, so the last batch survives a graceful drain.

## Tracing

Each iteration of a source / transform / sink runs inside a span:

| Span name           | Emitted by         | Default level |
| ------------------- | ------------------ | ------------- |
| `courier.source`    | `SourceCtx::send`  | `INFO`        |
| `courier.transform` | `BasicTransform`   | `INFO`        |
| `courier.sink`      | `ManagedSink`      | `INFO`        |

Spans carry the same `pipeline` / `node_id` / `node_kind` / `envelope.source_id` fields as logs. `envelope.key` is added only when `log_keys = true`.

### W3C trace-context propagation

Courier carries trace context on `Envelope.Meta.headers` using the W3C [`traceparent`](https://www.w3.org/TR/trace-context/) and `tracestate` keys — no envelope schema change. The flow is:

1. `SourceCtx::send` opens a `courier.source` span, extracts any incoming `traceparent` (e.g. set by `KafkaSource` from message headers), and writes the resulting context back into the outgoing envelope's headers.
2. Each downstream `BasicTransform` and `ManagedSink` extracts the parent from `meta.headers`, opens its stage span as a child, and re-injects on the way out so the next hop sees a refreshed `traceparent`.
3. Built-in egress sinks copy `meta.headers["traceparent"]` onto the wire — `ApiSink` as an HTTP request header, `KafkaSink` as a Kafka message header — so downstream consumers see the same trace.

Sampling is parent-based with the configured `sample_ratio` at root. A non-sampled trace coming in stays non-sampled all the way through. The default `0.1` is conservative; raise it for low-volume pipelines or when actively debugging.

## Privacy and cardinality

- **Payloads are never logged or attached to spans.** A CI grep test fails the build if any `tracing::*!` call references `env.payload` outside the dedicated `trace_context` module.
- **`meta.key` is off by default.** Set `log_keys = true` only if your keys are non-sensitive — they will appear as the `envelope.key` span field on every transform and sink hop.
- **Metric labels are restricted to topology.** `pipeline`, `node_id`, `node_kind`, plus `outcome` where applicable. Anything derived from envelope contents is a span field, not a label.

## Sample Collector { #sample-collector }

A reference Collector config lives at [`otel-collector.yaml`](https://github.com/gbPagano/courier/blob/main/otel-collector.yaml) at the repo root. It receives OTLP/gRPC and OTLP/HTTP from Courier, re-exposes a Prometheus scrape endpoint on `:9464`, and forwards traces to Tempo (or, by uncommenting a single block, Jaeger):

```yaml
receivers:
  otlp:
    protocols:
      grpc: { endpoint: 0.0.0.0:4317 }
      http: { endpoint: 0.0.0.0:4318 }

exporters:
  prometheus:
    endpoint: 0.0.0.0:9464
  otlp/tempo:
    endpoint: tempo:4317
    tls: { insecure: true }

service:
  pipelines:
    metrics:
      receivers: [otlp]
      exporters: [prometheus]
    traces:
      receivers: [otlp]
      exporters: [otlp/tempo]
```

Point Courier's `otlp_endpoint` at `http://<collector>:4317` and Prometheus at `http://<collector>:9464/metrics` to scrape Courier through the Collector.

## Sample Compose stack { #sample-compose-stack }

The repository's [`docker-compose.observability.yml`](https://github.com/gbPagano/courier/blob/main/docker-compose.observability.yml) boots a full local stack you can point Courier at:

- **otel-collector** on `4317`/`4318` (OTLP) and `9464` (Prometheus scrape)
- **prometheus** scraping the Collector
- **tempo** receiving traces over OTLP
- **grafana** preconfigured with both datasources and the bundled [`dashboards/courier.json`](https://github.com/gbPagano/courier/blob/main/dashboards/courier.json)

Boot it, set `otlp_endpoint = "http://localhost:4317"` in your Courier config, drive traffic, and open Grafana on `http://localhost:3000`.

```bash
docker compose -f docker-compose.observability.yml up -d
COURIER_CONFIG=config.toml cargo run -- run
```

## Field reference

```text
Logs / spans
  pipeline             string  pipeline name
  node_id              string  {pipeline}/{src|t{i}|sink{i}|broadcast|edge/...}
  node_kind            enum    source | transform | sink | splitter | edge
  envelope.source_id   string  Envelope.Meta.source_id
  envelope.key         string  Envelope.Meta.key — only when log_keys = true
  error                string  formatted anyhow chain on failure logs

Metrics labels
  pipeline, node_id, node_kind  (no payload-derived attributes)
```
