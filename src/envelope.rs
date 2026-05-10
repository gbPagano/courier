use std::collections::HashMap;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The single wire type flowing between all pipeline nodes.
///
/// Generics stop at the node boundary: every `Source`, `Transform`, and `Sink`
/// works on `Envelope` regardless of the underlying schema. Strongly-typed
/// payloads are opt-in via transforms that deserialize, map, and re-serialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub meta: Meta,
    pub payload: Value,
}

/// Per-envelope metadata.
///
/// `key` is populated by the source when available (e.g., Kafka record key)
/// or by a transform like `SetKeyTransform`. Sinks that require a key (like
/// `KafkaSink`) fall back to `source_id` when `key` is `None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A dead-letter entry written to a JSONL file when a sink exhausts its
/// retry budget with `ExhaustedPolicy::DeadLetter`.
///
/// The `envelope` field round-trips through `Envelope`'s serde impl
/// without manual transformation, so a `jsonl_file` source (or any
/// replay tool) can deserialize a line directly and extract a valid
/// `Envelope`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetterEntry {
    /// The envelope that failed to write.
    pub envelope: Envelope,
    /// Human-readable summary of the last error.
    pub error: String,
    /// Pipeline that produced the dead letter (e.g. `"my-pipeline"`).
    pub pipeline: String,
    /// Sink node id that produced the dead letter (e.g. `"my-pipeline/sink0"`).
    pub sink: String,
    /// Epoch millis when the dead-letter entry was written.
    pub dead_lettered_at_ms: u64,
    /// Total number of write attempts before the dead letter was written.
    pub attempts: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_sets_source_id_and_defaults() {
        let e = Envelope::new("src", json!({ "a": 1 }));
        assert_eq!(e.meta.source_id, "src");
        assert!(e.meta.key.is_none());
        assert!(e.meta.headers.is_empty());
        assert!(e.meta.timestamp_ms > 0);
        assert_eq!(e.payload, json!({ "a": 1 }));
    }
}
