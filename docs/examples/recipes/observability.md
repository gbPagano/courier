---
icon: lucide/activity
---

# Observability examples

Courier can export logs, metrics, and traces over OTLP. The repository includes a local Collector, Tempo, Loki, Prometheus, and Grafana stack.

## Local observability stack

Start the stack, then run Courier with the sample config:

```bash
docker compose -f examples/docker-compose.observability.yml up -d
COURIER_CONFIG=examples/config.toml courier run
```

The sample configuration exports all three signal types to `localhost:4317`:

```toml
[observability]
service_name = "courier"
log_level = "courier=debug"
log_keys = false

[observability.metrics]
otlp_endpoint = "http://localhost:4317"
export_interval_ms = 5000

[observability.tracing]
otlp_endpoint = "http://localhost:4317"
sample_ratio = 1.0

[observability.logs]
otlp_endpoint = "http://localhost:4317"
```

Grafana is available at `http://localhost:3000`. See [Observability](../../concepts/observability.md) for the full stack details.
