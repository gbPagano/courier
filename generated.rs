use courier::operations::*;
use courier::readers::api::ApiReader;
use courier::readers::kafka::KafkaReader;
use courier::writers::kafka::KafkaWriter;
use courier::Courier;
use serde_json::Value;
use std::time::Duration;
pub fn courier_from_config() -> Courier {
    let mut operations: Vec<Box<dyn Operation>> = Vec::new();
    {
        let reader =
            KafkaReader::<Value>::new("localhost:9092", "user-events-consumer", vec!["topic1"]);
        let writer = KafkaWriter::<Value>::new("localhost:9092", "topic2");
        let operation = StreamOperation::new("kafka->kafka", reader, writer);
        operations.push(Box::new(operation));
    }
    {
        let reader = ApiReader::new("http://192.168.47.204:8000").with_type::<Value>();
        let writer = KafkaWriter::<Value>::new("localhost:9092", "topic1");
        let operation =
            IntervalOperation::new("apiitalo->kafka", reader, writer, Duration::from_secs(3u64));
        operations.push(Box::new(operation));
    }
    {
        let reader = ApiReader::new("http://192.168.47.204:8000").with_type::<Value>();
        let mut operation =
            IntervalFanoutOperation::new("api->multi-kakfa", reader, Duration::from_secs(5u64));
        operation.add_writer(KafkaWriter::<Value>::new("localhost:9092", "topic3"));
        operation.add_writer(KafkaWriter::<Value>::new("localhost:9092", "topic4"));
        operations.push(Box::new(operation));
    }
    Courier::new(operations)
}
