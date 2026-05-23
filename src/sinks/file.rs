use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;

use crate::config::{parse_config, redact_secret_path};
use crate::envelope::Envelope;
use crate::pipeline::ErrorPolicy;
use crate::retry::RetryPolicy;
use crate::sinks::{ManagedSink, Sink, WriteOne};

/// Local file sink. Appends one record per envelope to the configured path
/// in either JSON Lines or CSV format.
///
/// The file is opened lazily on first write, then kept in append mode and
/// flushed after every write so validation can build this sink without file
/// system side effects. Streaming writes are not transactional: a retried
/// write after a partial failure may produce a duplicate row — `WriteOne`
/// semantics, same as any other `ManagedSink`-wrapped sink.
pub struct FileSink {
    id: String,
    path: PathBuf,
    format: Format,
    state: Mutex<WriterState>,
}

/// Output shape. JSONL emits one serialized JSON value per line; CSV emits
/// a row of configured columns with RFC 4180 quoting.
#[derive(Debug, Clone)]
pub enum Format {
    Jsonl {
        body: BodyFormat,
    },
    /// CSV columns are dotted paths evaluated against the full envelope —
    /// e.g. `payload.id`, `meta.source_id`, `meta.headers.priority`.
    Csv {
        columns: Vec<String>,
    },
}

/// Which slice of the envelope to serialize per JSONL line.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    #[default]
    Payload,
    Envelope,
}

struct WriterState {
    writer: Option<BufWriter<File>>,
    /// True until the CSV header row has been written. Always false for
    /// JSONL. Refined from file size at lazy-open time so restarts on a
    /// non-empty file resume cleanly without re-emitting headers.
    needs_header: bool,
}

impl FileSink {
    pub fn new(id: impl Into<String>, path: impl Into<PathBuf>, format: Format) -> Result<Self> {
        let path = path.into();
        Ok(Self {
            id: id.into(),
            path,
            state: Mutex::new(WriterState {
                writer: None,
                needs_header: matches!(&format, Format::Csv { .. }),
            }),
            format,
        })
    }

    fn ensure_open(&self, state: &mut WriterState) -> Result<()> {
        if state.writer.is_some() {
            return Ok(());
        }

        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow!(
                    "failed to create parent dir for {}: {e}",
                    redact_secret_path(&self.path)
                )
            })?;
        }

        if matches!(&self.format, Format::Csv { .. }) {
            state.needs_header = std::fs::metadata(&self.path)
                .map(|m| m.len() == 0)
                .unwrap_or(true);
        }

        let std_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| anyhow!("failed to open {}: {e}", redact_secret_path(&self.path)))?;
        state.writer = Some(BufWriter::new(File::from_std(std_file)));
        Ok(())
    }
}

#[async_trait]
impl WriteOne for FileSink {
    fn id(&self) -> &str {
        &self.id
    }

    async fn write(&self, env: &Envelope) -> Result<()> {
        match &self.format {
            Format::Jsonl { body } => self.write_jsonl(env, *body).await,
            Format::Csv { columns } => self.write_csv(env, columns).await,
        }
    }
}

impl FileSink {
    async fn write_jsonl(&self, env: &Envelope, body: BodyFormat) -> Result<()> {
        let line = match body {
            BodyFormat::Payload => serde_json::to_string(&env.payload)?,
            BodyFormat::Envelope => serde_json::to_string(env)?,
        };
        let mut buf = line;
        buf.push('\n');
        let mut state = self.state.lock().await;
        self.ensure_open(&mut state)?;
        self.commit(&mut state, buf.as_bytes()).await
    }

    async fn write_csv(&self, env: &Envelope, columns: &[String]) -> Result<()> {
        let env_value = serde_json::to_value(env)?;
        let mut state = self.state.lock().await;
        self.ensure_open(&mut state)?;
        let wrote_header = state.needs_header;
        let mut buf = String::new();
        if wrote_header {
            write_csv_row(&mut buf, columns.iter().map(String::as_str));
        }
        let row_strings: Vec<String> = columns
            .iter()
            .map(|col| extract_csv_cell(&env_value, col))
            .collect();
        write_csv_row(&mut buf, row_strings.iter().map(String::as_str));

        self.commit(&mut state, buf.as_bytes()).await?;
        // Only clear `needs_header` after a successful flush so a write that
        // fails mid-flight leaves the header pending for the next attempt.
        if wrote_header {
            state.needs_header = false;
        }
        Ok(())
    }

    /// Append `bytes` to the open writer and flush. The writer must already be
    /// opened (the callers run `ensure_open` under the same lock).
    async fn commit(&self, state: &mut WriterState, bytes: &[u8]) -> Result<()> {
        let writer = state.writer.as_mut().expect("writer is opened above");
        writer
            .write_all(bytes)
            .await
            .map_err(|e| anyhow!("write to {} failed: {e}", redact_secret_path(&self.path)))?;
        writer
            .flush()
            .await
            .map_err(|e| anyhow!("flush of {} failed: {e}", redact_secret_path(&self.path)))?;
        Ok(())
    }
}

/// Pull a CSV cell value from the envelope using a dotted path. Missing
/// fields render as an empty cell; container values (objects, arrays)
/// are JSON-encoded so they still round-trip.
fn extract_csv_cell(env: &Value, dotted: &str) -> String {
    let mut current = env;
    for segment in dotted.split('.') {
        match current.get(segment) {
            Some(v) => current = v,
            None => return String::new(),
        }
    }
    match current {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        // Fall back to JSON for nested structures so they remain machine-readable.
        other => other.to_string(),
    }
}

fn write_csv_row<'a>(buf: &mut String, fields: impl Iterator<Item = &'a str>) {
    let mut first = true;
    for field in fields {
        if !first {
            buf.push(',');
        }
        first = false;
        write_csv_field(buf, field);
    }
    buf.push('\n');
}

fn write_csv_field(buf: &mut String, field: &str) {
    let needs_quoting =
        field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r');
    if !needs_quoting {
        buf.push_str(field);
        return;
    }
    buf.push('"');
    for c in field.chars() {
        if c == '"' {
            buf.push('"');
        }
        buf.push(c);
    }
    buf.push('"');
}

#[derive(Debug, Deserialize)]
struct FileSinkConfig {
    path: PathBuf,
    #[serde(default)]
    format: FormatConfig,
    #[serde(default)]
    body: BodyFormat,
    #[serde(default)]
    columns: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FormatConfig {
    #[default]
    Jsonl,
    Csv,
}

/// Registry factory for [`FileSink`]. Registered by
/// `courier::registry::register_builtin` under kind `"file"`.
///
/// Retry and error policy are managed centrally by the registry and applied
/// to every sink uniformly — no per-sink config needed.
pub fn file_sink_factory(
    id: &str,
    config: Value,
    on_error: ErrorPolicy,
    retry: Option<RetryPolicy>,
) -> Result<Box<dyn Sink>> {
    let config: FileSinkConfig = parse_config("file", config)?;

    let format = match config.format {
        FormatConfig::Jsonl => Format::Jsonl { body: config.body },
        FormatConfig::Csv => {
            if config.columns.is_empty() {
                return Err(anyhow!(
                    "invalid config for component type 'file': csv format requires a non-empty 'columns' list"
                ));
            }
            Format::Csv {
                columns: config.columns,
            }
        }
    };

    let file = FileSink::new(id, config.path, format)?;
    let mut sink = ManagedSink::new(file).with_error_policy(on_error);
    if let Some(policy) = retry {
        sink = sink.with_retry(policy);
    }
    Ok(Box::new(sink))
}

/// Public convenience: open a JSONL file sink. Used by tests and by callers
/// wiring pipelines without going through the config layer.
pub fn jsonl(id: impl Into<String>, path: impl AsRef<Path>) -> Result<FileSink> {
    FileSink::new(
        id,
        path.as_ref().to_path_buf(),
        Format::Jsonl {
            body: BodyFormat::Payload,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[tokio::test]
    async fn jsonl_writes_one_payload_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.jsonl");
        let sink = FileSink::new(
            "file",
            &path,
            Format::Jsonl {
                body: BodyFormat::Payload,
            },
        )
        .unwrap();

        sink.write(&Envelope::new("src", json!({ "n": 1 })))
            .await
            .unwrap();
        sink.write(&Envelope::new("src", json!({ "n": 2 })))
            .await
            .unwrap();

        let contents = read(&path);
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first, json!({ "n": 1 }));
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second, json!({ "n": 2 }));
    }

    #[tokio::test]
    async fn jsonl_envelope_mode_persists_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.jsonl");
        let sink = FileSink::new(
            "file",
            &path,
            Format::Jsonl {
                body: BodyFormat::Envelope,
            },
        )
        .unwrap();

        let mut env = Envelope::new("src", json!({ "id": 7 }));
        env.meta.key = Some("k1".into());
        sink.write(&env).await.unwrap();

        let line = read(&path);
        let parsed: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["payload"], json!({ "id": 7 }));
        assert_eq!(parsed["meta"]["source_id"], "src");
        assert_eq!(parsed["meta"]["key"], "k1");
    }

    #[tokio::test]
    async fn jsonl_appends_across_sink_instances() {
        // Important for restart-friendliness: a fresh FileSink pointed at
        // an existing file must extend it, not truncate.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.jsonl");

        let sink = FileSink::new(
            "file",
            &path,
            Format::Jsonl {
                body: BodyFormat::Payload,
            },
        )
        .unwrap();
        sink.write(&Envelope::new("src", json!({ "n": 1 })))
            .await
            .unwrap();
        drop(sink);

        let sink = FileSink::new(
            "file",
            &path,
            Format::Jsonl {
                body: BodyFormat::Payload,
            },
        )
        .unwrap();
        sink.write(&Envelope::new("src", json!({ "n": 2 })))
            .await
            .unwrap();
        drop(sink);

        let contents = read(&path);
        assert_eq!(contents.lines().count(), 2);
    }

    #[tokio::test]
    async fn csv_writes_header_then_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        let sink = FileSink::new(
            "file",
            &path,
            Format::Csv {
                columns: vec!["payload.id".into(), "payload.name".into()],
            },
        )
        .unwrap();

        sink.write(&Envelope::new("src", json!({ "id": 1, "name": "alice" })))
            .await
            .unwrap();
        sink.write(&Envelope::new("src", json!({ "id": 2, "name": "bob" })))
            .await
            .unwrap();

        let contents = read(&path);
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines, vec!["payload.id,payload.name", "1,alice", "2,bob"]);
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn csv_keeps_header_pending_after_write_failure() {
        let dev_full = Path::new("/dev/full");
        if !dev_full.exists() {
            return;
        }

        let sink = FileSink::new(
            "file",
            dev_full,
            Format::Csv {
                columns: vec!["payload.id".into()],
            },
        )
        .unwrap();

        sink.write(&Envelope::new("src", json!({ "id": 1 })))
            .await
            .expect_err("expected write to /dev/full to fail");

        let state = sink.state.lock().await;
        assert!(
            state.needs_header,
            "header should remain pending until write and flush succeed"
        );
    }

    #[tokio::test]
    async fn csv_skips_header_when_appending_to_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        std::fs::write(&path, "id,name\n0,seed\n").unwrap();

        let sink = FileSink::new(
            "file",
            &path,
            Format::Csv {
                columns: vec!["payload.id".into(), "payload.name".into()],
            },
        )
        .unwrap();
        sink.write(&Envelope::new("src", json!({ "id": 1, "name": "alice" })))
            .await
            .unwrap();
        drop(sink);

        let contents = read(&path);
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines, vec!["id,name", "0,seed", "1,alice"]);
    }

    #[tokio::test]
    async fn csv_quotes_fields_with_special_characters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        let sink = FileSink::new(
            "file",
            &path,
            Format::Csv {
                columns: vec!["payload.text".into(), "payload.note".into()],
            },
        )
        .unwrap();

        sink.write(&Envelope::new(
            "src",
            json!({
                "text": "hello, world",
                "note": "She said \"hi\"\nto me",
            }),
        ))
        .await
        .unwrap();

        let contents = read(&path);
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines[0], "payload.text,payload.note");
        // The newline embedded inside the quoted field splits across raw
        // lines — assert via a contains check rather than line index.
        assert!(contents.contains("\"hello, world\""));
        assert!(contents.contains("\"She said \"\"hi\"\"\nto me\""));
    }

    #[tokio::test]
    async fn csv_pulls_from_meta_via_dotted_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        let sink = FileSink::new(
            "file",
            &path,
            Format::Csv {
                columns: vec![
                    "meta.source_id".into(),
                    "meta.key".into(),
                    "payload.v".into(),
                    "payload.missing".into(),
                ],
            },
        )
        .unwrap();

        let mut env = Envelope::new("src", json!({ "v": 42 }));
        env.meta.key = Some("k".into());
        sink.write(&env).await.unwrap();

        let contents = read(&path);
        let last = contents.lines().last().unwrap();
        assert_eq!(last, "src,k,42,");
    }

    #[tokio::test]
    async fn creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/sub/out.jsonl");
        let sink = jsonl("file", &path).unwrap();
        sink.write(&Envelope::new("src", json!({ "n": 1 })))
            .await
            .unwrap();
        assert!(path.exists());
    }

    // -----------------------------------------------------------------
    // Factory / config parsing
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn factory_defaults_to_jsonl_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.jsonl");

        let sink = file_sink_factory(
            "file",
            json!({ "path": path.to_str().unwrap() }),
            ErrorPolicy::Drop,
            None,
        )
        .unwrap();

        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let cancel = tokio_util::sync::CancellationToken::new();
        let handle = tokio::spawn(async move { sink.run(rx, cancel).await });

        tx.send(Envelope::new("src", json!({ "v": 1 })))
            .await
            .unwrap();
        drop(tx);
        handle.await.unwrap();

        let contents = read(&path);
        let parsed: Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(parsed, json!({ "v": 1 }));
    }

    #[test]
    fn factory_rejects_csv_without_columns() {
        let dir = tempfile::tempdir().unwrap();
        let err = file_sink_factory(
            "file",
            json!({
                "path": dir.path().join("x.csv").to_str().unwrap(),
                "format": "csv",
            }),
            ErrorPolicy::Drop,
            None,
        )
        .err()
        .expect("expected csv-without-columns error");
        let msg = format!("{err:#}");
        assert!(msg.contains("csv format requires"), "{msg}");
    }

    #[test]
    fn factory_reports_missing_path_with_uniform_prefix() {
        let err = file_sink_factory("file", json!({}), ErrorPolicy::Drop, None)
            .err()
            .expect("expected missing-path error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid config for component type 'file'"),
            "{msg}"
        );
    }
}
