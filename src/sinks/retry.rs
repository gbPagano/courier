use anyhow::Result;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::config::{redact_secret, redact_secret_path};
use crate::envelope::{DeadLetterEntry, Envelope};
use crate::observability::NodeCtx;
use crate::retry::{ExhaustedPolicy, RetryPolicy};
use crate::sinks::WriteOne;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteOutcome {
    Written,
    DeadLettered,
    Cancelled,
}

/// Attempts to write `env` via `inner`, retrying on failure according to
/// `policy`. Called by `ManagedSink` when a retry policy is configured.
///
/// Bumps `ctx.record_retry()` once per retry attempt (i.e. not on the
/// first try, only on attempts 2..N), and `ctx.record_dead_letter()`
/// when the policy successfully routes an exhausted envelope to a
/// dead-letter file.
pub(crate) async fn write_with_retry<W: WriteOne>(
    inner: &W,
    env: &Envelope,
    policy: &RetryPolicy,
    ctx: &NodeCtx,
    cancel: &CancellationToken,
) -> Result<WriteOutcome> {
    for attempt in 0..policy.max_attempts {
        if cancel.is_cancelled() {
            return Ok(WriteOutcome::Cancelled);
        }

        match inner.write(env).await {
            Ok(()) => return Ok(WriteOutcome::Written),
            Err(e) if attempt + 1 == policy.max_attempts => {
                return on_exhausted(
                    inner.id(),
                    env,
                    e,
                    &policy.on_exhausted,
                    ctx,
                    policy.max_attempts,
                )
                .await;
            }
            Err(e) => {
                let delay = policy.delay_for(attempt);
                ctx.record_retry();
                tracing::warn!(
                    node_id = %redact_secret(inner.id()),
                    attempt = attempt + 1,
                    max_attempts = policy.max_attempts,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    "write failed, retrying"
                );
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(WriteOutcome::Cancelled),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
    unreachable!()
}

async fn on_exhausted(
    id: &str,
    env: &Envelope,
    err: anyhow::Error,
    policy: &ExhaustedPolicy,
    ctx: &NodeCtx,
    attempts: u32,
) -> Result<WriteOutcome> {
    match policy {
        ExhaustedPolicy::Propagate => Err(err),
        ExhaustedPolicy::DeadLetter { path } => {
            let entry = DeadLetterEntry {
                envelope: env.clone(),
                error: format!("{err:#}"),
                pipeline: ctx.pipeline().to_string(),
                sink: ctx.node_id().to_string(),
                dead_lettered_at_ms: crate::envelope::now_ms(),
                attempts,
            };
            let mut line = serde_json::to_string(&entry)?;
            line.push('\n');

            let io_result = async {
                let mut file = tokio::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(path)
                    .await?;
                file.write_all(line.as_bytes()).await?;
                file.flush().await
            }
            .await;

            match io_result {
                Ok(()) => {
                    ctx.record_dead_letter();
                    tracing::warn!(
                        node_id = %redact_secret(id),
                        path = %redact_secret_path(path),
                        "all retries exhausted, envelope dead-lettered"
                    );
                    Ok(WriteOutcome::DeadLettered)
                }
                Err(io_err) => {
                    tracing::error!(
                        node_id = %redact_secret(id),
                        path = %redact_secret_path(path),
                        error = %io_err,
                        "failed to persist dead-letter entry"
                    );
                    Err(err)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use anyhow::anyhow;
    use async_trait::async_trait;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::envelope::Envelope;
    use crate::observability::metrics::testing::{counter_sum, obs_handle_in_memory};
    use crate::observability::{NodeCtx, NodeKind};
    use crate::retry::ExhaustedPolicy;

    struct CountingWriter {
        id: String,
        fail_times: u32,
        calls: Arc<AtomicU32>,
    }

    #[async_trait]
    impl WriteOne for CountingWriter {
        fn id(&self) -> &str {
            &self.id
        }

        async fn write(&self, _env: &Envelope) -> Result<()> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                Err(anyhow!("simulated failure"))
            } else {
                Ok(())
            }
        }
    }

    fn writer(fail_times: u32) -> (CountingWriter, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let w = CountingWriter {
            id: "test".into(),
            fail_times,
            calls: calls.clone(),
        };
        (w, calls)
    }

    fn fast_policy(max_attempts: u32, on_exhausted: ExhaustedPolicy) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            initial_delay_ms: 1,
            backoff_multiplier: 1.0,
            max_delay_ms: 10,
            on_exhausted,
        }
    }

    #[tokio::test]
    async fn succeeds_on_first_attempt() {
        let (w, calls) = writer(0);
        let policy = fast_policy(3, ExhaustedPolicy::Propagate);
        let env = Envelope::new("src", json!({}));
        let cancel = CancellationToken::new();
        assert!(
            write_with_retry(&w, &env, &policy, &NodeCtx::noop(), &cancel)
                .await
                .is_ok()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_and_succeeds() {
        let (w, calls) = writer(2);
        let policy = fast_policy(3, ExhaustedPolicy::Propagate);
        let env = Envelope::new("src", json!({}));
        let cancel = CancellationToken::new();
        assert_eq!(
            write_with_retry(&w, &env, &policy, &NodeCtx::noop(), &cancel)
                .await
                .unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_sleep_stops_when_cancelled() {
        let (w, calls) = writer(5);
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_delay_ms: 60_000,
            backoff_multiplier: 1.0,
            max_delay_ms: 60_000,
            on_exhausted: ExhaustedPolicy::Propagate,
        };
        let env = Envelope::new("src", json!({}));
        let cancel = CancellationToken::new();
        let cancel_after = cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel_after.cancel();
        });

        let outcome = tokio::time::timeout(
            Duration::from_millis(250),
            write_with_retry(&w, &env, &policy, &NodeCtx::noop(), &cancel),
        )
        .await
        .expect("retry sleep should stop promptly after cancellation")
        .unwrap();

        assert_eq!(outcome, WriteOutcome::Cancelled);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn records_retry_attempts() {
        let (handle, exporter) = obs_handle_in_memory();
        let ctx = NodeCtx::for_node("p", "p/sink0", NodeKind::Sink, handle.clone());
        let (w, calls) = writer(2);
        let policy = fast_policy(3, ExhaustedPolicy::Propagate);
        let env = Envelope::new("src", json!({}));
        let cancel = CancellationToken::new();

        write_with_retry(&w, &env, &policy, &ctx, &cancel)
            .await
            .unwrap();
        handle.shutdown();

        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            counter_sum(
                &exporter,
                "courier_retries_total",
                &[("pipeline", "p"), ("node_id", "p/sink0")]
            ),
            2
        );
    }

    #[tokio::test]
    async fn propagates_after_exhausting_attempts() {
        let (w, calls) = writer(5);
        let policy = fast_policy(3, ExhaustedPolicy::Propagate);
        let env = Envelope::new("src", json!({}));
        let cancel = CancellationToken::new();
        assert!(
            write_with_retry(&w, &env, &policy, &NodeCtx::noop(), &cancel)
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn dead_letters_exhausted_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dead.jsonl");

        let (w, calls) = writer(5);
        let policy = fast_policy(2, ExhaustedPolicy::DeadLetter { path: path.clone() });
        let mut env = Envelope::new("src", json!({"x": 1}));
        env.meta.key = Some("k1".into());
        let cancel = CancellationToken::new();

        assert_eq!(
            write_with_retry(&w, &env, &policy, &NodeCtx::noop(), &cancel)
                .await
                .unwrap(),
            WriteOutcome::DeadLettered
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let contents = std::fs::read_to_string(&path).unwrap();
        let entry: crate::envelope::DeadLetterEntry =
            serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(entry.envelope.meta.source_id, "src");
        assert_eq!(entry.envelope.meta.key, Some("k1".into()));
        assert_eq!(entry.envelope.payload, json!({"x": 1}));
        assert!(
            entry.error.contains("simulated failure"),
            "expected 'simulated failure' in error, got: {}",
            entry.error
        );
        assert_eq!(entry.attempts, 2);
        assert_eq!(entry.pipeline, "");
        assert_eq!(entry.sink, "");
        assert!(entry.dead_lettered_at_ms > 0);
    }

    #[tokio::test]
    async fn dead_letters_includes_routing_metadata() {
        let (handle, _exporter) = obs_handle_in_memory();
        let ctx = NodeCtx::for_node(
            "my-pipeline",
            "my-pipeline/sink0",
            NodeKind::Sink,
            handle.clone(),
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dead.jsonl");
        let (w, _) = writer(5);
        let policy = fast_policy(1, ExhaustedPolicy::DeadLetter { path: path.clone() });
        let env = Envelope::new("src", json!({"data": 42}));
        let cancel = CancellationToken::new();

        write_with_retry(&w, &env, &policy, &ctx, &cancel)
            .await
            .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let entry: crate::envelope::DeadLetterEntry =
            serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(entry.pipeline, "my-pipeline");
        assert_eq!(entry.sink, "my-pipeline/sink0");
        assert!(entry.dead_lettered_at_ms > 0);
        assert_eq!(entry.attempts, 1);
        assert_eq!(entry.envelope.payload, json!({"data": 42}));
    }

    #[tokio::test]
    async fn records_dead_lettered_envelope() {
        let (handle, exporter) = obs_handle_in_memory();
        let ctx = NodeCtx::for_node("p", "p/sink0", NodeKind::Sink, handle.clone());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dead.jsonl");
        let (w, _) = writer(5);
        let policy = fast_policy(1, ExhaustedPolicy::DeadLetter { path });
        let env = Envelope::new("src", json!({}));
        let cancel = CancellationToken::new();

        write_with_retry(&w, &env, &policy, &ctx, &cancel)
            .await
            .unwrap();
        handle.shutdown();

        assert_eq!(
            counter_sum(
                &exporter,
                "courier_dead_lettered_total",
                &[("pipeline", "p"), ("node_id", "p/sink0")]
            ),
            1
        );
    }

    #[tokio::test]
    async fn dead_letter_appends_multiple_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dead.jsonl");

        for i in 0..3u32 {
            let (w, _) = writer(5);
            let policy = fast_policy(1, ExhaustedPolicy::DeadLetter { path: path.clone() });
            let env = Envelope::new("src", json!({"i": i}));
            let cancel = CancellationToken::new();
            write_with_retry(&w, &env, &policy, &NodeCtx::noop(), &cancel)
                .await
                .unwrap();
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 3);
    }
}
