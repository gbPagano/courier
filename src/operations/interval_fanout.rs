use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use futures::future;
use tokio::time::{MissedTickBehavior, interval};

use super::Operation;
use crate::readers::Reader;
use crate::writers::Writer;

#[async_trait]
trait WriterBox<T>: Send + Sync {
    async fn write(&self, item: &T) -> Result<()>;
}

#[async_trait]
impl<T, W> WriterBox<T> for W
where
    W: Writer,
    W::Item: From<T>,
    T: Clone + Sync,
{
    async fn write(&self, item: &T) -> Result<()> {
        let converted = W::Item::from(item.clone());
        self.write(converted).await
    }
}

pub struct IntervalFanoutOperation<R>
where
    R: Reader,
{
    reader: R,
    writers: Vec<Box<dyn WriterBox<R::Item>>>,
    interval: Duration,
    id: String,
}

impl<R> IntervalFanoutOperation<R>
where
    R: Reader,
    R::Item: Clone,
{
    pub fn new(id: &str, reader: R, interval: Duration) -> Self {
        Self {
            reader,
            writers: Vec::new(),
            interval,
            id: id.into(),
        }
    }

    pub fn add_writer<W>(&mut self, writer: W)
    where
        W: Writer + 'static,
        W::Item: From<R::Item>,
    {
        self.writers.push(Box::new(writer));
    }

    pub fn with_writer<W>(mut self, writer: W) -> Self
    where
        W: Writer + 'static,
        W::Item: From<R::Item>,
    {
        self.add_writer(writer);
        self
    }
}

#[async_trait]
impl<R> Operation for IntervalFanoutOperation<R>
where
    R: Reader,
    R::Item: Clone,
{
    async fn run(&self) {
        let mut interval = interval(self.interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await; // The first tick completes immediately.

        if self.writers.is_empty() {
            log::error!(
                "[{}] Cannot start operation: no writers configured",
                self.id
            );
            return;
        }

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

                    let write_futures = self.writers.iter().map(|writer| {
                        let data_clone = data.clone();
                        async move {
                            if let Err(e) = writer.write((&data_clone).into()).await {
                                log::error!("[{}] Failed to write data: {:?}", self.id, e);
                            } else {
                                log::info!("[{}] Successfully wrote data", self.id);
                            }
                        }
                    });
                    future::join_all(write_futures).await;
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
