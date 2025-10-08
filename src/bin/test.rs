use async_stream::stream;
use courier::writers::Writer;
use courier::writers::kafka::KafkaWriter;
use std::time::Duration;
use tokio_stream::{Stream, StreamExt};

use courier::operations::{self, StreamOperation};
use courier::readers::StreamReader;
use courier::readers::kafka::{KafkaMessage, KafkaReader};

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let reader: KafkaReader<String, String> =
        KafkaReader::new("localhost:9092", "test-group-id", vec!["simple-producer"]);

    let writer: KafkaWriter<String, String> = KafkaWriter::new("localhost:9092", "simple-producer");

    let operation = StreamOperation::new(reader, writer);
    operation.run().await;
}
