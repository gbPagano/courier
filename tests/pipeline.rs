//! End-to-end tests for the pipeline runtime: source → transforms → sinks,
//! fan-out, filtering, cancellation, and backpressure.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures::future::join_all;
use serde_json::json;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use courier::Courier;
use courier::envelope::Envelope;
use courier::pipeline::Pipeline;
use courier::register_builtin;
use courier::sinks::{ManagedSink, WriteOne};
use courier::sources::Source;
use courier::transforms::set_key::SetKeyTransform;
use courier::transforms::{BasicTransform, MapOne};
use courier::{
    Registry,
    config::{Config, ErrorPolicyConfig, PipelineSpec, SinkSpec, SourceSpec, TransformSpec},
};

mod common;
use common::{CollectingSink, VecSource};

fn env(id: &str, payload: serde_json::Value) -> Envelope {
    Envelope::new(id, payload)
}

#[tokio::test]
async fn source_transform_sink_end_to_end() {
    let source = VecSource::new(
        "src",
        vec![
            env("src", json!({ "user_id": "a", "v": 1 })),
            env("src", json!({ "user_id": "b", "v": 2 })),
        ],
    );
    let sink = CollectingSink::new("dst");
    let store = sink.handle();

    let pipeline = Pipeline::new("p", Box::new(source))
        .with_transform(Box::new(BasicTransform::new(SetKeyTransform::new(
            "set_key", "user_id",
        ))))
        .with_sink(Box::new(ManagedSink::new(sink)));

    let handles = Courier::new(vec![pipeline]).spawn(CancellationToken::new());
    join_all(handles).await;

    let collected = store.lock().unwrap();
    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0].meta.key.as_deref(), Some("a"));
    assert_eq!(collected[1].meta.key.as_deref(), Some("b"));
}

#[tokio::test]
async fn fan_out_to_multiple_sinks() {
    let source = VecSource::new(
        "src",
        (0..5).map(|i| env("src", json!({ "i": i }))).collect(),
    );
    let sink_a = CollectingSink::new("a");
    let sink_b = CollectingSink::new("b");
    let store_a = sink_a.handle();
    let store_b = sink_b.handle();

    let pipeline = Pipeline::new("fan", Box::new(source))
        .with_sink(Box::new(ManagedSink::new(sink_a)))
        .with_sink(Box::new(ManagedSink::new(sink_b)));

    let handles = Courier::new(vec![pipeline]).spawn(CancellationToken::new());
    join_all(handles).await;

    assert_eq!(store_a.lock().unwrap().len(), 5);
    assert_eq!(store_b.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn transform_filter_drops_envelopes() {
    struct EvenOnly;
    #[async_trait]
    impl MapOne for EvenOnly {
        fn id(&self) -> &str {
            "even_only"
        }
        async fn map(&self, env: Envelope) -> Result<Option<Envelope>> {
            let v = env.payload.get("v").and_then(|v| v.as_i64()).unwrap_or(0);
            Ok(if v % 2 == 0 { Some(env) } else { None })
        }
    }

    let source = VecSource::new(
        "src",
        (0..6).map(|i| env("src", json!({ "v": i }))).collect(),
    );
    let sink = CollectingSink::new("dst");
    let store = sink.handle();

    let pipeline = Pipeline::new("p", Box::new(source))
        .with_transform(Box::new(BasicTransform::new(EvenOnly)))
        .with_sink(Box::new(ManagedSink::new(sink)));

    let handles = Courier::new(vec![pipeline]).spawn(CancellationToken::new());
    join_all(handles).await;

    let collected = store.lock().unwrap();
    assert_eq!(collected.len(), 3); // 0, 2, 4
    for e in collected.iter() {
        let v = e.payload.get("v").unwrap().as_i64().unwrap();
        assert!(v % 2 == 0);
    }
}

#[tokio::test]
async fn cancellation_stops_pipeline() {
    // Cooperative source: checks `cancel` on every tick. With a fast sink
    // draining the channel, firing the token must drain every stage quickly.
    struct InfiniteSource;
    #[async_trait]
    impl Source for InfiniteSource {
        fn id(&self) -> &str {
            "infinite"
        }
        async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    res = tx.send(Envelope::new("infinite", json!({}))) => {
                        if res.is_err() { return }
                    }
                }
            }
        }
    }

    let sink = CollectingSink::new("sink");
    let store = sink.handle();
    let pipeline =
        Pipeline::new("p", Box::new(InfiniteSource)).with_sink(Box::new(ManagedSink::new(sink)));

    let cancel = CancellationToken::new();
    let handles = Courier::new(vec![pipeline]).spawn(cancel.clone());

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let res = tokio::time::timeout(Duration::from_secs(1), join_all(handles)).await;
    assert!(
        res.is_ok(),
        "pipeline did not shut down within 1s of cancel"
    );
    assert!(
        !store.lock().unwrap().is_empty(),
        "sink should have received at least one envelope before cancel"
    );
}

#[tokio::test]
async fn backpressure_blocks_source_on_full_channel() {
    struct CountingSource {
        produced: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Source for CountingSource {
        fn id(&self) -> &str {
            "counting"
        }
        async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    res = tx.send(Envelope::new("counting", json!({}))) => {
                        if res.is_err() { return }
                        self.produced.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        }
    }

    struct SlowSink;
    #[async_trait]
    impl WriteOne for SlowSink {
        fn id(&self) -> &str {
            "slow"
        }
        async fn write(&self, _env: &Envelope) -> Result<()> {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(())
        }
    }

    let produced = Arc::new(AtomicUsize::new(0));
    let pipeline = Pipeline::new(
        "bp",
        Box::new(CountingSource {
            produced: produced.clone(),
        }),
    )
    .with_sink(Box::new(ManagedSink::new(SlowSink)))
    .with_channel_capacity(4);

    let cancel = CancellationToken::new();
    let handles = Courier::new(vec![pipeline]).spawn(cancel.clone());

    // With capacity=4 and a sink that never completes a write in time,
    // the source saturates after ~5 sends (4 buffered + 1 pulled by sink).
    // Without backpressure it would produce thousands in 100ms.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let count = produced.load(Ordering::SeqCst);
    assert!(
        count <= 10,
        "source produced {count} items despite slow sink, expected backpressure"
    );

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), join_all(handles)).await;
}

#[tokio::test]
async fn script_transform_end_to_end() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![Envelope::new(id, json!({ "value": 1 }))],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "scripted".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
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

    let collected = capture.handle();
    let items = collected.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].payload, json!({ "value": 1, "processed": true }));
}

#[tokio::test]
async fn script_transform_filters_with_unit() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![Envelope::new(id, json!({ "skip": true }))],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "filtered".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "rhai",
                        "script": "fn transform(env) { () }",
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

    assert!(capture.handle().lock().unwrap().is_empty());
}

#[tokio::test]
async fn script_transform_drop_policy_continues_after_error() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![
                    Envelope::new(id, json!({ "fail": true, "value": 1 })),
                    Envelope::new(id, json!({ "fail": false, "value": 2 })),
                ],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "drop-errors".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "rhai",
                        "script": r#"
                            fn transform(env) {
                                if env.payload["fail"] == true {
                                    throw "boom";
                                }
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

    let items = capture.handle();
    let items = items.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].payload,
        json!({ "fail": false, "value": 2, "processed": true })
    );
}

#[tokio::test]
async fn script_transform_fail_pipeline_stops_after_error() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![
                    Envelope::new(id, json!({ "fail": true, "value": 1 })),
                    Envelope::new(id, json!({ "fail": false, "value": 2 })),
                ],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "fail-pipeline".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "rhai",
                        "script": r#"
                            fn transform(env) {
                                if env.payload["fail"] == true {
                                    throw "boom";
                                }
                                env.payload["processed"] = true;
                                env
                            }
                        "#,
                    }),
                    on_error: Some(ErrorPolicyConfig::FailPipeline),
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

    assert!(capture.handle().lock().unwrap().is_empty());
}

#[tokio::test]
async fn lua_script_transform_end_to_end() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![Envelope::new(id, json!({ "value": 1 }))],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "lua-scripted".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
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

    let collected = capture.handle();
    let items = collected.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].payload, json!({ "value": 1, "processed": true }));
}

#[tokio::test]
async fn lua_script_transform_filters_with_nil() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![Envelope::new(id, json!({ "skip": true }))],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "lua-filtered".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "lua",
                        "script": "function transform(env) return nil end",
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

    assert!(capture.handle().lock().unwrap().is_empty());
}

#[tokio::test]
async fn python_script_transform_end_to_end() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![Envelope::new(id, json!({ "value": 1 }))],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "python-scripted".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
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

    let collected = capture.handle();
    let items = collected.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].payload, json!({ "value": 1, "processed": true }));
}

#[tokio::test]
async fn python_script_transform_filters_with_none() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![Envelope::new(id, json!({ "skip": true }))],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "python-filtered".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "python",
                        "script": "def transform(env):\n    return None\n",
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

    assert!(capture.handle().lock().unwrap().is_empty());
}

#[tokio::test]
async fn script_transform_chains_rhai_then_python() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![Envelope::new(id, json!({ "value": 1 }))],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "rhai-then-python".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
                },
                transforms: vec![
                    TransformSpec {
                        kind: "script".into(),
                        config: json!({
                            "runtime": "rhai",
                            "script": r#"
                                fn transform(env) {
                                    env.payload["rhai"] = true;
                                    env
                                }
                            "#,
                        }),
                        on_error: Some(ErrorPolicyConfig::Drop),
                    },
                    TransformSpec {
                        kind: "script".into(),
                        config: json!({
                            "runtime": "python",
                            "script": "def transform(env):\n    env['payload']['python'] = True\n    env['payload']['value'] = env['payload']['value'] + 1\n    return env\n",
                        }),
                        on_error: Some(ErrorPolicyConfig::Drop),
                    },
                ],
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

    let collected = capture.handle();
    let items = collected.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].payload,
        json!({ "value": 2, "rhai": true, "python": true })
    );
}

#[tokio::test]
async fn script_transform_chains_rhai_then_lua() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![Envelope::new(id, json!({ "value": 1 }))],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "rhai-then-lua".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
                },
                transforms: vec![
                    TransformSpec {
                        kind: "script".into(),
                        config: json!({
                            "runtime": "rhai",
                            "script": r#"
                                fn transform(env) {
                                    env.payload["rhai"] = true;
                                    env
                                }
                            "#,
                        }),
                        on_error: Some(ErrorPolicyConfig::Drop),
                    },
                    TransformSpec {
                        kind: "script".into(),
                        config: json!({
                            "runtime": "lua",
                            "script": r#"
                                function transform(env)
                                    env.payload.lua = true
                                    env.payload.value = env.payload.value + 1
                                    return env
                                end
                            "#,
                        }),
                        on_error: Some(ErrorPolicyConfig::Drop),
                    },
                ],
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

    let collected = capture.handle();
    let items = collected.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].payload,
        json!({ "value": 2, "rhai": true, "lua": true })
    );
}

#[tokio::test]
async fn python_script_transform_drop_policy_continues_after_error() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![
                    Envelope::new(id, json!({ "fail": true, "value": 1 })),
                    Envelope::new(id, json!({ "fail": false, "value": 2 })),
                ],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "python-drop-errors".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "python",
                        "script": "def transform(env):\n    if env['payload']['fail']:\n        raise RuntimeError('boom')\n    env['payload']['processed'] = True\n    return env\n",
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

    let items = capture.handle();
    let items = items.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].payload,
        json!({ "fail": false, "value": 2, "processed": true })
    );
}

#[tokio::test]
async fn python_script_transform_fail_pipeline_stops_after_error() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![
                    Envelope::new(id, json!({ "fail": true, "value": 1 })),
                    Envelope::new(id, json!({ "fail": false, "value": 2 })),
                ],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "python-fail-pipeline".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "python",
                        "script": "def transform(env):\n    if env['payload']['fail']:\n        raise RuntimeError('boom')\n    env['payload']['processed'] = True\n    return env\n",
                    }),
                    on_error: Some(ErrorPolicyConfig::FailPipeline),
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

    assert!(capture.handle().lock().unwrap().is_empty());
}

#[tokio::test]
async fn lua_script_transform_drop_policy_continues_after_error() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![
                    Envelope::new(id, json!({ "fail": true, "value": 1 })),
                    Envelope::new(id, json!({ "fail": false, "value": 2 })),
                ],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "lua-drop-errors".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "lua",
                        "script": r#"
                            function transform(env)
                                if env.payload.fail == true then
                                    error("boom")
                                end
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

    let items = capture.handle();
    let items = items.lock().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].payload,
        json!({ "fail": false, "value": 2, "processed": true })
    );
}

#[tokio::test]
async fn lua_script_transform_fail_pipeline_stops_after_error() {
    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();

    let capture = CollectingSink::new("dst");
    let store = capture.handle();
    registry
        .register_source("vec", |id: &str, _| {
            Ok(Box::new(VecSource::new(
                id,
                vec![
                    Envelope::new(id, json!({ "fail": true, "value": 1 })),
                    Envelope::new(id, json!({ "fail": false, "value": 2 })),
                ],
            )) as Box<dyn Source>)
        })
        .unwrap();
    registry
        .register_sink("capture", move |id: &str, _, _, _| {
            Ok(Box::new(ManagedSink::new(CollectingSink::from_store(
                id,
                store.clone(),
            ))) as Box<dyn courier::sinks::Sink>)
        })
        .unwrap();

    let courier = registry
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "lua-fail-pipeline".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({}),
                },
                transforms: vec![TransformSpec {
                    kind: "script".into(),
                    config: json!({
                        "runtime": "lua",
                        "script": r#"
                            function transform(env)
                                if env.payload.fail == true then
                                    error("boom")
                                end
                                env.payload.processed = true
                                return env
                            end
                        "#,
                    }),
                    on_error: Some(ErrorPolicyConfig::FailPipeline),
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

    assert!(capture.handle().lock().unwrap().is_empty());
}
