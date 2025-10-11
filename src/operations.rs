use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::time::{Interval, MissedTickBehavior, interval, sleep};

use crate::readers::{Reader, StreamReader};
use crate::writers::Writer;

#[async_trait]
pub trait Operation: Send + Sync {
    async fn run(&self);
}

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

pub struct StreamOperation<R, W>
where
    R: StreamReader,
    W: Writer,
{
    reader: R,
    writer: W,
    id: String,
}

impl<R, W> StreamOperation<R, W>
where
    R: StreamReader,
    W: Writer,
    W::Item: From<R::Item>,
{
    pub fn new(id: &str, reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            id: id.into(),
        }
    }
}

#[async_trait]
impl<R, W> Operation for StreamOperation<R, W>
where
    R: StreamReader,
    W: Writer,
    W::Item: From<R::Item>,
{
    async fn run(&self) {
        log::info!("[{}] Starting stream processing", self.id);

        let stream = self.reader.stream().await;
        tokio::pin!(stream);

        log::info!("[{}] Waiting for incoming data", self.id);
        let mut waiting_time = Instant::now();

        while let Some(value) = stream.next().await {
            let time_to_receive = waiting_time.elapsed();
            log::debug!("[{}] Data received after {:?}", self.id, time_to_receive);
            log::info!("[{}] Successfully read data, writing...", self.id);

            let write_start = Instant::now();
            match self.writer.write(value.into()).await {
                Ok(_) => {
                    log::info!("[{}] Successfully wrote data", self.id);
                    log::debug!("[{}] Write took {:.2?}", self.id, write_start.elapsed());
                }
                Err(e) => log::error!("[{}] Failed to write data: {:?}", self.id, e),
            }

            log::info!("[{}] Waiting for incoming data", self.id);
            waiting_time = Instant::now();
        }

        log::warn!("[{}] Stream ended unexpectedly", self.id);
    }
}
