//! Test helpers shared across integration tests.
//!
//! Kept out of `src/` on purpose: only needed by files under `tests/`,
//! so there's no reason to expose it in the public crate API.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use courier::envelope::Envelope;
use courier::sinks::WriteOne;
use courier::sources::Source;

/// Source that emits a preset list of envelopes, then closes `tx` so
/// downstream stages drain and exit.
pub struct VecSource {
    id: String,
    items: Vec<Envelope>,
}

impl VecSource {
    pub fn new(id: impl Into<String>, items: Vec<Envelope>) -> Self {
        Self {
            id: id.into(),
            items,
        }
    }
}

#[async_trait]
impl Source for VecSource {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
        for env in self.items {
            tokio::select! {
                _ = cancel.cancelled() => return,
                res = tx.send(env) => {
                    if res.is_err() { return }
                }
            }
        }
    }
}

/// `WriteOne` sink that records every envelope it receives into a shared
/// `Vec`. Call [`CollectingSink::handle`] before moving the sink into the
/// pipeline to keep a reference the test can inspect afterwards.
pub struct CollectingSink {
    id: String,
    store: Arc<Mutex<Vec<Envelope>>>,
}

impl CollectingSink {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            store: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[allow(dead_code)]
    pub fn from_store(id: impl Into<String>, store: Arc<Mutex<Vec<Envelope>>>) -> Self {
        Self {
            id: id.into(),
            store,
        }
    }

    pub fn handle(&self) -> Arc<Mutex<Vec<Envelope>>> {
        Arc::clone(&self.store)
    }
}

#[async_trait]
impl WriteOne for CollectingSink {
    fn id(&self) -> &str {
        &self.id
    }

    async fn write(&self, env: &Envelope) -> Result<()> {
        self.store.lock().unwrap().push(env.clone());
        Ok(())
    }
}
