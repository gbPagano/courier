# Error handling & retry

Transforms and sinks can independently configure how they react to failures. Sinks additionally support automatic retry with a configurable backoff and dead-letter routing.

## `on_error` — the error policy

Every transform and sink accepts an optional `on_error` field:

| Value           | Behavior |
| --------------- | -------- |
| `drop`          | Log the error and continue. The envelope is dropped. |
| `fail_pipeline` | Cancel the entire pipeline via its `CancellationToken`. Other pipelines in the same `Courier` keep running. |

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

The dead-letter file format is one JSON envelope per line; treat it as provisional until Courier reaches 1.0.

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

- For idempotent sinks, prefer `dead_letter` with a generous `max_attempts` — transient blips will retry, and persistent failures land in a file you can inspect or replay.
- For pipelines where any data loss is unacceptable, set `on_error = "fail_pipeline"` and let your supervisor (systemd, Kubernetes, etc.) restart the binary.
- For transforms where the failure mode is "this one envelope is malformed", `on_error = "drop"` is usually right.
