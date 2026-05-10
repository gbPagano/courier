---
icon: lucide/heart-pulse
---

# Lifecycle, health probes, and shutdown

Courier tracks the lifecycle state of every pipeline and exposes HTTP endpoints for container orchestrators (Kubernetes, systemd, Docker, etc.) to probe liveness and readiness. The same state machine drives graceful shutdown and exit codes.

## Pipeline states

Each pipeline transitions through a well-defined set of states:

```text
Starting → Running → Draining → Stopped   (normal shutdown)
               ↘ Failed                    (unrecoverable error)
```

| State      | Meaning |
| ---------- | ------- |
| `Starting` | Tasks have been spawned but have not yet begun processing envelopes. |
| `Running`  | The pipeline is actively processing envelopes. Reached once `spawn_pipeline` finishes wiring channels. |
| `Draining` | A shutdown signal was received. Sources stop pulling; sinks drain their channel receivers until upstream closes. |
| `Stopped`  | All tasks completed. Terminal state for graceful shutdown. |
| `Failed`   | An unrecoverable error terminated the pipeline (the `fail_pipeline` error policy). Terminal state — the pipeline does not recover. |

State transitions are **forward-only** (except for `Failed`, which can be reached from any non-terminal state). Once a pipeline enters `Failed` or `Stopped`, no subsequent transition can overwrite it — a concurrent SIGINT arriving after a `FailPipeline` error will not erase the failure.

## Health probes

Enable the health server with a `[health]` block in your config:

```toml
[health]
address = "0.0.0.0:9090"
```

The server provides two endpoints:

| Endpoint           | Method | Returns |
| ------------------ | ------ | ------- |
| `/health/live`     | GET    | `200 OK` with body `ok` — the process is alive. |
| `/health/ready`    | GET    | `200 OK` when every pipeline is `Running` and no shutdown has been requested; `503 Service Unavailable` otherwise. |

The readiness response includes the state of every pipeline:

```json
{
  "status": "not_ready",
  "pipelines": [
    { "name": "orders", "state": "starting" },
    { "name": "events", "state": "running" }
  ]
}
```

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

## Graceful shutdown

On SIGINT/Ctrl+C, Courier:

1. **Transitions** every pipeline to `Draining`.
2. **Cancels** the shared `CancellationToken` — sources stop pulling, transforms finish their current item, sinks drain their channel receivers.
3. **Waits** up to `shutdown.timeout_secs` (default 30 s) for all tasks to complete.
4. If the deadline expires, **logs a warning** and orphan remaining tasks (they keep running until the process exits; dropping a `JoinHandle` does not abort the underlying tokio task).
5. **Finalizes** pipeline states: non-failed pipelines move to `Stopped`, failed pipelines stay `Failed`.
6. **Force-flushes** metrics, traces, and logs providers.
7. **Aborts** the health server task.

### Shutdown timeout

Configure the drain deadline with a `[shutdown]` block:

```toml
[shutdown]
timeout_secs = 60
```

| Field           | Required | Default | Description |
| --------------- | -------- | ------- | ----------- |
| `timeout_secs` | no       | 30      | Maximum seconds to wait for in-flight envelopes to drain after SIGINT. Must be greater than 0. |

A shorter timeout means faster process exit but risks dropping envelopes that are still in channel buffers. A longer timeout gives sinks more time to flush but delays termination.

### What happens to in-flight envelopes

Sources stop producing new envelopes immediately when the cancellation token fires. Sinks continue consuming from their channel receivers until the upstream closes (the source task drops its sender). Envelopes already in channel buffers are processed as long as they drain within the timeout. If the timeout expires first, those envelopes are lost.

## Exit codes

Courier's `run` command exits with:

| Code | Condition |
| ---- | --------- |
| 0    | All pipelines shut down cleanly (SIGINT or natural source exhaustion). |
| 1    | Configuration or startup error, or at least one pipeline hit `fail_pipeline`. |
| 2    | Invalid CLI arguments (handled by `clap`). |

The `fail_pipeline` error policy sets the pipeline state to `Failed` and cancels its token. At process exit, `Courier::run()` inspects `CourierState::has_failures()` and returns `RunOutcome::Failed`, which maps to exit code 1.

This lets supervisors and init systems detect runtime failures:

```bash
courier run -c config.toml || echo "Courier exited with error code $?"
```

In a Kubernetes pod spec, a non-zero exit code triggers pod restart policies automatically.

## Interaction with error policies

The `fail_pipeline` error policy and lifecycle state are tightly coupled:

1. A sink or transform with `on_error = "fail_pipeline"` encounters an error.
2. `NodeCtx::mark_pipeline_failed()` transitions the pipeline to `Failed`.
3. The node calls `cancel.cancel()` to signal all other tasks in the pipeline.
4. Sources, transforms, and sinks see the cancellation and exit their loops.
5. At the end of `run()`, `has_failures()` returns `true` and the process exits with code 1.

The `drop` error policy does not affect lifecycle state — the pipeline continues running.