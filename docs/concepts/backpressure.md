---
icon: lucide/gauge
---

# Backpressure

Courier does not have a global rate limiter or scheduler. End-to-end flow control comes entirely from the bounded `tokio::mpsc` channels between nodes.

## How it works

Every edge in a pipeline is an mpsc channel with a buffer of `channel_capacity` envelopes. The channel's `send` future completes only when there is room in the buffer.

That gives natural propagation: when a sink slows down, its inbox fills, the upstream transform's `send` blocks, *its* inbox fills, and eventually the source stops pulling. There is no back-channel "please slow down" message — the absence of buffer space is the signal.

```text
Source ──[64 buffered]──> Transform ──[64 buffered]──> Sink
                                                        ▲
                                                slow consumer
                                                pauses here
```

## The broadcast splitter

When a pipeline has more than one sink, Courier inserts an implicit broadcast splitter that clones each envelope to every sink. **The splitter is synchronous per sink**: it sends to every downstream channel in turn and only moves on once they have all accepted. That means a slow sink applies backpressure to the whole pipeline.

This is a deliberate design choice. It keeps semantics simple — every sink sees every envelope, in order — at the cost of pinning throughput to the slowest consumer. If you need an independent slow consumer, run it in its own pipeline reading from the same source.

## Tuning `channel_capacity`

`channel_capacity` is a per-pipeline knob set in the pipeline's top-level config:

```toml
[[pipelines]]
name = "api->kafka"
channel_capacity = 64
```

- **Smaller** values (e.g. 1–16) tighten backpressure: producers feel slowdowns sooner, latency stays predictable, memory bounded. Good for low-volume control planes.
- **Larger** values (e.g. 256–4096) let bursts pass through without stalling the source. Good for ingestion pipelines where short consumer hiccups are common and average throughput is what matters.

There is no one-size-fits-all default — pick a value that matches the burst profile of your source and the latency characteristics of your sink. A safe starting point for most workloads is somewhere in the 32–128 range, then adjust based on the queue depth you observe in the logs.

## Cancellation vs backpressure

Backpressure pauses *production*; cancellation stops the pipeline. Both are wired through Tokio primitives but they are separate concerns:

- A full inbox is normal — wait for it to drain.
- A fired `CancellationToken` (SIGINT, or `on_error = "fail_pipeline"`) tells every node to finish its current item and exit.
