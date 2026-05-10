use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::config::parse_config;
use crate::envelope::{DeadLetterEntry, Envelope};
use crate::observability::{NodeCtx, SendStopped, SourceCtx};
use crate::sources::Source;

pub struct JsonlFileSource {
    id: String,
    path: PathBuf,
    source_ctx: SourceCtx,
    emit_interval: Option<Duration>,
    dead_letter_pipeline: Option<String>,
    dead_letter_sink: Option<String>,
}

impl JsonlFileSource {
    pub fn new(id: impl Into<String>, path: PathBuf) -> Self {
        let id = id.into();
        let source_ctx = SourceCtx::new(&id);
        Self {
            id,
            path,
            source_ctx,
            emit_interval: None,
            dead_letter_pipeline: None,
            dead_letter_sink: None,
        }
    }

    pub fn with_emit_interval(mut self, interval: Duration) -> Self {
        self.emit_interval = Some(interval);
        self
    }

    pub fn with_dead_letter_pipeline(mut self, pipeline: impl Into<String>) -> Self {
        self.dead_letter_pipeline = Some(pipeline.into());
        self
    }

    pub fn with_dead_letter_sink(mut self, sink: impl Into<String>) -> Self {
        self.dead_letter_sink = Some(sink.into());
        self
    }
}

#[async_trait]
impl Source for JsonlFileSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        self.source_ctx = SourceCtx::from_node_ctx(ctx);
    }

    async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
        let file = match File::open(&self.path).await {
            Ok(file) => file,
            Err(e) => {
                tracing::error!(
                    node_id = %self.id,
                    path = %self.path.display(),
                    error = %e,
                    "failed to read jsonl_file source"
                );
                return;
            }
        };

        let source_ctx = self.source_ctx.clone();
        let mut line_number: u64 = 0;
        let emit_interval = self.emit_interval;
        let dead_letter_pipeline = self.dead_letter_pipeline.as_deref();
        let dead_letter_sink = self.dead_letter_sink.as_deref();
        let mut lines = BufReader::new(file).lines();

        loop {
            let line = tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!(node_id = %self.id, reason = "cancel", "jsonl_file source stopping");
                    return;
                }
                result = lines.next_line() => match result {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!(
                            node_id = %self.id,
                            path = %self.path.display(),
                            error = %e,
                            "failed to read line from jsonl_file source"
                        );
                        return;
                    }
                },
            };

            line_number += 1;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let envelope = if let Some(pipeline) = dead_letter_pipeline {
                match parse_line_for_pipeline(trimmed, pipeline, dead_letter_sink) {
                    Ok(Some(env)) => env,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!(
                            node_id = %self.id,
                            line_number,
                            error = %e,
                            "skipping unparseable line in jsonl_file source"
                        );
                        continue;
                    }
                }
            } else {
                match parse_line(trimmed) {
                    Ok(env) => env,
                    Err(e) => {
                        tracing::warn!(
                            node_id = %self.id,
                            line_number,
                            error = %e,
                            "skipping unparseable line in jsonl_file source"
                        );
                        continue;
                    }
                }
            };

            match source_ctx.send(&tx, envelope, &cancel).await {
                Ok(()) => {}
                Err(SendStopped::Cancelled) => return,
                Err(SendStopped::DownstreamClosed) => {
                    tracing::info!(
                        node_id = %self.id,
                        reason = "downstream_closed",
                        "jsonl_file source stopping"
                    );
                    return;
                }
            }

            if let Some(interval) = emit_interval {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(interval) => {}
                }
            }
        }

        tracing::debug!(node_id = %self.id, "jsonl_file source emitted all envelopes");
    }
}

fn parse_line(line: &str) -> Result<Envelope> {
    Ok(parse_line_entry(line)?.envelope)
}

struct ParsedLine {
    envelope: Envelope,
    dead_letter_pipeline: Option<String>,
    dead_letter_sink: Option<String>,
}

fn parse_line_for_pipeline(
    line: &str,
    expected_pipeline: &str,
    expected_sink: Option<&str>,
) -> Result<Option<Envelope>> {
    let parsed = parse_line_entry(line)?;

    match parsed.dead_letter_pipeline.as_deref() {
        Some(actual_pipeline) if actual_pipeline == expected_pipeline => match expected_sink {
            Some(expected) => match parsed.dead_letter_sink.as_deref() {
                Some(actual_sink) if actual_sink == expected => Ok(Some(parsed.envelope)),
                Some(_) => Ok(None),
                None => Ok(None),
            },
            None => Ok(Some(parsed.envelope)),
        },
        Some(_) => Ok(None),
        None => match expected_sink {
            Some(expected) => {
                tracing::warn!(
                    sink_filter = %expected,
                    "dropping unannotated DLQ entry during multi-sink replay: \
                     no pipeline annotation, cannot determine target sink"
                );
                Ok(None)
            }
            None => Ok(Some(parsed.envelope)),
        },
    }
}

fn parse_line_entry(line: &str) -> Result<ParsedLine> {
    let value: Value =
        serde_json::from_str(line).with_context(|| "invalid JSON in jsonl_file source line")?;

    if value.get("envelope").is_some() {
        let entry: DeadLetterEntry = serde_json::from_value(value)
            .with_context(|| "failed to deserialize as DeadLetterEntry")?;
        Ok(ParsedLine {
            envelope: entry.envelope,
            dead_letter_pipeline: Some(entry.pipeline),
            dead_letter_sink: Some(entry.sink),
        })
    } else if value.get("meta").is_some() {
        Ok(ParsedLine {
            envelope: serde_json::from_value::<Envelope>(value)
                .with_context(|| "failed to deserialize as Envelope")?,
            dead_letter_pipeline: None,
            dead_letter_sink: None,
        })
    } else {
        bail!("line does not match any known envelope format (missing 'envelope' or 'meta' key)");
    }
}

#[derive(Debug, Deserialize)]
struct JsonlFileSourceConfig {
    path: PathBuf,
    #[serde(default)]
    emit_interval_ms: Option<u64>,
    #[serde(default)]
    dead_letter_pipeline: Option<String>,
    /// Internal use by `courier replay-dlq`. Filters entries to those that
    /// originally failed at this specific sink node id (e.g. `"my-pipeline/sink1"`).
    /// Used when a pipeline is split into per-sink replay pipelines to prevent
    /// cross-contamination between sibling sinks.
    #[serde(default)]
    dead_letter_sink: Option<String>,
}

pub fn jsonl_file_source_factory(
    id: &str,
    config: Value,
    retry: Option<crate::retry::RetryPolicy>,
) -> Result<Box<dyn Source>> {
    if retry.is_some() {
        bail!("invalid config for component type 'jsonl_file': retry has no effect on this source");
    }
    let cfg: JsonlFileSourceConfig = parse_config("jsonl_file", config)?;
    if !cfg.path.exists() {
        bail!(
            "invalid config for component type 'jsonl_file': path '{}' does not exist",
            cfg.path.display()
        );
    }
    if !cfg.path.is_file() {
        bail!(
            "invalid config for component type 'jsonl_file': path '{}' is not a file",
            cfg.path.display()
        );
    }
    let mut source = JsonlFileSource::new(id, cfg.path);
    if let Some(pipeline) = cfg.dead_letter_pipeline {
        source = source.with_dead_letter_pipeline(pipeline);
    }
    if let Some(sink) = cfg.dead_letter_sink {
        source = source.with_dead_letter_sink(sink);
    }
    if let Some(ms) = cfg.emit_interval_ms {
        if ms == 0 {
            bail!(
                "invalid config for component type 'jsonl_file': emit_interval_ms must be greater than 0"
            );
        }
        source = source.with_emit_interval(Duration::from_millis(ms));
    }
    Ok(Box::new(source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn tmp_jsonl_file(content: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(&path, content).unwrap();
        dir
    }

    #[test]
    fn parse_line_dead_letter_entry() {
        let entry = DeadLetterEntry {
            envelope: Envelope::new("src", json!({"x": 1})),
            error: "something failed".to_string(),
            pipeline: "my-pipeline".to_string(),
            sink: "my-pipeline/sink0".to_string(),
            dead_lettered_at_ms: 1715234567890,
            attempts: 3,
        };
        let line = serde_json::to_string(&entry).unwrap();
        let env = parse_line(&line).unwrap();
        assert_eq!(env.meta.source_id, "src");
        assert_eq!(env.payload, json!({"x": 1}));
    }

    #[test]
    fn parse_line_bare_envelope() {
        let env = Envelope::new("src", json!({"y": 2}));
        let line = serde_json::to_string(&env).unwrap();
        let parsed = parse_line(&line).unwrap();
        assert_eq!(parsed.meta.source_id, "src");
        assert_eq!(parsed.payload, json!({"y": 2}));
    }

    #[test]
    fn parse_line_skips_unknown_format() {
        let line = r#"{"unknown":"data"}"#;
        assert!(parse_line(line).is_err());
    }

    #[tokio::test]
    async fn emits_envelopes_from_file() {
        let env1 = Envelope::new("src", json!({"i": 0}));
        let env2 = Envelope::new("src", json!({"i": 1}));
        let line1 = serde_json::to_string(&env1).unwrap();
        let line2 = serde_json::to_string(&env2).unwrap();
        let content = format!("{line1}\n{line2}\n");

        let dir = tmp_jsonl_file(&content);
        let path = dir.path().join("test.jsonl");

        let source = JsonlFileSource::new("test", path);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(async move { Box::new(source).run(tx, cancel).await });

        let received1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let received2 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(received1.payload, json!({"i": 0}));
        assert_eq!(received2.payload, json!({"i": 1}));

        drop(rx);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn skips_blank_lines() {
        let env = Envelope::new("src", json!({"ok": true}));
        let line = serde_json::to_string(&env).unwrap();
        let content = format!("\n  \n{line}\n\n");

        let dir = tmp_jsonl_file(&content);
        let path = dir.path().join("test.jsonl");

        let source = JsonlFileSource::new("test", path);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(async move { Box::new(source).run(tx, cancel).await });

        let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.payload, json!({"ok": true}));

        drop(rx);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn filters_dead_letter_entries_by_pipeline() {
        let entry1 = DeadLetterEntry {
            envelope: Envelope::new("src", json!({"pipeline": "first"})),
            error: "failed".to_string(),
            pipeline: "first".to_string(),
            sink: "first/sink0".to_string(),
            dead_lettered_at_ms: 1715234567890,
            attempts: 3,
        };
        let entry2 = DeadLetterEntry {
            envelope: Envelope::new("src", json!({"pipeline": "second"})),
            error: "failed".to_string(),
            pipeline: "second".to_string(),
            sink: "second/sink0".to_string(),
            dead_lettered_at_ms: 1715234567891,
            attempts: 3,
        };
        let bare = Envelope::new("src", json!({"pipeline": "bare"}));
        let content = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&entry1).unwrap(),
            serde_json::to_string(&entry2).unwrap(),
            serde_json::to_string(&bare).unwrap()
        );

        let dir = tmp_jsonl_file(&content);
        let path = dir.path().join("test.jsonl");

        let source = JsonlFileSource::new("test", path).with_dead_letter_pipeline("first");
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(async move { Box::new(source).run(tx, cancel).await });

        let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.payload, json!({"pipeline": "first"}));

        // Bare envelopes have no pipeline annotation, so they are emitted
        // for every pipeline filter.
        let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.payload, json!({"pipeline": "bare"}));

        assert!(
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .is_none()
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn filters_dead_letter_entries_by_sink() {
        let entry_sink0 = DeadLetterEntry {
            envelope: Envelope::new("src", json!({"which": "sink0"})),
            error: "failed".to_string(),
            pipeline: "p".to_string(),
            sink: "p/sink0".to_string(),
            dead_lettered_at_ms: 1715234567890,
            attempts: 3,
        };
        let entry_sink1 = DeadLetterEntry {
            envelope: Envelope::new("src", json!({"which": "sink1"})),
            error: "failed".to_string(),
            pipeline: "p".to_string(),
            sink: "p/sink1".to_string(),
            dead_lettered_at_ms: 1715234567891,
            attempts: 3,
        };
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&entry_sink0).unwrap(),
            serde_json::to_string(&entry_sink1).unwrap(),
        );

        let dir = tmp_jsonl_file(&content);
        let path = dir.path().join("test.jsonl");

        let source = JsonlFileSource::new("test", path)
            .with_dead_letter_pipeline("p")
            .with_dead_letter_sink("p/sink0");
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(async move { Box::new(source).run(tx, cancel).await });

        let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.payload, json!({"which": "sink0"}));

        assert!(
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .is_none()
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[test]
    fn factory_rejects_missing_path() {
        let err =
            jsonl_file_source_factory("test", json!({ "path": "/nonexistent/path.jsonl" }), None)
                .err()
                .unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("does not exist"), "{msg}");
    }

    #[test]
    fn factory_rejects_directory_path() {
        let dir = tempfile::tempdir().unwrap();

        let err = jsonl_file_source_factory(
            "test",
            json!({ "path": dir.path().to_str().unwrap() }),
            None,
        )
        .err()
        .unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("is not a file"), "{msg}");
    }

    #[test]
    fn factory_rejects_zero_emit_interval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(&path, "").unwrap();

        let err = jsonl_file_source_factory(
            "test",
            json!({ "path": path.to_str().unwrap(), "emit_interval_ms": 0 }),
            None,
        )
        .err()
        .unwrap();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("emit_interval_ms must be greater than 0"),
            "{msg}"
        );
    }
}
