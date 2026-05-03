use anyhow::{Result, bail};
use serde::Deserialize;

use crate::config::parse_config;
use crate::sources::Source;
use crate::{RetryPolicy, observability::SourceCtx};

pub struct ObjectStorageSource {
    id: String,
    config: ObjectStorageSourceConfig,
    source_ctx: SourceCtx,
}

impl ObjectStorageSource {
    pub fn new(
        id: impl Into<String>,
        provider: &str,
        endpoint: &str,
        bucket: &str,
        prefix: &str,
        format: &str,
    ) -> Result<Self> {
        // todo
    }
}

#[derive(Debug, Deserialize)]
struct ObjectStorageSourceConfig {
    provider: String,
    endpoint: String,
    bucket: String,
    prefix: String,
    format: String,
}

pub fn object_storage_source_factory(
    id: &str,
    config: Value,
    retry: Option<RetryPolicy>,
) -> Result<Box<dyn Source>> {
    let config: ObjectStorageSourceConfig = parse_config("object_storage", config)?;

    if config.provider.trim().is_empty() {
        bail!("invalid config for component type 'object_storage': provider must not be empty");
    }

    if config.endpoint.trim().is_empty() {
        bail!("invalid config for component type 'object_storage': endpoint must not be empty");
    }

    if config.bucket.trim().is_empty() {
        bail!("invalid config for component type 'object_storage': bucket must not be empty");
    }

    if config.prefix.trim().is_empty() {
        bail!("invalid config for component type 'object_storage': prefix must not be empty");
    }

    if config.format.trim().is_empty() {
        bail!("invalid config for component type 'object_storage': format msut not be empty");
    }

    Ok(Box::new(ObjectStorageSource::new(
        id,
        &config.provider,
        &config.endpoint,
        &config.bucket,
        &config.prefix,
        &config.format,
    )?))
}
