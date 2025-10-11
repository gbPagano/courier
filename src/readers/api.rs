use std::fmt::Debug;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::readers::Reader;

pub struct ApiReader<T> {
    url: String,
    _phantom: std::marker::PhantomData<T>,
}

impl ApiReader<Value> {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.into(),
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn with_type<T>(self) -> ApiReader<T> {
        ApiReader {
            url: self.url,
            _phantom: std::marker::PhantomData,
        }
    }
}

#[async_trait]
impl<T> Reader for ApiReader<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + Sync + Debug,
{
    type Item = T;

    async fn read(&self) -> Result<Self::Item> {
        let response = match reqwest::get(&self.url).await {
            Ok(resp) => resp,
            Err(e) => {
                if e.is_timeout() {
                    log::error!("Request timeout for {}", self.url);
                } else if e.is_connect() {
                    log::error!("Connection failed for {}: {}", self.url, e);
                } else if e.is_request() {
                    log::error!("Request error for {}: {}", self.url, e);
                } else {
                    log::error!("Unknown error for {}: {}", self.url, e);
                }
                return Err(e.into());
            }
        };
        log::trace!(
            "Response received from {} with status: {}",
            self.url,
            response.status()
        );

        if !response.status().is_success() {
            log::error!("HTTP error {}: {}", response.status(), self.url);
            return Err(anyhow!("HTTP error: {}", response.status()));
        }

        let data: T = response.json().await.map_err(|e| {
            log::error!("Failed to parse JSON from {}: {}", self.url, e);
            e
        })?;
        log::debug!("Successfully retrieved data from: {}", self.url);
        log::trace!("Received payload: {data:?}");

        Ok(data)
    }
}
