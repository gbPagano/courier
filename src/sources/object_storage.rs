use anyhow::{Result, bail};
use async_trait::async_trait;
use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use serde::Deserialize;
use std::env;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::config::parse_config;
use crate::envelope::Envelope;
use crate::observability::{NodeCtx, SourceCtx};
use crate::sources::Source;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectStorageProvider {
    LocalFS,
    S3,
}

#[derive(Debug, Deserialize)]
struct ObjectStorageSourceConfig {
    provider: ObjectStorageProvider,
    region: Option<String>,
    endpoint: String,
    bucket: String,
    prefix: String,
    format: String,
}

pub struct ObjectStorageSource {
    id: String,
    store: Box<dyn ObjectStore>,
    config: ObjectStorageSourceConfig,
    source_ctx: SourceCtx,
}

impl ObjectStorageSource {
    pub fn new(
        id: impl Into<String>,
        config: ObjectStorageSourceConfig,
        source_ctx: SourceCtx,
    ) -> Result<Self> {
        let store: Box<dyn ObjectStore> = match config.provider {
            ObjectStorageProvider::LocalFS => Box::new(LocalFileSystem::new()),
            ObjectStorageProvider::S3 => {
                let region = config.region.as_deref().unwrap_or_default();
                Box::new(
                    AmazonS3Builder::new()
                        .with_region(region)
                        .with_bucket_name(&config.bucket)
                        .with_access_key_id(env::var("AWS_ACCESS_KEY_ID").unwrap())
                        .with_secret_access_key(env::var("AWS_SECRET_ACCESS_KEY").unwrap())
                        .build()
                        .unwrap(),
                )
            }
        };

        Ok(Self {
            id: id.into(),
            store,
            config,
            source_ctx,
        })
    }
}

#[async_trait]
impl Source for ObjectStorageSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        self.source_ctx = SourceCtx::from_node_ctx(ctx);
    }

    async fn run(
        &mut self,
        tx: Sender<Envelope>,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        // your logic using self.store.list(), self.store.get(), etc.
        todo!()
    }
}

pub fn object_storage_source_factory(
    id: &str,
    config: ObjectStorageSourceConfig,
) -> Result<Box<dyn Source>> {
    let config: ObjectStorageSourceConfig = parse_config("object_storage", config)?;

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
        config,
        SourceCtx::new(id),
    )?))
}
