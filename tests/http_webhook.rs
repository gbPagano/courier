//! Integration tests for the built-in `http_webhook` source: a real HTTP
//! client posts into a Courier pipeline built through the registry, and the
//! resulting envelope is captured by a sink.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::future::join_all;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use courier::config::{Config, FanOutPolicyConfig, PipelineSpec, SinkSpec, SourceSpec};
use courier::envelope::Envelope;
use courier::pipeline::ErrorPolicy;
use courier::retry::RetryPolicy;
use courier::sinks::{ManagedSink, Sink};
use courier::{Registry, register_builtin};

#[allow(dead_code)]
mod common;
use common::CollectingSink;

type SinkStore = Arc<Mutex<Vec<Envelope>>>;

fn capture_sink_factory(
    store: SinkStore,
) -> impl Fn(&str, Value, ErrorPolicy, Option<RetryPolicy>) -> Result<Box<dyn Sink>> + Send + Sync {
    move |id, _config, on_error, retry| {
        let sink = CollectingSink::from_store(id, Arc::clone(&store));
        let mut managed = ManagedSink::new(sink).with_error_policy(on_error);
        if let Some(policy) = retry {
            managed = managed.with_retry(policy);
        }
        Ok(Box::new(managed) as Box<dyn Sink>)
    }
}

#[tokio::test]
async fn pipeline_receives_http_post_as_envelope() {
    let bind = unused_local_addr();
    let store = Arc::new(Mutex::new(Vec::new()));

    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();
    registry
        .register_sink("capture", capture_sink_factory(Arc::clone(&store)))
        .unwrap();

    let courier = registry
        .build_courier(Config {
            observability: None,
            health: None,
            shutdown: None,
            pipelines: vec![PipelineSpec {
                name: "incoming-events".into(),
                source: SourceSpec {
                    kind: "http_webhook".into(),
                    config: json!({
                        "bind": bind.to_string(),
                        "path": "/webhooks/events",
                    }),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "capture".into(),
                    config: json!({}),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: Some(1),
                fan_out: FanOutPolicyConfig::default(),
            }],
        })
        .unwrap();

    let cancel = CancellationToken::new();
    let (handles, _state) = courier.spawn(cancel.clone());
    tokio::time::sleep(Duration::from_millis(50)).await;

    let response = reqwest::Client::new()
        .post(format!("http://{bind}/webhooks/events"))
        .header("x-delivery-id", "delivery-1")
        .json(&json!({ "kind": "user.created", "id": 42 }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let env = wait_for_one(&store).await;
    assert_eq!(env.meta.source_id, "incoming-events/src");
    assert_eq!(env.payload, json!({ "kind": "user.created", "id": 42 }));
    assert_eq!(
        env.meta.headers.get("http.header.x-delivery-id"),
        Some(&"delivery-1".to_string())
    );

    cancel.cancel();
    join_all(handles).await;
}

#[tokio::test]
async fn invalid_webhook_requests_return_client_errors() {
    let bind = unused_local_addr();
    let store = Arc::new(Mutex::new(Vec::new()));

    let mut registry = Registry::default();
    register_builtin(&mut registry).unwrap();
    registry
        .register_sink("capture", capture_sink_factory(Arc::clone(&store)))
        .unwrap();

    let courier = registry
        .build_courier(Config {
            observability: None,
            health: None,
            shutdown: None,
            pipelines: vec![PipelineSpec {
                name: "incoming-events".into(),
                source: SourceSpec {
                    kind: "http_webhook".into(),
                    config: json!({
                        "bind": bind.to_string(),
                        "path": "/webhooks/events",
                    }),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "capture".into(),
                    config: json!({}),
                    on_error: None,
                    retry: None,
                }],
                channel_capacity: None,
                fan_out: FanOutPolicyConfig::default(),
            }],
        })
        .unwrap();

    let cancel = CancellationToken::new();
    let (handles, _state) = courier.spawn(cancel.clone());
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let wrong_path = client
        .post(format!("http://{bind}/wrong"))
        .json(&json!({ "ok": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_path.status(), reqwest::StatusCode::NOT_FOUND);

    let wrong_method = client
        .put(format!("http://{bind}/webhooks/events"))
        .json(&json!({ "ok": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        wrong_method.status(),
        reqwest::StatusCode::METHOD_NOT_ALLOWED
    );

    let invalid_body = client
        .post(format!("http://{bind}/webhooks/events"))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_body.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(store.lock().unwrap().is_empty());

    cancel.cancel();
    join_all(handles).await;
}

#[test]
fn parse_config_reports_invalid_webhook_config() {
    let registry = Registry::with_builtins();
    let err = match registry.build_source(
        "incoming-events/src",
        SourceSpec {
            kind: "http_webhook".into(),
            config: json!({
                "bind": "127.0.0.1:8080",
                "path": "webhooks/events",
            }),
            retry: None,
        },
    ) {
        Ok(_) => panic!("expected invalid path error"),
        Err(err) => err,
    };

    assert!(
        err.to_string()
            .contains("failed to build source 'http_webhook'")
    );
    assert!(format!("{err:#}").contains("path must start with '/'"));
}

async fn wait_for_one(store: &SinkStore) -> Envelope {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(env) = store.lock().unwrap().first().cloned() {
            return env;
        }
        assert!(Instant::now() < deadline, "timed out waiting for envelope");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn unused_local_addr() -> SocketAddr {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}
