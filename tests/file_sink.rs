//! Integration tests for the built-in `file` sink: drives a Courier
//! pipeline through the registry and verifies the output landed on disk
//! in the requested format.

use anyhow::Result;
use futures::future::join_all;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use courier::Registry;
use courier::config::{Config, PipelineSpec, SinkSpec, SourceSpec};
use courier::envelope::Envelope;
use courier::register_builtin;
use courier::sources::Source;

#[allow(dead_code)]
mod common;
use common::VecSource;

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
async fn pipeline_writes_jsonl_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.jsonl");

    let courier = registry()
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "to-file".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({
                        "items": [
                            { "id": 1, "name": "alice" },
                            { "id": 2, "name": "bob" },
                        ],
                    }),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "file".into(),
                    config: json!({
                        "path": path.to_str().unwrap(),
                        "format": "jsonl",
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

    let contents = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 2);
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    let second: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(first, json!({ "id": 1, "name": "alice" }));
    assert_eq!(second, json!({ "id": 2, "name": "bob" }));
}

#[tokio::test]
async fn pipeline_writes_csv_with_header_then_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.csv");

    let courier = registry()
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "csv".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({
                        "items": [
                            { "id": 1, "name": "alice" },
                            { "id": 2, "name": "bob, with comma" },
                        ],
                    }),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "file".into(),
                    config: json!({
                        "path": path.to_str().unwrap(),
                        "format": "csv",
                        "columns": ["payload.id", "payload.name"],
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

    let contents = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(
        lines,
        vec![
            "payload.id,payload.name",
            "1,alice",
            "2,\"bob, with comma\"",
        ]
    );
}

#[tokio::test]
async fn jsonl_envelope_mode_persists_meta_through_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("envelopes.jsonl");

    let courier = registry()
        .build_courier(Config {
            pipelines: vec![PipelineSpec {
                name: "with-meta".into(),
                source: SourceSpec {
                    kind: "vec".into(),
                    config: json!({ "items": [{ "v": 1 }] }),
                    retry: None,
                },
                transforms: vec![],
                sinks: vec![SinkSpec {
                    kind: "file".into(),
                    config: json!({
                        "path": path.to_str().unwrap(),
                        "format": "jsonl",
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

    let line = std::fs::read_to_string(&path).unwrap();
    let parsed: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(parsed["payload"], json!({ "v": 1 }));
    assert_eq!(parsed["meta"]["source_id"], "with-meta/src");
}
