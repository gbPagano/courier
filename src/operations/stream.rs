use async_trait::async_trait;
use futures::StreamExt;
use std::time::Instant;

use super::Operation;
use crate::readers::StreamReader;
use crate::writers::Writer;

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
