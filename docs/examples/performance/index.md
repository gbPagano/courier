
# Performance

Courier is built around bounded `tokio::mpsc` channels and per-stage tasks, so throughput and latency are shaped by how many transforms sit in the hot path and how many pipelines run concurrently.

The pages here show measured numbers from the built-in Criterion benchmarks. Use them as a reference baseline — always re-run on your own hardware with a workload that matches your use case.

```bash
cargo bench --bench transform_throughput   # single-pipeline throughput by runtime
cargo bench --bench pipeline_scaling       # concurrent pipeline scaling
```

- [Benchmarks](benchmarks.md) — reference numbers for transform throughput across scripting runtimes and concurrent pipeline scaling.
