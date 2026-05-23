#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use futures::future::join_all;
use serde_json::json;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{Sender, channel};
use tokio_util::sync::CancellationToken;

use courier::envelope::Envelope;
use courier::pipeline::ErrorPolicy;
use courier::sinks::{ManagedSink, Sink, WriteOne};
use courier::sources::Source;
use courier::transforms::script::script_transform_factory;
use courier::transforms::{BasicTransform, MapOne, Transform};

pub const ENVELOPE_COUNT: usize = 10_000;
pub const PYTHON_ENVELOPE_COUNT: usize = 1_000;
pub const CHANNEL_CAPACITY: usize = 1024;

pub fn single_thread_rt() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

pub struct BurstSource {
    pub count: usize,
}

#[async_trait]
impl Source for BurstSource {
    fn id(&self) -> &str {
        "bench-src"
    }

    async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
        for i in 0..self.count {
            let env = Envelope::new("bench-src", json!({ "n": i }));
            tokio::select! {
                _ = cancel.cancelled() => break,
                res = tx.send(env) => {
                    if res.is_err() {
                        break;
                    }
                }
            }
        }
    }
}

pub struct CountWriteOne {
    pub received: Arc<AtomicU64>,
}

#[async_trait]
impl WriteOne for CountWriteOne {
    fn id(&self) -> &str {
        "bench-sink"
    }

    async fn write(&self, _env: &Envelope) -> Result<()> {
        self.received.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

struct NativeAddField;

#[async_trait]
impl MapOne for NativeAddField {
    fn id(&self) -> &str {
        "native_add_field"
    }

    async fn map(&self, mut env: Envelope) -> Result<Option<Envelope>> {
        env.payload["processed"] = json!(true);
        Ok(Some(env))
    }
}

fn build_native_transform(_index: usize) -> Box<dyn Transform> {
    Box::new(BasicTransform::new(NativeAddField))
}

fn build_rhai_transform(index: usize) -> Box<dyn Transform> {
    script_transform_factory(
        &format!("bench/t{index}"),
        json!({
            "runtime": "rhai",
            "script": "fn transform(env) { env.payload.processed = true; env }"
        }),
        ErrorPolicy::Drop,
    )
    .expect("failed to build Rhai transform")
}

fn build_lua_transform(index: usize) -> Box<dyn Transform> {
    script_transform_factory(
        &format!("bench/t{index}"),
        json!({
            "runtime": "lua",
            "script": "function transform(env) env.payload.processed = true; return env end"
        }),
        ErrorPolicy::Drop,
    )
    .expect("failed to build Lua transform")
}

fn build_python_transform(index: usize) -> Box<dyn Transform> {
    script_transform_factory(
        &format!("bench/t{index}"),
        json!({
            "runtime": "python",
            "script": "def transform(env):\n    env[\"payload\"][\"processed\"] = True\n    return env\n"
        }),
        ErrorPolicy::Drop,
    )
    .expect("failed to build Python transform (is python3 installed?)")
}

pub fn build_transform(runtime: &str, index: usize) -> Box<dyn Transform> {
    match runtime {
        "native" => build_native_transform(index),
        "rhai" => build_rhai_transform(index),
        "lua" => build_lua_transform(index),
        "python" => build_python_transform(index),
        other => panic!("unknown transform runtime: {other}"),
    }
}

async fn execute_pipeline(
    source: Box<dyn Source>,
    transforms: Vec<Box<dyn Transform>>,
    sink: Box<dyn Sink>,
) {
    let cancel = CancellationToken::new();
    let (src_tx, mut prev_rx) = channel(CHANNEL_CAPACITY);
    let cancel_src = cancel.clone();
    let mut handles = vec![];

    handles.push(tokio::spawn(async move {
        source.run(src_tx, cancel_src).await;
    }));

    for t in transforms {
        let next_cancel = cancel.clone();
        let (next_tx, next_rx) = channel(CHANNEL_CAPACITY);
        handles.push(tokio::spawn(async move {
            t.run(prev_rx, next_tx, next_cancel).await;
        }));
        prev_rx = next_rx;
    }

    let sink_cancel = cancel.clone();
    handles.push(tokio::spawn(async move {
        sink.run(prev_rx, sink_cancel).await;
    }));

    join_all(handles).await;
}

pub async fn run_pipeline(envelope_count: usize, runtime: &str, transform_count: usize) -> u64 {
    let received = Arc::new(AtomicU64::new(0));
    let source = Box::new(BurstSource {
        count: envelope_count,
    });
    let sink: Box<dyn Sink> = Box::new(ManagedSink::new(CountWriteOne {
        received: received.clone(),
    }));
    let transforms: Vec<Box<dyn Transform>> = (0..transform_count)
        .map(|i| build_transform(runtime, i))
        .collect();
    execute_pipeline(source, transforms, sink).await;
    received.load(Ordering::SeqCst)
}

/// One Criterion benchmark sample: a `(group_id, function_id, value_str)`
/// triple from `benchmark.json` plus the mean throughput in envelopes/sec.
pub struct CriterionSample<'a> {
    pub group: &'a str,
    pub function: &'a str,
    pub value_str: Option<&'a str>,
    pub throughput: f64,
}

/// Recursively walk a Criterion result tree (typically `target/criterion`) and
/// invoke `visit` with each leaf sample. Leaves are directories that contain
/// both `new/estimates.json` and `new/benchmark.json`. Skips the `report`
/// subdirectory Criterion writes for HTML output.
pub fn visit_criterion_samples(dir: &std::path::Path, visit: &mut dyn FnMut(CriterionSample<'_>)) {
    let new_dir = dir.join("new");
    let estimates_path = new_dir.join("estimates.json");
    let meta_path = new_dir.join("benchmark.json");
    if estimates_path.exists() && meta_path.exists() {
        // `?` chains on Option live inside this closure so an unreadable or
        // malformed sample directory is just skipped, not propagated.
        let mut parse = || -> Option<()> {
            let e: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&estimates_path).ok()?).ok()?;
            let m: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&meta_path).ok()?).ok()?;
            let mean_ns = e["mean"]["point_estimate"].as_f64()?;
            if mean_ns <= 0.0 {
                return None;
            }
            let elements = m["throughput"]["Elements"].as_u64()?;
            let throughput = elements as f64 / (mean_ns / 1e9);
            visit(CriterionSample {
                group: m["group_id"].as_str()?,
                function: m["function_id"].as_str()?,
                value_str: m["value_str"].as_str(),
                throughput,
            });
            Some(())
        };
        parse();
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.file_name().map(|n| n != "report").unwrap_or(true) {
                visit_criterion_samples(&path, visit);
            }
        }
    }
}

pub fn print_table(
    title: &str,
    row_label: &str,
    row_keys: &[String],
    headers: &[&str],
    cells: &[Vec<String>],
) {
    let lw = row_label
        .len()
        .max(row_keys.iter().map(|k| k.len()).max().unwrap_or(0));
    let cws: Vec<usize> = (0..headers.len())
        .map(|i| {
            headers[i]
                .len()
                .max(cells.iter().map(|row| row[i].len()).max().unwrap_or(0))
        })
        .collect();

    let sep: Vec<String> = cws.iter().map(|&w| "─".repeat(w + 2)).collect();
    let top = format!("╭{}┬{}╮", "─".repeat(lw + 2), sep.join("┬"));
    let mid = format!("├{}┼{}┤", "─".repeat(lw + 2), sep.join("┼"));
    let bot = format!("╰{}┴{}╯", "─".repeat(lw + 2), sep.join("┴"));

    let inner_w = lw + 2 + cws.iter().map(|w| w + 2 + 1).sum::<usize>();
    let pad = inner_w.saturating_sub(title.len());
    let title_line = format!("  {}{}", title, " ".repeat(pad));

    let hdr: String = headers
        .iter()
        .zip(&cws)
        .map(|(&h, &w)| format!(" {:^width$} ", h, width = w))
        .collect::<Vec<_>>()
        .join("│");
    let header = format!("│ {:>width$} │{hdr}│", row_label, width = lw);

    let mut out = format!("\n{title_line}\n{top}\n{header}\n{mid}\n");
    for (i, key) in row_keys.iter().enumerate() {
        let cols: String = cells[i]
            .iter()
            .zip(&cws)
            .map(|(val, &w)| format!(" {:>width$} ", val, width = w))
            .collect::<Vec<_>>()
            .join("│");
        out.push_str(&format!("│ {:>width$} │{cols}│\n", key, width = lw));
    }
    out.push_str(&bot);
    out.push('\n');

    eprintln!("{out}");
}

pub fn fmt_throughput(thr: f64) -> String {
    if thr >= 1_000_000.0 {
        format!("{:.2}M", thr / 1_000_000.0)
    } else if thr >= 1_000.0 {
        format!("{:.1}K", thr / 1_000.0)
    } else {
        format!("{thr:.0}")
    }
}

pub async fn run_pipeline_fixed() -> u64 {
    let received = Arc::new(AtomicU64::new(0));
    let source = Box::new(BurstSource {
        count: ENVELOPE_COUNT,
    });
    let sink: Box<dyn Sink> = Box::new(ManagedSink::new(CountWriteOne {
        received: received.clone(),
    }));
    let transforms: Vec<Box<dyn Transform>> = vec![
        build_native_transform(0),
        build_native_transform(1),
        build_rhai_transform(2),
    ];
    execute_pipeline(source, transforms, sink).await;
    received.load(Ordering::SeqCst)
}

pub async fn run_pipelines(pipeline_count: usize) -> u64 {
    let handles: Vec<_> = (0..pipeline_count)
        .map(|_| tokio::spawn(run_pipeline_fixed()))
        .collect();
    join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .sum()
}
