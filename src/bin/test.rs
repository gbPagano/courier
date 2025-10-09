use async_stream::stream;
use courier::writers::Writer;
use courier::writers::kafka::KafkaWriter;
use rdkafka::message::OwnedHeaders;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_stream::{Stream, StreamExt};

use courier::operations::{self, Operation, StreamOperation};
use courier::readers::api::ApiReader;
use courier::readers::kafka::{KafkaMessage, KafkaReader};
use courier::readers::{Reader, StreamReader};

#[derive(Serialize, Deserialize, Debug)]
pub struct ApiItalo {
    message: String,
}

impl From<ApiItalo> for KafkaMessage<String, String> {
    fn from(value: ApiItalo) -> Self {
        KafkaMessage::new("message".to_owned(), value.message, OwnedHeaders::new())
    }
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let api_reader = ApiReader::new("http://192.168.1.2:8000").with_type::<ApiItalo>();
    // let api_reader = ApiReader::new("http://192.168.1.2:8000/asd").with_type::<ApiItalo>();

    // let result = api_reader.read().await;
    // dbg!(result);

    // let reader: KafkaReader<String, String> =
    //     KafkaReader::new("localhost:9092", "test-group-id", vec!["simple-producer"]);
    //
    let writer: KafkaWriter<String, String> = KafkaWriter::new("localhost:9092", "simple-producer");
    //
    let operation = Operation::new(
        "Italo-helloworld",
        api_reader,
        writer,
        Duration::from_secs(2),
    );
    //
    operation.run().await;

    //
    //
    // let operation = StreamOperation::new(reader, writer);
    // operation.run().await;
}
