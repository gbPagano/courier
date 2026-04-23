//! End-to-end tests for the registry: register custom + built-in
//! components, build a `Courier` from a `Config`, and verify envelopes
//! flow through the resolved pipeline.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures::future::join_all;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use courier::Registry;
use courier::config::{
    Config, ErrorPolicyConfig, PipelineSpec, SinkSpec, SourceSpec, TransformSpec,
};
use courier::envelope::Envelope;
use courier::pipeline::ErrorPolicy;
use courier::sinks::{BasicSink, Sink};
use courier::sources::Source;
use courier::transforms::set_key::SetKeyTransform;
use courier::transforms::{BasicTransform, Transform};

mod common;
use common::{CollectingSink, VecSource};

/// Factory for `VecSource` — reads a list of JSON payloads from the spec
/// and emits one `Envelope` per entry.
#[derive(Deserialize)]
struct VecSourceSpec {
    items: Vec<Value>,
}

fn vec_source_factory(id: &str, config: Value) -> Result<Box<dyn Source>> {
    let spec: VecSourceSpec = serde_json::from_value(config)?;
    let envs = spec
        .items
        .into_iter()
        .map(|payload| Envelope::new(id, payload))
        .collect();
    Ok(Box::new(VecSource::new(id, envs)))
}

/// Built-in `set_key` factory reimplemented locally so this test doesn't
/// depend on the lib's internal `register_builtin` — verifies the
/// extension surface is enough for an external crate to plug in.
#[derive(Deserialize)]
struct SetKeySpec {
    from_field: String,
}

fn set_key_factory(id: &str, config: Value, on_error: ErrorPolicy) -> Result<Box<dyn Transform>> {
    let spec: SetKeySpec = serde_json::from_value(config)?;
    Ok(Box::new(
        BasicTransform::new(SetKeyTransform::new(id, spec.from_field)).with_error_policy(on_error),
    ))
}

type SinkHandle = Arc<Mutex<Vec<Envelope>>>;
type SinkMap = Arc<Mutex<std::collections::HashMap<String, SinkHandle>>>;

/// Captures every envelope for inspection. Stores handles per sink id so
/// we can fan out to multiple sinks and check each independently.
#[derive(Clone, Default)]
struct SinkRegistry {
    sinks: SinkMap,
}

impl SinkRegistry {
    fn factory(
        &self,
    ) -> impl Fn(&str, Value, ErrorPolicy) -> Result<Box<dyn Sink>> + Send + Sync + use<> {
        let sinks = self.sinks.clone();
        move |id: &str, _config: Value, on_error: ErrorPolicy| {
            let sink = CollectingSink::new(id);
            sinks.lock().unwrap().insert(id.to_string(), sink.handle());
            Ok(Box::new(BasicSink::new(sink).with_error_policy(on_error)) as Box<dyn Sink>)
        }
    }

    fn get(&self, id: &str) -> SinkHandle {
        self.sinks
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_else(|| panic!("no sink registered for id `{id}`"))
    }
}

#[tokio::test]
async fn end_to_end_pipeline_built_through_registry() {
    let capture = SinkRegistry::default();

    let mut registry = Registry::default();
    registry.register_source("vec", vec_source_factory).unwrap();
    registry
        .register_transform("set_key", set_key_factory)
        .unwrap();
    registry
        .register_sink("capture", capture.factory())
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "p".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({
                        "items": [
                            { "user_id": "a", "v": 1 },
                            { "user_id": "b", "v": 2 },
                        ],
                    }),
                },
                transforms: vec![TransformSpec {
                    kind: "set_key".into(),
                    config: json!({ "from_field": "user_id" }),
                    on_error: None,
                }],
                sinks: vec![SinkSpec {
                    kind: "capture".into(),
                    config: json!({}),
                    on_error: Some(ErrorPolicyConfig::Drop),
                }],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    join_all(handles).await;

    let collected = capture.get("p/sink0");
    let items = collected.lock().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].meta.source_id, "p/src");
    assert_eq!(items[0].meta.key.as_deref(), Some("a"));
    assert_eq!(items[1].meta.key.as_deref(), Some("b"));
}

#[tokio::test]
async fn registry_fan_out_to_multiple_sinks() {
    // Two sinks on the same pipeline — each receives every envelope via
    // the runtime's broadcast splitter. Exercises the id scheme
    // `sink0`, `sink1` and the split-by-registry path.
    let capture = SinkRegistry::default();

    let mut registry = Registry::default();
    registry.register_source("vec", vec_source_factory).unwrap();
    registry
        .register_sink("capture", capture.factory())
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "fan".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({
                        "items": [{ "i": 0 }, { "i": 1 }, { "i": 2 }],
                    }),
                },
                transforms: vec![],
                sinks: vec![
                    SinkSpec {
                        kind: "capture".into(),
                        config: json!({}),
                        on_error: None,
                    },
                    SinkSpec {
                        kind: "capture".into(),
                        config: json!({}),
                        on_error: None,
                    },
                ],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    join_all(handles).await;

    assert_eq!(capture.get("fan/sink0").lock().unwrap().len(), 3);
    assert_eq!(capture.get("fan/sink1").lock().unwrap().len(), 3);
}
