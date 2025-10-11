use async_trait::async_trait;
use std::time::{Duration, Instant};
use tokio::time::{MissedTickBehavior, interval};

use super::Operation;
use crate::readers::Reader;
use crate::writers::Writer;

pub struct IntervalOperation<R, W>
where
    R: Reader,
    W: Writer,
{
    reader: R,
    writer: W,
    interval: Duration,
    id: String,
}

impl<R, W> IntervalOperation<R, W>
where
    R: Reader,
    W: Writer,
    W::Item: From<R::Item>,
{
    pub fn new(id: &str, reader: R, writer: W, interval: Duration) -> Self {
        Self {
            reader,
            writer,
            interval,
            id: id.into(),
        }
    }
}

#[async_trait]
impl<R, W> Operation for IntervalOperation<R, W>
where
    R: Reader,
    W: Writer,
    W::Item: From<R::Item>,
{
    async fn run(&self) {
        let mut interval = interval(self.interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await; // The first tick completes immediately.
        log::info!(
            "[{}] Starting operation loop with interval {:?}",
            self.id,
            self.interval
        );

        loop {
            let start = Instant::now();

            log::info!("[{}] Reading data", self.id);
            match self.reader.read().await {
                Ok(data) => {
                    log::debug!("[{}] Read completed in {:?}", self.id, start.elapsed());
                    log::info!("[{}] Successfully read data, writing...", self.id);
                    if let Err(e) = self.writer.write(data.into()).await {
                        log::error!("[{}] Failed to write data: {:?}", self.id, e);
                    } else {
                        log::info!("[{}] Successfully wrote data", self.id);
                    }
                }
                Err(e) => {
                    log::error!("[{}] Failed to read data: {:?}", self.id, e);
                }
            }

            let elapsed = start.elapsed();
            if elapsed > self.interval {
                log::warn!(
                    "[{}] Loop iteration took {:?}, which exceeds the configured interval of {:?}",
                    self.id,
                    elapsed,
                    self.interval,
                );
            } else {
                log::debug!("[{}] Loop iteration completed in {:?}", self.id, elapsed);
            }

            interval.tick().await;
        }
    }
}
