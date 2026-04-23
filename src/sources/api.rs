use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc::Sender;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

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
