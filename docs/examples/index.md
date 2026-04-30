# Recipes

Use these examples as starting points for real `config.toml` files. They are grouped by what you want to test in a Courier pipeline.

## Run an example

Copy a recipe into `config.toml`, adjust endpoints or credentials, then run:

```bash
courier run --config config.toml
```

Kafka examples assume a broker at `localhost:9092` and an existing target topic.

## In this section

- **[Pipelines](recipes/pipelines.md)** — complete source-to-sink recipes.
- **[Transforms](recipes/transforms.md)** — keying and script transform recipes.
- **[Reliability](recipes/reliability.md)** — retry, dead-letter, and fan-out behavior.
- **[Observability](recipes/observability.md)** — local OTLP, Collector, and Grafana setup.
