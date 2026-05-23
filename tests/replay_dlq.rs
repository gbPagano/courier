//! End-to-end integration tests for `courier replay-dlq`: writes DLQ entries
//! to disk, runs `build_replay_runtime`, and verifies the envelopes reach
//! the downstream sink.

use std::time::Duration;

use futures::future::join_all;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use courier::DeadLetterEntry;
use courier::Registry;
use courier::cli::build_replay_runtime;
use courier::config::{Config, FanOutPolicyConfig, PipelineSpec, SinkSpec, SourceSpec};
use courier::envelope::Envelope;

fn pipeline_config(name: &str, sink_path: &str, source_placeholder: SourceSpec) -> PipelineSpec {
    PipelineSpec {
        name: name.into(),
        source: source_placeholder,
        transforms: vec![],
        sinks: vec![SinkSpec {
            kind: "file".into(),
            config: json!({ "path": sink_path, "format": "jsonl" }),
            on_error: None,
            retry: None,
        }],
        channel_capacity: None,
        fan_out: FanOutPolicyConfig::default(),
    }
}

fn placeholder_source() -> SourceSpec {
    SourceSpec {
        kind: "api_poll".into(),
        config: json!({ "url": "http://localhost/unused", "interval_secs": 60 }),
        retry: None,
    }
}

fn dlq_entry(pipeline: &str, sink: &str, payload: Value) -> String {
    let entry = DeadLetterEntry {
        envelope: Envelope::new("src", payload),
        error: "simulated failure".into(),
        pipeline: pipeline.into(),
        sink: sink.into(),
        dead_lettered_at_ms: 1_715_234_567_890,
        attempts: 3,
    };
    serde_json::to_string(&entry).unwrap()
}

#[tokio::test]
async fn replay_delivers_dlq_envelopes_to_sink() {
    let dir = tempfile::tempdir().unwrap();
    let dlq_path = dir.path().join("dlq.jsonl");
    let out_path = dir.path().join("out.jsonl");

    let content = format!(
        "{}\n{}\n",
        dlq_entry("p", "p/sink0", json!({"id": 1})),
        dlq_entry("p", "p/sink0", json!({"id": 2})),
    );
    std::fs::write(&dlq_path, content).unwrap();

    let config = Config {
        observability: None,
        health: None,
        shutdown: None,
        pipelines: vec![pipeline_config(
            "p",
            out_path.to_str().unwrap(),
            placeholder_source(),
        )],
    };

    let courier = build_replay_runtime(config, &dlq_path, Registry::with_builtins()).unwrap();

    let (handles, _state) = courier.spawn(CancellationToken::new());
    tokio::time::timeout(Duration::from_secs(5), join_all(handles))
        .await
        .expect("replay did not complete within 5 s");

    let contents = std::fs::read_to_string(&out_path).unwrap();
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 output lines, got: {contents}");

    let first: Value = serde_json::from_str(lines[0]).unwrap();
    let second: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(first["id"], 1);
    assert_eq!(second["id"], 2);
}

#[tokio::test]
async fn replay_filters_entries_by_pipeline_in_multi_pipeline_config() {
    let dir = tempfile::tempdir().unwrap();
    let dlq_path = dir.path().join("dlq.jsonl");
    let out_a = dir.path().join("a.jsonl");
    let out_b = dir.path().join("b.jsonl");

    let content = format!(
        "{}\n{}\n{}\n",
        dlq_entry("a", "a/sink0", json!({"pipeline": "a"})),
        dlq_entry("b", "b/sink0", json!({"pipeline": "b"})),
        dlq_entry("a", "a/sink0", json!({"pipeline": "a", "seq": 2})),
    );
    std::fs::write(&dlq_path, content).unwrap();

    let config = Config {
        observability: None,
        health: None,
        shutdown: None,
        pipelines: vec![
            pipeline_config("a", out_a.to_str().unwrap(), placeholder_source()),
            pipeline_config("b", out_b.to_str().unwrap(), placeholder_source()),
        ],
    };

    let courier = build_replay_runtime(config, &dlq_path, Registry::with_builtins()).unwrap();

    let (handles, _state) = courier.spawn(CancellationToken::new());
    tokio::time::timeout(Duration::from_secs(5), join_all(handles))
        .await
        .expect("replay did not complete within 5 s");

    let lines_a: Vec<Value> = std::fs::read_to_string(&out_a)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let lines_b: Vec<Value> = std::fs::read_to_string(&out_b)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(lines_a.len(), 2, "pipeline 'a' should receive 2 envelopes");
    assert_eq!(lines_b.len(), 1, "pipeline 'b' should receive 1 envelope");
    assert_eq!(lines_a[0]["pipeline"], "a");
    assert_eq!(lines_a[1]["pipeline"], "a");
    assert_eq!(lines_b[0]["pipeline"], "b");
}

#[tokio::test]
async fn replay_splits_multi_sink_pipeline_per_sink() {
    let dir = tempfile::tempdir().unwrap();
    let dlq_path = dir.path().join("dlq.jsonl");
    let out_a = dir.path().join("a.jsonl");
    let out_b = dir.path().join("b.jsonl");

    let content = format!(
        "{}\n{}\n{}\n",
        dlq_entry("p", "p/sink0", json!({"target": "a"})),
        dlq_entry("p", "p/sink1", json!({"target": "b"})),
        dlq_entry("p", "p/sink0", json!({"target": "a", "seq": 2})),
    );
    std::fs::write(&dlq_path, content).unwrap();

    let config = Config {
        observability: None,
        health: None,
        shutdown: None,
        pipelines: vec![PipelineSpec {
            name: "p".into(),
            source: placeholder_source(),
            transforms: vec![],
            sinks: vec![
                SinkSpec {
                    kind: "file".into(),
                    config: json!({ "path": out_a.to_str().unwrap(), "format": "jsonl" }),
                    on_error: None,
                    retry: None,
                },
                SinkSpec {
                    kind: "file".into(),
                    config: json!({ "path": out_b.to_str().unwrap(), "format": "jsonl" }),
                    on_error: None,
                    retry: None,
                },
            ],
            channel_capacity: None,
            fan_out: FanOutPolicyConfig::BroadcastWithDrop,
        }],
    };

    let courier = build_replay_runtime(config, &dlq_path, Registry::with_builtins()).unwrap();

    let (handles, _state) = courier.spawn(CancellationToken::new());
    tokio::time::timeout(Duration::from_secs(5), join_all(handles))
        .await
        .expect("replay did not complete within 5 s");

    let lines_a: Vec<Value> = std::fs::read_to_string(&out_a)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let lines_b: Vec<Value> = std::fs::read_to_string(&out_b)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(
        lines_a.len(),
        2,
        "sink a should receive 2 envelopes (only those that failed at sink0)"
    );
    assert_eq!(
        lines_b.len(),
        1,
        "sink b should receive 1 envelope (only the one that failed at sink1)"
    );
    assert_eq!(lines_a[0]["target"], "a");
    assert_eq!(lines_a[1]["target"], "a");
    assert_eq!(lines_b[0]["target"], "b");
}
