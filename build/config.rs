use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub operations: Vec<OperationConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum OperationConfig {
    #[serde(rename = "Interval")]
    Interval {
        name: String,
        reader: ReaderConfig,
        writer: WriterConfig,
        interval_secs: u64,
    },
    #[serde(rename = "Stream")]
    Stream {
        name: String,
        reader: ReaderConfig,
        writer: WriterConfig,
    },
    #[serde(rename = "IntervalFanout")]
    IntervalFanout {
        name: String,
        reader: ReaderConfig,
        writers: Vec<WriterConfig>,
        interval_secs: u64,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ReaderConfig {
    #[serde(rename = "kafka")]
    KafkaReader {
        brokers: String,
        group_id: String,
        topics: Vec<String>,
        data_type: String,
    },
    #[serde(rename = "api")]
    ApiReader { url: String, data_type: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum WriterConfig {
    #[serde(rename = "kafka")]
    KafkaWriter {
        brokers: String,
        topic: String,
        data_type: String,
    },
}
