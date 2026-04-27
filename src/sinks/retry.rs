use anyhow::Result;
use tokio::io::AsyncWriteExt;

use crate::envelope::Envelope;
use crate::observability::NodeCtx;
use crate::retry::{ExhaustedPolicy, RetryPolicy};
use crate::sinks::WriteOne;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteOutcome {
    Written,
    DeadLettered,
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
) -> Result<WriteOutcome> {
    for attempt in 0..policy.max_attempts {
        match inner.write(env).await {
            Ok(()) => return Ok(WriteOutcome::Written),
            Err(e) if attempt + 1 == policy.max_attempts => {
                return on_exhausted(inner.id(), env, e, &policy.on_exhausted, ctx).await;
            }
            Err(e) => {
                let delay = policy.delay_for(attempt);
                ctx.record_retry();
                tracing::warn!(
                    node_id = %inner.id(),
                    attempt = attempt + 1,
                    max_attempts = policy.max_attempts,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    "write failed, retrying"
                );
                tokio::time::sleep(delay).await;
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
) -> Result<WriteOutcome> {
    match policy {
        ExhaustedPolicy::Propagate => Err(err),
        ExhaustedPolicy::DeadLetter { path } => {
            let entry = serde_json::json!({
                "timestamp_ms": env.meta.timestamp_ms,
                "source_id": env.meta.source_id,
                "key": env.meta.key,
                "headers": env.meta.headers,
                "payload": env.payload,
                "error": format!("{err:#}"),
            });
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
                        node_id = %id,
                        path = %path.display(),
                        "all retries exhausted, envelope dead-lettered"
                    );
                    Ok(WriteOutcome::DeadLettered)
                }
                Err(io_err) => {
                    tracing::error!(
                        node_id = %id,
                        path = %path.display(),
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

    use anyhow::anyhow;
    use async_trait::async_trait;
    use serde_json::json;

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
        assert!(
            write_with_retry(&w, &env, &policy, &NodeCtx::noop())
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
        assert_eq!(
            write_with_retry(&w, &env, &policy, &NodeCtx::noop())
                .await
                .unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn records_retry_attempts() {
        let (handle, exporter) = obs_handle_in_memory();
        let ctx = NodeCtx::for_node("p", "p/sink0", NodeKind::Sink, handle.clone());
        let (w, calls) = writer(2);
        let policy = fast_policy(3, ExhaustedPolicy::Propagate);
        let env = Envelope::new("src", json!({}));

        write_with_retry(&w, &env, &policy, &ctx).await.unwrap();
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
        assert!(
            write_with_retry(&w, &env, &policy, &NodeCtx::noop())
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

        assert_eq!(
            write_with_retry(&w, &env, &policy, &NodeCtx::noop())
                .await
                .unwrap(),
            WriteOutcome::DeadLettered
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let contents = std::fs::read_to_string(&path).unwrap();
        let entry: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(entry["source_id"], "src");
        assert_eq!(entry["key"], "k1");
        assert_eq!(entry["payload"]["x"], 1);
        assert!(
            entry["error"]
                .as_str()
                .unwrap()
                .contains("simulated failure")
        );
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

        write_with_retry(&w, &env, &policy, &ctx).await.unwrap();
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
            write_with_retry(&w, &env, &policy, &NodeCtx::noop())
                .await
                .unwrap();
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 3);
    }
}
