use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::Sender;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::config::{parse_config, redact_secret};
use crate::envelope::Envelope;
use crate::observability::{NodeCtx, SendStopped, SourceCtx};
use crate::retry::RetryPolicy;
use crate::sources::Source;
use crate::sources::retry::PollScheduler;

/// Polls an HTTP endpoint at a fixed interval, emitting each JSON response
/// body as the envelope payload. Logs a warning when an iteration exceeds
/// the configured interval.
///
/// When `retry` is configured, consecutive fetch failures schedule the
/// next attempt sooner than the normal cadence — see `PollScheduler` for
/// the exact rule.
pub struct ApiPollSource {
    id: String,
    url: String,
    interval: Duration,
    retry: Option<RetryPolicy>,
    source_ctx: SourceCtx,
}

impl ApiPollSource {
    pub fn new(id: impl Into<String>, url: impl Into<String>, poll_interval: Duration) -> Self {
        let id = id.into();
        Self {
            source_ctx: SourceCtx::new(&id),
            id,
            url: url.into(),
            interval: poll_interval,
            retry: None,
        }
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = Some(retry);
        self
    }
}

#[async_trait]
impl Source for ApiPollSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        self.source_ctx = SourceCtx::from_node_ctx(ctx);
    }

    async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
        let mut scheduler = PollScheduler::new(self.interval, self.retry.clone());
        let mut ticker = interval(self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker.tick().await; // first tick completes immediately

        log::info!(
            "[{}] starting poll loop at {:?}",
            redact_secret(&self.id),
            self.interval
        );
        let source_ctx = self.source_ctx.clone();

        loop {
            let start = Instant::now();

            let payload = tokio::select! {
                _ = cancel.cancelled() => {
                    log::info!("[{}] cancelled", redact_secret(&self.id));
                    return;
                }
                result = fetch(&self.url) => match result {
                    Ok(v) => v,
                    Err(e) => {
                        let delay = scheduler.record_failure();
                        log::error!(
                            "[{}] fetch failed (consecutive failures: {}), next attempt in {:?}: {e}",
                            redact_secret(&self.id),
                            scheduler.consecutive_failures(),
                            delay,
                        );
                        // Backoff bypasses the ticker. If the retry finishes
                        // before the original deadline, the next tick.tick()
                        // still blocks until that deadline — original cadence
                        // preserved. If the deadline has already passed,
                        // MissedTickBehavior::Delay schedules the next tick
                        // `interval` from now, so we don't burst-catch-up.
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(delay) => {}
                        }
                        continue;
                    }
                },
            };

            scheduler.record_success();
            log::debug!(
                "[{}] fetch completed in {:?}",
                redact_secret(&self.id),
                start.elapsed()
            );

            let env = Envelope::new(&self.id, payload);
            match source_ctx.send(&tx, env, &cancel).await {
                Ok(()) => {}
                Err(SendStopped::Cancelled) => return,
                Err(SendStopped::DownstreamClosed) => {
                    log::info!("[{}] downstream closed, stopping", redact_secret(&self.id));
                    return;
                }
            }

            let elapsed = start.elapsed();
            if elapsed > self.interval {
                log::warn!(
                    "[{}] iteration took {:?}, exceeding interval {:?}",
                    redact_secret(&self.id),
                    elapsed,
                    self.interval,
                );
            }

            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {}
            }
        }
    }
}

async fn fetch(url: &str) -> anyhow::Result<Value> {
    let resp = reqwest::get(url).await.map_err(|e| {
        let e = e.without_url();
        anyhow::anyhow!("HTTP request to {} failed: {e}", redact_secret(url))
    })?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("HTTP error: {}", resp.status()));
    }
    resp.json::<Value>().await.map_err(|e| {
        let e = e.without_url();
        anyhow::anyhow!(
            "HTTP response from {} was not valid JSON: {e}",
            redact_secret(url)
        )
    })
}

#[derive(Debug, Deserialize)]
struct ApiPollSourceConfig {
    url: String,
    interval_secs: u64,
}

/// Registry factory for [`ApiPollSource`]. Registered by
/// `courier::registry::register_builtin` under kind `"api_poll"`. The
/// optional `retry` policy is extracted by the registry and threaded into
/// the source's `PollScheduler`.
pub fn api_poll_source_factory(
    id: &str,
    config: Value,
    retry: Option<RetryPolicy>,
) -> Result<Box<dyn Source>> {
    let config: ApiPollSourceConfig = parse_config("api_poll", config)?;
    reqwest::Url::parse(&config.url).with_context(|| {
        format!(
            "invalid config for component type 'api_poll': invalid url '{}'",
            redact_secret(&config.url)
        )
    })?;
    if config.interval_secs == 0 {
        bail!("invalid config for component type 'api_poll': interval_secs must be greater than 0");
    }
    let mut source = ApiPollSource::new(id, config.url, Duration::from_secs(config.interval_secs));
    if let Some(policy) = retry {
        source = source.with_retry(policy);
    }
    Ok(Box::new(source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn closing_local_url(path: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
        });
        format!("http://{addr}{path}")
    }

    #[test]
    fn factory_rejects_invalid_url() {
        let err = api_poll_source_factory(
            "api",
            json!({
                "url": "not a url",
                "interval_secs": 60
            }),
            None,
        )
        .err()
        .expect("expected invalid URL to fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid config for component type 'api_poll'"),
            "{msg}"
        );
        assert!(msg.contains("invalid url"), "{msg}");
    }

    #[test]
    fn factory_rejects_zero_interval() {
        let err = api_poll_source_factory(
            "api",
            json!({
                "url": "http://localhost/data",
                "interval_secs": 0
            }),
            None,
        )
        .err()
        .expect("expected zero interval to fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("interval_secs must be greater than 0"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn fetch_errors_do_not_repeat_url_from_reqwest_error() {
        let url = closing_local_url("/token-in-url");

        let err = fetch(&url).await.expect_err("expected connection failure");
        let msg = format!("{err:#}");
        assert_eq!(msg.matches(&url).count(), 1, "{msg}");
    }

    #[tokio::test]
    async fn emits_envelope_per_poll() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "v": 1 })))
            .mount(&server)
            .await;

        let url = format!("{}/data", server.uri());
        let source = ApiPollSource::new("api", url, Duration::from_millis(20));
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let c = cancel.clone();
        let handle = tokio::spawn(async move { Box::new(source).run(tx, c).await });

        let env = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("poll timed out")
            .expect("source closed before emitting");

        assert_eq!(env.meta.source_id, "api");
        assert_eq!(env.payload, json!({ "v": 1 }));

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn recovers_from_transient_http_error() {
        let server = MockServer::start().await;
        // First response is a 500; subsequent requests succeed.
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
            .mount(&server)
            .await;

        let url = format!("{}/data", server.uri());
        let source = ApiPollSource::new("api", url, Duration::from_millis(20));
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let c = cancel.clone();
        let handle = tokio::spawn(async move { Box::new(source).run(tx, c).await });

        let env = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("poll timed out after retry")
            .expect("source closed before emitting");
        assert_eq!(env.payload, json!({ "ok": true }));

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn retry_backoff_recovers_faster_than_polling_interval() {
        use crate::retry::{ExhaustedPolicy, RetryPolicy};
        use std::time::Instant;

        // Interval is 5s — without retry, recovery from a transient 500 would
        // wait the full 5s. With retry (initial 20ms < interval), the next
        // attempt fires far sooner. This is the whole point of the policy:
        // retry can compress the cadence below interval to recover quickly.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
            .mount(&server)
            .await;

        let url = format!("{}/data", server.uri());
        let source =
            ApiPollSource::new("api", url, Duration::from_secs(5)).with_retry(RetryPolicy {
                max_attempts: 5,
                initial_delay_ms: 20,
                backoff_multiplier: 2.0,
                max_delay_ms: 1_000,
                on_exhausted: ExhaustedPolicy::Propagate,
            });
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let c = cancel.clone();
        let started = Instant::now();
        let handle = tokio::spawn(async move { Box::new(source).run(tx, c).await });

        let env = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("poll timed out — retry did not compress the cadence")
            .expect("source closed before emitting");
        assert_eq!(env.payload, json!({ "ok": true }));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "recovery took {:?}, retry should have fired well under interval (5s)",
            started.elapsed(),
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn retry_is_disregarded_when_polling_interval_is_smaller() {
        use crate::retry::{ExhaustedPolicy, RetryPolicy};
        use std::time::Instant;

        // Interval (50ms) is shorter than initial backoff (5s). The policy
        // says: when interval < retry, disregard retry — i.e. the next
        // attempt fires at the polling cadence, not after the longer
        // backoff. Recovery completes well under the backoff window.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
            .mount(&server)
            .await;

        let url = format!("{}/data", server.uri());
        let source =
            ApiPollSource::new("api", url, Duration::from_millis(50)).with_retry(RetryPolicy {
                max_attempts: 5,
                initial_delay_ms: 5_000,
                backoff_multiplier: 2.0,
                max_delay_ms: 30_000,
                on_exhausted: ExhaustedPolicy::Propagate,
            });
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let c = cancel.clone();
        let started = Instant::now();
        let handle = tokio::spawn(async move { Box::new(source).run(tx, c).await });

        let env = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("poll timed out — interval should override the larger backoff")
            .expect("source closed before emitting");
        assert_eq!(env.payload, json!({ "ok": true }));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "recovery took {:?}, interval should have overridden the 5s backoff",
            started.elapsed(),
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn stops_when_cancelled() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let url = format!("{}/data", server.uri());
        // Long interval: after the first tick the source sits in `wait_next`
        // until either the interval elapses or cancel fires.
        let source = ApiPollSource::new("api", url, Duration::from_secs(60));
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let c = cancel.clone();
        let handle = tokio::spawn(async move { Box::new(source).run(tx, c).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        let res = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(res.is_ok(), "source did not exit within 1s of cancel");
    }
}
