//! Integration tests for the built-in `api` sink: a real HTTP server
//! receives traffic from a Courier pipeline driven through the registry,
//! exercising config parsing, body shape, retry, and dead-letter together.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures::future::join_all;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_json, body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use courier::Registry;
use courier::config::{Config, PipelineSpec, SinkSpec, SourceSpec};
use courier::envelope::Envelope;
use courier::register_builtin;
use courier::retry::{ExhaustedPolicy, RetryPolicy};
use courier::sources::Source;

#[allow(dead_code)]
mod common;
use common::VecSource;

/// Local source factory: emits each entry in `items` as one envelope and
/// then closes the channel. Mirrors the helper used by other integration
/// tests.
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

fn registry() -> Registry {
    let mut r = Registry::default();
    register_builtin(&mut r).unwrap();
    r.register_source("vec", vec_source_factory).unwrap();
    r
}

#[tokio::test]
async fn pipeline_posts_payload_via_api_sink() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .and(body_json(json!({ "user_id": "alice", "v": 1 })))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;

    let courier = registry()
        .build_courier(Config {
            observability: None,
            pipelines: vec![PipelineSpec {
                name: "to-webhook".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({
                        "items": [{ "user_id": "alice", "v": 1 }],
                    }),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "api".into(),
                    config: json!({
                        "url": format!("{}/webhook", server.uri()),
                    }),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    join_all(handles).await;

    // Wiremock's `.expect(1)` is verified on Drop — server falls out of
    // scope here.
}

#[tokio::test]
async fn api_sink_sends_full_envelope_when_configured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ingest"))
        .and(body_partial_json(json!({
            "payload": { "id": 7 },
            "meta": { "source_id": "with-meta/src" }
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let courier = registry()
        .build_courier(Config {
            observability: None,
            pipelines: vec![PipelineSpec {
                name: "with-meta".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({ "items": [{ "id": 7 }] }),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "api".into(),
                    config: json!({
                        "url": format!("{}/ingest", server.uri()),
                        "body": "envelope",
                    }),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    join_all(handles).await;
}

#[tokio::test]
async fn api_sink_forwards_custom_headers_and_method() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/items/1"))
        .and(header("authorization", "Bearer s3cret"))
        .and(header("x-tenant", "acme"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let courier = registry()
        .build_courier(Config {
            observability: None,
            pipelines: vec![PipelineSpec {
                name: "put".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({ "items": [{ "name": "widget" }] }),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "api".into(),
                    config: json!({
                        "url": format!("{}/items/1", server.uri()),
                        "method": "PUT",
                        "headers": {
                            "authorization": "Bearer s3cret",
                            "x-tenant": "acme",
                        }
                    }),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    join_all(handles).await;
}

#[tokio::test]
async fn api_sink_retries_then_succeeds_on_5xx() {
    let server = MockServer::start().await;
    // First two responses are 503, then 200 — verifies retry actually
    // re-issues the request rather than silently dropping the envelope.
    Mock::given(method("POST"))
        .and(path("/flaky"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/flaky"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let courier = registry()
        .build_courier(Config {
            observability: None,
            pipelines: vec![PipelineSpec {
                name: "with-retry".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({ "items": [{ "n": 1 }] }),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "api".into(),
                    config: json!({ "url": format!("{}/flaky", server.uri()) }),
                    on_error: None,
                    retry: Some(RetryPolicy {
                        max_attempts: 4,
                        initial_delay_ms: 1,
                        backoff_multiplier: 1.0,
                        max_delay_ms: 5,
                        on_exhausted: ExhaustedPolicy::Propagate,
                    }),
                }],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    join_all(handles).await;
}

#[tokio::test]
async fn api_sink_dead_letters_after_retry_exhaustion() {
    let server = MockServer::start().await;
    // Always 500 — every attempt fails, so the envelope must end up in
    // the dead-letter file.
    Mock::given(method("POST"))
        .and(path("/dead"))
        .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dlq = dir.path().join("dlq.jsonl");

    let courier = registry()
        .build_courier(Config {
            observability: None,
            pipelines: vec![PipelineSpec {
                name: "dlq".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({ "items": [{ "id": "abc" }] }),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "api".into(),
                    config: json!({ "url": format!("{}/dead", server.uri()) }),
                    on_error: None,
                    retry: Some(RetryPolicy {
                        max_attempts: 2,
                        initial_delay_ms: 1,
                        backoff_multiplier: 1.0,
                        max_delay_ms: 5,
                        on_exhausted: ExhaustedPolicy::DeadLetter { path: dlq.clone() },
                    }),
                }],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    join_all(handles).await;

    let contents = std::fs::read_to_string(&dlq).expect("dead-letter file should exist");
    let entry: Value = serde_json::from_str(contents.trim()).unwrap();
    assert_eq!(entry["payload"], json!({ "id": "abc" }));
    assert!(entry["error"].as_str().unwrap().contains("500"), "{entry}");
}

#[tokio::test]
async fn api_sink_drop_policy_continues_after_failure() {
    // Source emits two envelopes. The first POST fails; the second must
    // still be attempted because `on_error = "drop"` is the default and
    // no retry is configured.
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));

    let counter = attempts.clone();
    struct CountingResponder {
        counter: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl wiremock::Respond for CountingResponder {
        fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
            let n = self.counter.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200)
            }
        }
    }

    Mock::given(method("POST"))
        .and(path("/maybe"))
        .respond_with(CountingResponder { counter })
        .mount(&server)
        .await;

    // Source factory that holds the channel open briefly so the sink has
    // time to drain both items before the pipeline shuts down. `VecSource`
    // already does this — it sends sequentially and only closes when done.
    let courier = registry()
        .build_courier(Config {
            observability: None,
            pipelines: vec![PipelineSpec {
                name: "drop".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({ "items": [{ "n": 1 }, { "n": 2 }] }),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "api".into(),
                    config: json!({ "url": format!("{}/maybe", server.uri()) }),
                    on_error: None, // defaults to drop
                    retry: None,
                }],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    let _ = tokio::time::timeout(Duration::from_secs(5), join_all(handles))
        .await
        .expect("pipeline should finish");

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "second envelope should have been attempted despite first failing"
    );
}

#[tokio::test]
async fn api_sink_writes_through_a_transform() {
    // End-to-end shape from the proposal: source -> transform -> api sink,
    // verifying the transform output is what reaches the HTTP server.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tx"))
        .and(body_json(json!({ "value": 1, "processed": true })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let courier = registry()
        .build_courier(Config {
            observability: None,
            pipelines: vec![PipelineSpec {
                name: "tx-then-api".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({ "items": [{ "value": 1 }] }),
                    retry: None,
                },
                transforms: vec![courier::config::TransformSpec {
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
                    on_error: None,
                }],
                sinks: vec![SinkSpec {
                    kind: "api".into(),
                    config: json!({ "url": format!("{}/tx", server.uri()) }),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
            }],
        })
        .unwrap();

    let handles = courier.spawn(CancellationToken::new());
    let _ = tokio::time::timeout(Duration::from_secs(5), join_all(handles))
        .await
        .expect("pipeline should finish");
}
