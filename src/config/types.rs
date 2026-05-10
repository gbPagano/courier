use std::fmt;

use serde_json::Value;

use crate::pipeline::ErrorPolicy;
use crate::retry::RetryPolicy;

use super::redact::{RedactedJsonValue, RedactedOptionRetryPolicy, RedactedStr};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthConfig {
    pub address: String,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            address: "0.0.0.0:9090".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShutdownConfig {
    pub timeout_secs: u64,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self { timeout_secs: 30 }
    }
}

#[derive(Clone, Default, PartialEq)]
pub struct Config {
    pub pipelines: Vec<PipelineSpec>,
    pub observability: Option<super::observability::ObservabilityConfig>,
    pub health: Option<HealthConfig>,
    pub shutdown: Option<ShutdownConfig>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("pipelines", &self.pipelines)
            .field("observability", &self.observability)
            .field("health", &self.health)
            .field("shutdown", &self.shutdown)
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct PipelineSpec {
    pub name: String,
    pub source: SourceSpec,
    pub transforms: Vec<TransformSpec>,
    pub sinks: Vec<SinkSpec>,
    pub channel_capacity: Option<usize>,
    pub fan_out: FanOutPolicyConfig,
}

#[derive(Clone, PartialEq)]
pub struct SourceSpec {
    pub kind: String,
    pub config: Value,
    pub retry: Option<RetryPolicy>,
}

#[derive(Clone, PartialEq)]
pub struct TransformSpec {
    pub kind: String,
    pub config: Value,
    pub on_error: Option<ErrorPolicyConfig>,
}

#[derive(Clone, PartialEq)]
pub struct SinkSpec {
    pub kind: String,
    pub config: Value,
    pub on_error: Option<ErrorPolicyConfig>,
    pub retry: Option<RetryPolicy>,
}

impl fmt::Debug for PipelineSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineSpec")
            .field("name", &RedactedStr(&self.name))
            .field("source", &self.source)
            .field("transforms", &self.transforms)
            .field("sinks", &self.sinks)
            .field("channel_capacity", &self.channel_capacity)
            .field("fan_out", &self.fan_out)
            .finish()
    }
}

impl fmt::Debug for SourceSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceSpec")
            .field("kind", &RedactedStr(&self.kind))
            .field("config", &RedactedJsonValue(&self.config))
            .field("retry", &RedactedOptionRetryPolicy(self.retry.as_ref()))
            .finish()
    }
}

impl fmt::Debug for TransformSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransformSpec")
            .field("kind", &RedactedStr(&self.kind))
            .field("config", &RedactedJsonValue(&self.config))
            .field("on_error", &self.on_error)
            .finish()
    }
}

impl fmt::Debug for SinkSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SinkSpec")
            .field("kind", &RedactedStr(&self.kind))
            .field("config", &RedactedJsonValue(&self.config))
            .field("on_error", &self.on_error)
            .field("retry", &RedactedOptionRetryPolicy(self.retry.as_ref()))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum ErrorPolicyConfig {
    #[default]
    Drop,
    FailPipeline,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum FanOutPolicyConfig {
    #[default]
    Broadcast,
    BroadcastWithDrop,
}

impl FanOutPolicyConfig {
    pub fn as_str(self) -> &'static str {
        match self {
            FanOutPolicyConfig::Broadcast => "broadcast",
            FanOutPolicyConfig::BroadcastWithDrop => "broadcast_with_drop",
        }
    }
}

impl From<ErrorPolicyConfig> for ErrorPolicy {
    fn from(value: ErrorPolicyConfig) -> Self {
        match value {
            ErrorPolicyConfig::Drop => ErrorPolicy::Drop,
            ErrorPolicyConfig::FailPipeline => ErrorPolicy::FailPipeline,
        }
    }
}

impl From<FanOutPolicyConfig> for crate::pipeline::FanOutPolicy {
    fn from(value: FanOutPolicyConfig) -> Self {
        match value {
            FanOutPolicyConfig::Broadcast => crate::pipeline::FanOutPolicy::Broadcast,
            FanOutPolicyConfig::BroadcastWithDrop => {
                crate::pipeline::FanOutPolicy::BroadcastWithDrop
            }
        }
    }
}
