---
icon: lucide/rotate-ccw
---

# Error handling & retry

Transforms and sinks can independently configure how they react to failures. Sinks additionally support automatic retry with a configurable backoff and dead-letter routing.

## `on_error` — the error policy

Every transform and sink accepts an optional `on_error` field:

| Value           | Behavior |
| --------------- | -------- |
| `drop`          | Log the error and continue. The envelope is dropped. |
| `fail_pipeline` | Cancel the entire pipeline via its `CancellationToken` and transition the pipeline to the `Failed` state. Other pipelines in the same `Courier` keep running. The process exits with code 1. See [Lifecycle](../concepts/lifecycle.md). |

```toml
[[pipelines.transforms]]
type = "script"
runtime = "rhai"
on_error = "drop"
script = "fn transform(env) { env }"
```

If `on_error` is omitted the implementation default is used (typically `drop`).

## Retry on sinks

Sinks built on top of `ManagedSink` accept an optional retry policy. Retry runs *before* `on_error`: if all attempts fail, the policy's `on_exhausted` action decides whether to propagate the error (and let `on_error` handle it) or to dead-letter the envelope.

```toml
[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
on_error = "drop"

[pipelines.sinks.retry]
max_attempts = 5
initial_delay_ms = 100
backoff_multiplier = 2.0
max_delay_ms = 5000

[pipelines.sinks.retry.on_exhausted]
kind = "propagate"
```

| Field                | Description |
| -------------------- | ----------- |
| `max_attempts`       | Maximum attempts including the first try. |
| `initial_delay_ms`   | Delay before the second attempt. |
| `backoff_multiplier` | Backoff multiplier applied after each failure. |
| `max_delay_ms`       | Cap on the delay between attempts. |
| `on_exhausted`       | What to do once `max_attempts` is reached. See below. |

Validation rejects retry policies with `max_attempts = 0`, non-finite or less-than-`1.0` backoff multipliers, `max_delay_ms < initial_delay_ms`, or zero delays when multiple attempts are configured. Dead-letter paths must be non-empty; if a parent directory is present, it must already exist and be a directory.

## Exhausted policy

Once retries are exhausted, `on_exhausted` decides the fate of the envelope:

=== "Propagate"

    ```toml
    [pipelines.sinks.retry]
    max_attempts = 3
    initial_delay_ms = 100
    backoff_multiplier = 2.0
    max_delay_ms = 5000

    [pipelines.sinks.retry.on_exhausted]
    kind = "propagate"
    ```

    The last error is returned to `ManagedSink`, which then applies `on_error`. With `on_error = "drop"`, the envelope is logged and dropped; with `fail_pipeline`, the whole pipeline is cancelled.

=== "Dead-letter"

    ```toml
    [pipelines.sinks.retry]
    max_attempts = 3
    initial_delay_ms = 100
    backoff_multiplier = 2.0
    max_delay_ms = 5000

    [pipelines.sinks.retry.on_exhausted]
    kind = "dead_letter"
    path = "./dlq.jsonl"
    ```

    The failed envelope is appended to `path` as a single JSON line, then the pipeline continues. If the dead-letter write itself fails, the original error is propagated as if `kind = "propagate"` had been configured.

Each dead-letter line is a JSON object with the following structure:

```json
{
  "envelope": {
    "meta": {
      "key": "some-key",
      "source_id": "my-pipeline/src",
      "timestamp_ms": 1715234567890,
      "headers": {}
    },
    "payload": { "x": 1 }
  },
  "error": "HTTP error: 500 Internal Server Error",
  "pipeline": "my-pipeline",
  "sink": "my-pipeline/sink0",
  "dead_lettered_at_ms": 1715234568000,
  "attempts": 3
}
```

The `envelope` field round-trips through `Envelope`'s serde serialization, so tools that replay dead-letter files can deserialize each line and extract a valid `Envelope` without manual transformation. The `pipeline` and `sink` fields identify the origin of the dead letter for multi-pipeline routing. `dead_lettered_at_ms` records when the entry was written. `attempts` is the total number of write attempts before the dead letter was created.

### Replaying dead-letter entries

Use the `jsonl_file` source to re-ingest envelopes from a dead-letter file:

```toml
[[pipelines]]
name = "replay"

[pipelines.source]
type = "jsonl_file"
path = "./dlq.jsonl"

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
```

`jsonl_file` source config fields:

| Field | Required | Description |
|---|---|---|
| `path` | Yes | Path to the JSONL file. Must exist at build time. |
| `emit_interval_ms` | No | Milliseconds to wait between each emitted line. Useful for rate-limiting replay against a slow sink. Must be greater than 0 if set. |
| `dead_letter_pipeline` | No | When set, only lines whose `pipeline` field matches this value are emitted; bare envelopes (no `pipeline` annotation) are also emitted, unless a sink filter is active (see below). |
| `dead_letter_sink` | No | When set, only lines whose `sink` field matches this value are emitted. Used to split a multi-sink replay so each sink only receives its own failed envelopes. |

!!! warning "Prefer `replay-dlq` over manual `jsonl_file` configuration"
    Manually wiring a `jsonl_file` source works for simple cases, but has pitfalls that `replay-dlq` handles automatically:

    - **No pipeline filtering** — without `dead_letter_pipeline`, entries from every pipeline are emitted to the sink, including entries intended for other pipelines.
    - **No sink filtering** — if the original pipeline fans out to multiple sinks, every sink receives every entry, including entries that already succeeded at sibling sinks.
    - **Transforms re-execute** — envelopes in a DLQ have already passed through the transform chain. Running them through transforms again can produce incorrect or duplicated data, especially for non-idempotent transforms.
    - **Infinite loop risk** — if the sink has `on_exhausted = { kind = "dead_letter", path = "./dlq.jsonl" }` pointing at the same file the source is reading, failed writes append back into the replay source, creating an infinite loop. `replay-dlq` detects this and switches the sink to `propagate`.

    Use manual `jsonl_file` configuration only when you need explicit control — for example, to add specific transforms during replay, or to route DLQ entries to different sinks.

Or use the CLI:

```
courier replay-dlq ./dlq.jsonl -c config.toml
```

This replaces every pipeline's source with a `jsonl_file` source reading from the given path and runs until all entries have been emitted. `replay-dlq` also:

- **Strips transforms** from each pipeline, since envelopes captured by the sink have already been through the transform chain. Re-running non-idempotent transforms would produce incorrect or duplicated data.
- **Filters sinks** to only the sink that produced each DLQ entry (determined from the `sink` field). If the original pipeline fans out to multiple sinks and only one failed, replay sends entries only to the failed sink instead of duplicating them into sibling sinks that already succeeded.
- **Neutralizes dead-letter paths** that would write back into the replay source file. If a sink's `on_exhausted` targets the same file being replayed, it is switched to `propagate` so replay failures surface as errors instead of creating an infinite loop.

!!! note "Bare entries and multi-sink replay"
    When `courier replay-dlq` splits a pipeline that originally fanned out to multiple sinks, each replay pipeline uses a per-sink filter to prevent cross-contamination. Bare-envelope entries that carry no `pipeline` annotation are **dropped with a warning** in that case, because the source cannot determine which sink they were intended for.

    When the DLQ file contains **any** unattributed lines (bare envelopes), `replay-dlq` keeps all sinks in the pipeline and omits per-sink filtering so those lines can be replayed. In this mode, attributed entries are still filtered to the matching pipeline, but all entries are delivered to every sink — meaning each sink receives both its own entries and entries intended for sibling sinks. If this is undesirable, remove bare-envelope lines from the DLQ file before replaying.

## Defaults

Repeating the same `on_error` and `retry` block on every sink across every pipeline gets noisy fast. A top-level `[defaults]` block lets you set them once and override per-component when needed.

```toml
[defaults.sink]
on_error = "fail_pipeline"

[defaults.sink.retry]
max_attempts = 5
initial_delay_ms = 200
backoff_multiplier = 2.0
max_delay_ms = 5000
on_exhausted = { kind = "dead_letter", path = "/var/log/courier/dlq.jsonl" }

[defaults.transform]
on_error = "drop"
```

Supported keys:

| Key                         | Applied to | Description |
| --------------------------- | ---------- | ----------- |
| `defaults.sink.on_error`    | every sink | Used when the sink omits `on_error`. |
| `defaults.sink.retry`       | every sink | Used when the sink omits the `retry` block. |
| `defaults.transform.on_error` | every transform | Used when the transform omits `on_error`. |

Merge semantics are **shallow**: a per-component value entirely replaces the default. That means a sink that defines its own `[pipelines.sinks.retry]` does *not* inherit individual fields from `[defaults.sink.retry]` — spell out the full retry block when you want to deviate.

In directory mode (`COURIER_CONFIG=./conf.d`) defaults are **per file**: each file is parsed independently, so a default declared in `a.toml` never leaks into pipelines defined in `b.toml`. This keeps load order from quietly changing behavior.

## Choosing a strategy

- For idempotent sinks, prefer `dead_letter` with a generous `max_attempts` — transient blips will retry, and persistent failures land in a file you can inspect. Use the `jsonl_file` source or `courier replay-dlq` to re-ingest dead-lettered envelopes.
- For pipelines where any data loss is unacceptable, set `on_error = "fail_pipeline"` and let your supervisor (systemd, Kubernetes, etc.) restart the binary.
- For transforms where the failure mode is "this one envelope is malformed", `on_error = "drop"` is usually right.
