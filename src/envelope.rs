use std::collections::HashMap;
use std::time::SystemTime;

use serde_json::Value;

/// The single wire type flowing between all pipeline nodes.
///
/// Generics stop at the node boundary: every `Source`, `Transform`, and `Sink`
/// works on `Envelope` regardless of the underlying schema. Strongly-typed
/// payloads are opt-in via transforms that deserialize, map, and re-serialize.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub meta: Meta,
    pub payload: Value,
}

/// Per-envelope metadata.
///
/// `key` is populated by the source when available (e.g., Kafka record key)
/// or by a transform like `SetKeyTransform`. Sinks that require a key (like
/// `KafkaSink`) fall back to `source_id` when `key` is `None`.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub key: Option<String>,
    pub source_id: String,
    pub timestamp_ms: u64,
    pub headers: HashMap<String, String>,
}

impl Envelope {
    pub fn new(source_id: impl Into<String>, payload: Value) -> Self {
        Self {
            meta: Meta {
                source_id: source_id.into(),
                timestamp_ms: now_ms(),
                ..Meta::default()
            },
            payload,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
