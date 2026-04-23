use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::Sender;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::config::parse_config;
use crate::envelope::Envelope;
use crate::sources::Source;

/// Polls an HTTP endpoint at a fixed interval, emitting each JSON response
/// body as the envelope payload. Logs a warning when an iteration exceeds
/// the configured interval.
pub struct ApiPollSource {
    id: String,
    url: String,
    interval: Duration,
}

impl ApiPollSource {
    pub fn new(id: impl Into<String>, url: impl Into<String>, poll_interval: Duration) -> Self {
        Self {
            id: id.into(),
            url: url.into(),
            interval: poll_interval,
        }
    }
}

#[async_trait]
impl Source for ApiPollSource {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
        let mut ticker = interval(self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker.tick().await; // first tick completes immediately

        log::info!("[{}] starting poll loop at {:?}", self.id, self.interval);

        loop {
            let start = Instant::now();

            let payload = tokio::select! {
                _ = cancel.cancelled() => {
                    log::info!("[{}] cancelled", self.id);
                    return;
                }
                result = fetch(&self.url) => match result {
                    Ok(v) => v,
                    Err(e) => {
                        log::error!("[{}] fetch failed: {e}", self.id);
                        self.wait_next(&mut ticker, &cancel).await;
                        if cancel.is_cancelled() { return; }
                        continue;
                    }
                },
            };

            log::debug!("[{}] fetch completed in {:?}", self.id, start.elapsed());

            let env = Envelope::new(&self.id, payload);
            tokio::select! {
                _ = cancel.cancelled() => return,
                res = tx.send(env) => {
                    if res.is_err() {
                        log::info!("[{}] downstream closed, stopping", self.id);
                        return;
                    }
                }
            }

            let elapsed = start.elapsed();
            if elapsed > self.interval {
                log::warn!(
                    "[{}] iteration took {:?}, exceeding interval {:?}",
                    self.id,
                    elapsed,
                    self.interval,
                );
            }

            self.wait_next(&mut ticker, &cancel).await;
            if cancel.is_cancelled() {
                return;
            }
        }
    }
}

impl ApiPollSource {
    async fn wait_next(&self, ticker: &mut tokio::time::Interval, cancel: &CancellationToken) {
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = ticker.tick() => {}
        }
    }
}

async fn fetch(url: &str) -> anyhow::Result<Value> {
    let resp = reqwest::get(url).await?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("HTTP error: {}", resp.status()));
    }
    Ok(resp.json::<Value>().await?)
}

#[derive(Debug, Deserialize)]
struct ApiPollSourceConfig {
    url: String,
    interval_secs: u64,
}

/// Registry factory for [`ApiPollSource`]. Registered by
/// `courier::registry::register_builtin` under kind `"api_poll"`.
pub fn api_poll_source_factory(id: &str, config: Value) -> Result<Box<dyn Source>> {
    let config: ApiPollSourceConfig = parse_config("api_poll", config)?;
    Ok(Box::new(ApiPollSource::new(
        id,
        config.url,
        Duration::from_secs(config.interval_secs),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
