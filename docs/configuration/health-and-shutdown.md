---
icon: lucide/heart-pulse
---

# Health probes & shutdown

Courier exposes HTTP health probes for container orchestrators and a configurable drain timeout for graceful shutdown. Both are optional — omit them from your config and Courier runs without a health server and uses a 30-second drain deadline.

## Health probes

Enable by adding a `[health]` block:

```toml
[health]
address = "0.0.0.0:9090"
```

| Field      | Required | Description |
| ---------- | -------- | ----------- |
| `address`  | yes      | Socket address to bind the health server to (`HOST:PORT` format). Must be a valid `SocketAddr`. |

When configured, Courier starts an HTTP server at startup that serves two endpoints:

| Endpoint           | Method | Response |
| ------------------ | ------ | -------- |
| `/health/live`     | GET    | `200 OK` — body `ok`. The process is alive. |
| `/health/ready`    | GET    | `200 OK` when every pipeline is `Running` and no shutdown has been requested; `503 Service Unavailable` otherwise. |

The readiness response body is JSON:

```json
{
  "status": "not_ready",
  "pipelines": [
    { "name": "orders", "state": "starting" },
    { "name": "events", "state": "running" }
  ]
}
```

Pipeline states are explained in [Lifecycle, health probes, and shutdown](../concepts/lifecycle.md).

### Kubernetes example

```yaml
livenessProbe:
  httpGet:
    path: /health/live
    port: 9090
  initialDelaySeconds: 5
  periodSeconds: 10

readinessProbe:
  httpGet:
    path: /health/ready
    port: 9090
  initialDelaySeconds: 5
  periodSeconds: 10
```

If `[health]` is not present in the config, no health server is started.

## Shutdown timeout

On SIGINT/Ctrl+C, Courier stops sources and drains in-flight envelopes through sinks. The drain is bounded by a configurable timeout:

```toml
[shutdown]
timeout_secs = 60
```

| Field           | Required | Default | Description |
| --------------- | -------- | ------- | ----------- |
| `timeout_secs` | no       | 30      | Maximum seconds to wait for in-flight envelopes to drain after SIGINT. Must be greater than 0. |

If the timeout expires before all tasks complete, Courier logs a warning and continues shutdown — remaining tasks are orphaned (they keep running until the process exits; dropping a `JoinHandle` does not abort the underlying tokio task) and any envelopes still in channel buffers are dropped.

### What happens during drain

1. **SIGINT received** — all pipelines transition to `Draining`, the shared `CancellationToken` is fired.
2. **Sources** stop producing new envelopes (their `run` loop exits on cancellation).
3. **Sinks** continue consuming from their channel receivers until the upstream sender closes.
4. Envelopes already in channel buffers are processed as long as they drain within the timeout.
5. If the timeout expires first, those envelopes are lost.

A shorter timeout means faster process exit but risks dropping envelopes. A longer timeout gives sinks more time to flush but delays termination.

## Exit codes

The `courier run` command exits with:

| Code | Condition |
| ---- | --------- |
| 0    | All pipelines shut down cleanly (SIGINT or natural source exhaustion). |
| 1    | Configuration or startup error, or at least one pipeline hit `fail_pipeline`. |
| 2    | Invalid CLI arguments (handled by `clap`). |

When `on_error = "fail_pipeline"` triggers, the pipeline transitions to the `Failed` state and the process exits with code 1. See [Error handling & retry](error-handling.md) for configuring error policies.