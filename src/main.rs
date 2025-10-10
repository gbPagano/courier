use std::time::Duration;

use courier::readers::kafka::KafkaReader;
use courier::writers::kafka::KafkaWriter;
use serde::{Deserialize, Serialize};

use courier::operations::{self, Operation, StreamOperation};
use courier::readers::api::ApiReader;

#[derive(Serialize, Deserialize, Debug)]
pub struct ApiItalo {
    message: String,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let api_reader = ApiReader::new("http://192.168.1.4:8000").with_type::<ApiItalo>();
    let kafka_reader: KafkaReader<ApiItalo> =
        KafkaReader::new("localhost:9092", "group-id-1", vec!["simple-producer"]);

    let writer: KafkaWriter<ApiItalo> = KafkaWriter::new("localhost:9092", "simple-producer2");

    let operation = StreamOperation::new("kafka-kafka", kafka_reader, writer);
    // let operation = Operation::new(
    //     "Italo-helloworld-2",
    //     api_reader,
    //     writer,
    //     Duration::from_secs(5),
    // );

    operation.run().await;
}
