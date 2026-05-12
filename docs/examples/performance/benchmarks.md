---
icon: lucide/zap
---

# Benchmarks

**Reference machine:** Intel Core i7-12800H (14C/20T, 2.8–4.8 GHz) · 32 GB RAM · Arch Linux.

## Transform throughput

A single pipeline on a single-thread Tokio runtime, varying the number of chained transforms. **Native** is pure Rust (no scripting) and serves as the overhead baseline.

| Transforms | native  | rhai    | lua     | python  |
| ---------: | ------: | ------: | ------: | ------: |
| 0          | 2.39 M  |       — |       — |       — |
| 1          | 1.42 M  | 488.7 K | 209.5 K |  21.8 K |
| 3          | 837.9 K | 196.1 K |  70.3 K |   4.0 K |
| 5          | 548.3 K | 119.8 K |  43.2 K |   5.9 K |
| 10         | 327.3 K |  61.7 K |  21.5 K |       — |
| 20         | 181.5 K |  31.5 K |  10.8 K |       — |

Python benchmarks use 1 K envelopes (vs 10 K for embedded runtimes) due to subprocess overhead per invocation. Entries marked `—` were not measured.

## Pipeline scaling

Each pipeline runs 2 native + 1 Rhai transform on a multi-thread Tokio runtime. This measures how aggregate throughput grows as more concurrent pipelines are added.

| Pipelines | total (env/s) | per-pipeline (env/s) |
| --------: | ------------: | -------------------: |
|         1 |        444.6 K |              444.6 K |
|         5 |         1.24 M |              247.5 K |
|        10 |         1.83 M |              183.0 K |
|        25 |         2.20 M |               87.9 K |
|        50 |         2.06 M |               41.2 K |
|       100 |         2.30 M |               23.0 K |
|       500 |         2.41 M |                4.8 K |

Per-pipeline throughput drops as pipelines compete for CPU. For I/O-bound pipelines (e.g. Kafka source → Kafka sink), the bottleneck shifts to the external system and these numbers do not apply.
