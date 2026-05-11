mod utils;

use std::collections::BTreeMap;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};

use utils::{ENVELOPE_COUNT, fmt_throughput, print_table, run_pipelines};

fn bench_pipeline_scaling(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let pipeline_counts = [1usize, 5, 10, 25, 50, 100, 500];

    let mut group = c.benchmark_group("pipeline_scaling");
    for &count in &pipeline_counts {
        group.throughput(Throughput::Elements(ENVELOPE_COUNT as u64));
        group.bench_with_input(BenchmarkId::new("pipelines", count), &count, |b, &pc| {
            b.iter(|| rt.block_on(run_pipelines(pc)));
        });
    }
    group.finish();
}

// --- Summary table ---

fn walk_pipeline_results(dir: &std::path::Path, results: &mut BTreeMap<usize, f64>) {
    let new_dir = dir.join("new");
    let estimates_path = new_dir.join("estimates.json");
    let meta_path = new_dir.join("benchmark.json");

    if estimates_path.exists() && meta_path.exists() {
        let mut parse = || -> Option<()> {
            let e: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&estimates_path).ok()?).ok()?;
            let m: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&meta_path).ok()?).ok()?;
            let mean_ns = e["mean"]["point_estimate"].as_f64()?;
            let elements = m["throughput"]["Elements"].as_u64()?;
            let count: usize = m["value_str"].as_str()?.parse().ok()?;
            if mean_ns > 0.0 {
                results.insert(count, elements as f64 / (mean_ns / 1e9));
            }
            Some(())
        };
        parse();
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.file_name().map(|n| n != "report").unwrap_or(true) {
            walk_pipeline_results(&path, results);
        }
    }
}

fn print_pipeline_table() {
    let mut results: BTreeMap<usize, f64> = BTreeMap::new();
    let base = std::path::Path::new("target/criterion");
    if base.exists() {
        walk_pipeline_results(base, &mut results);
    }
    if results.is_empty() {
        return;
    }

    let headers = ["total (env/s)", "per-pipeline (env/s)"];
    let row_keys: Vec<String> = results.keys().map(|c| c.to_string()).collect();
    let cells: Vec<Vec<String>> = results
        .iter()
        .map(|(&count, &total)| vec![fmt_throughput(total * count as f64), fmt_throughput(total)])
        .collect();

    print_table(
        "Pipeline Scaling (2× native + 1× rhai each)",
        "Pipelines",
        &row_keys,
        &headers,
        &cells,
    );
}

// --- Entry point ---

criterion_group! {
    name = benches;
    config = criterion::Criterion::default()
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(30))
        .sample_size(10);
    targets = bench_pipeline_scaling
}

fn main() {
    benches();
    criterion::Criterion::default()
        .configure_from_args()
        .final_summary();
    print_pipeline_table();
}
