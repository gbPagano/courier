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
use courier::register_builtin;
use courier::retry::RetryPolicy;
use courier::sinks::{ManagedSink, Sink};
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

fn vec_source_factory(
    id: &str,
    config: Value,
    _retry: Option<courier::retry::RetryPolicy>,
) -> Result<Box<dyn Source>> {
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
    ) -> impl Fn(&str, Value, ErrorPolicy, Option<RetryPolicy>) -> Result<Box<dyn Sink>>
    + Send
    + Sync
    + use<> {
        let sinks = self.sinks.clone();
        move |id: &str, _config: Value, on_error: ErrorPolicy, retry: Option<RetryPolicy>| {
            let sink = CollectingSink::new(id);
            sinks.lock().unwrap().insert(id.to_string(), sink.handle());
            let mut managed = ManagedSink::new(sink).with_error_policy(on_error);
            if let Some(policy) = retry {
                managed = managed.with_retry(policy);
            }
            Ok(Box::new(managed) as Box<dyn Sink>)
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
                    retry: None,
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
                    retry: None,
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
async fn built_in_script_transform_runs_through_registry() {
    let capture = SinkRegistry::default();

    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();
    registry.register_source("vec", vec_source_factory).unwrap();
    registry
        .register_sink("capture", capture.factory())
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "scripted".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({
                        "items": [{ "value": 1 }],
                    }),
                    retry: None,
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "rhai",
                        "script": r#"
                            fn transform(env) {
                                env.payload["processed"] = true;
                                env
                            }
                        "#,
                    }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                }],
                sinks: vec![SinkSpec {
                    kind: "capture".into(),
                    config: json!({}),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    join_all(handles).await;

    let collected = capture.get("scripted/sink0");
    let items = collected.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].payload, json!({ "value": 1, "processed": true }));
}

#[tokio::test]
async fn built_in_lua_script_transform_runs_through_registry() {
    let capture = SinkRegistry::default();

    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();
    registry.register_source("vec", vec_source_factory).unwrap();
    registry
        .register_sink("capture", capture.factory())
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "lua-scripted".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({
                        "items": [{ "value": 1 }],
                    }),
                    retry: None,
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "lua",
                        "script": r#"
                            function transform(env)
                                env.payload.processed = true
                                return env
                            end
                        "#,
                    }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                }],
                sinks: vec![SinkSpec {
                    kind: "capture".into(),
                    config: json!({}),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    join_all(handles).await;

    let collected = capture.get("lua-scripted/sink0");
    let items = collected.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].payload, json!({ "value": 1, "processed": true }));
}

#[tokio::test]
async fn built_in_python_script_transform_runs_through_registry() {
    let capture = SinkRegistry::default();

    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();
    registry.register_source("vec", vec_source_factory).unwrap();
    registry
        .register_sink("capture", capture.factory())
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "python-scripted".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({
                        "items": [{ "value": 1 }],
                    }),
                                    retry: None,
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "python",
                        "script": "def transform(env):\n    env['payload']['processed'] = True\n    return env\n",
                    }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                }],
                sinks: vec![SinkSpec {
                    kind: "capture".into(),
                    config: json!({}),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    join_all(handles).await;

    let collected = capture.get("python-scripted/sink0");
    let items = collected.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].payload, json!({ "value": 1, "processed": true }));
}

#[tokio::test]
async fn built_in_python_script_transform_supports_custom_entrypoint() {
    let capture = SinkRegistry::default();

    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();
    registry.register_source("vec", vec_source_factory).unwrap();
    registry
        .register_sink("capture", capture.factory())
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "python-custom-entrypoint".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({
                        "items": [{ "value": 1 }],
                    }),
                                    retry: None,
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "python",
                        "entrypoint": "process",
                        "script": "def process(env):\n    env['payload']['processed'] = True\n    return env\n",
                    }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                }],
                sinks: vec![SinkSpec {
                    kind: "capture".into(),
                    config: json!({}),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    join_all(handles).await;

    let collected = capture.get("python-custom-entrypoint/sink0");
    let items = collected.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].payload, json!({ "value": 1, "processed": true }));
}

#[tokio::test]
async fn built_in_script_transform_requires_runtime() {
    let capture = SinkRegistry::default();

    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();
    registry.register_source("vec", vec_source_factory).unwrap();
    registry
        .register_sink("capture", capture.factory())
        .unwrap();

    let err = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "missing-runtime".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({
                        "items": [{ "value": 1 }],
                    }),
                    retry: None,
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "script": "fn transform(env) { env }",
                    }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                }],
                sinks: vec![SinkSpec {
                    kind: "capture".into(),
                    config: json!({}),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
            }],
        })
        .err()
        .expect("expected missing runtime error");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("invalid config for component type 'script'"),
        "{msg}"
    );
    assert!(msg.contains("runtime"), "{msg}");
}

#[tokio::test]
async fn built_in_lua_script_transform_rejects_rhai_limits() {
    let capture = SinkRegistry::default();

    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();
    registry.register_source("vec", vec_source_factory).unwrap();
    registry
        .register_sink("capture", capture.factory())
        .unwrap();

    let err = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "lua-rhai-limits".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({
                        "items": [{ "value": 1 }],
                    }),
                    retry: None,
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "lua",
                        "script": "function transform(env) return env end",
                        "max_variables": 1,
                    }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                }],
                sinks: vec![SinkSpec {
                    kind: "capture".into(),
                    config: json!({}),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
            }],
        })
        .err()
        .expect("expected Lua Rhai-limit validation error");

    let msg = format!("{err:#}");
    assert!(msg.contains("Rhai-only limits"), "{msg}");
}

#[tokio::test]
async fn built_in_python_script_transform_rejects_rhai_limits() {
    let capture = SinkRegistry::default();

    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();
    registry.register_source("vec", vec_source_factory).unwrap();
    registry
        .register_sink("capture", capture.factory())
        .unwrap();

    let err = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "python-rhai-limits".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({
                        "items": [{ "value": 1 }],
                    }),
                    retry: None,
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "python",
                        "script": "def transform(env):\n    return env\n",
                        "max_variables": 1,
                    }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                }],
                sinks: vec![SinkSpec {
                    kind: "capture".into(),
                    config: json!({}),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
            }],
        })
        .err()
        .expect("expected Python Rhai-limit validation error");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("Rhai-only limits") && msg.contains("runtime 'python'"),
        "{msg}"
    );
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
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![
                    SinkSpec {
                        kind: "capture".into(),
                        config: json!({}),
                        on_error: None,
                        retry: None,
                    },
                    SinkSpec {
                        kind: "capture".into(),
                        config: json!({}),
                        on_error: None,
                        retry: None,
                    },
                ],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    join_all(handles).await;

    let sink0 = capture.get("fan/sink0");
    let sink1 = capture.get("fan/sink1");
    assert_eq!(sink0.lock().unwrap().len(), 3);
    assert_eq!(sink1.lock().unwrap().len(), 3);
}
