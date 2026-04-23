use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub pipelines: Vec<PipelineConfig>,
}

#[derive(Debug, Deserialize)]
pub struct PipelineConfig {
    pub name: String,
    pub source: SourceConfig,
    #[serde(default)]
    pub transforms: Vec<TransformConfig>,
    #[serde(default)]
    pub sinks: Vec<SinkConfig>,
    #[serde(default)]
    pub channel_capacity: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum SourceConfig {
    #[serde(rename = "api_poll")]
    ApiPoll { url: String, interval_secs: u64 },
    #[serde(rename = "kafka")]
    Kafka {
        brokers: String,
        group_id: String,
        topics: Vec<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum TransformConfig {
    #[serde(rename = "set_key")]
    SetKey {
        from_field: String,
        #[serde(default)]
        on_error: Option<ErrorPolicyConfig>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum SinkConfig {
    #[serde(rename = "kafka")]
    Kafka {
        brokers: String,
        topic: String,
        #[serde(default)]
        on_error: Option<ErrorPolicyConfig>,
    },
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ErrorPolicyConfig {
    Drop,
    FailPipeline,
}
