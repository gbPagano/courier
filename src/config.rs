use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::pipeline::ErrorPolicy;

/// Shared helper for factory authors: deserialize a component spec into
/// a typed config and wrap any failure with a uniform
/// "invalid config for component type '{kind}'" context. Using this
/// keeps error messages consistent between built-in and third-party
/// factories.
pub fn parse_config<T: DeserializeOwned>(kind: &str, config: Value) -> Result<T> {
    serde_json::from_value(config)
        .with_context(|| format!("invalid config for component type '{kind}'"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub pipelines: Vec<PipelineSpec>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PipelineSpec {
    pub name: String,
    pub source: SourceSpec,
    pub transforms: Vec<TransformSpec>,
    pub sinks: Vec<SinkSpec>,
    pub channel_capacity: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceSpec {
    pub kind: String,
    pub config: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransformSpec {
    pub kind: String,
    pub config: Value,
    pub on_error: Option<ErrorPolicyConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SinkSpec {
    pub kind: String,
    pub config: Value,
    pub on_error: Option<ErrorPolicyConfig>,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum ErrorPolicyConfig {
    #[default]
    Drop,
    FailPipeline,
}

impl From<ErrorPolicyConfig> for ErrorPolicy {
    fn from(value: ErrorPolicyConfig) -> Self {
        match value {
            ErrorPolicyConfig::Drop => ErrorPolicy::Drop,
            ErrorPolicyConfig::FailPipeline => ErrorPolicy::FailPipeline,
        }
    }
}
