use futures::StreamExt;

use crate::readers::StreamReader;
use crate::writers::Writer;

pub struct StreamOperation<R, W>
where
    R: StreamReader,
    W: Writer,
{
    reader: R,
    writer: W,
}

impl<R, W> StreamOperation<R, W>
where
    R: StreamReader,
    W: Writer,
    W::Item: From<R::Item>,
{
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    pub async fn run(&self) {
        let my_stream = self.reader.stream().await;
        tokio::pin!(my_stream);
        while let Some(value) = my_stream.next().await {
            let _ = self.writer.write(value.into()).await;
        }
    }
}
