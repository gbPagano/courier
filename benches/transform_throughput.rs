mod utils;

use std::collections::BTreeMap;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};

use utils::{
    ENVELOPE_COUNT, PYTHON_ENVELOPE_COUNT, fmt_throughput, print_table, run_pipeline,
    single_thread_rt,
};

fn bench_passthrough(c: &mut Criterion) {
    let rt = single_thread_rt();

    let mut group = c.benchmark_group("passthrough");
    group.throughput(Throughput::Elements(ENVELOPE_COUNT as u64));

    group.bench_function("0_transforms_source_to_sink", |b| {
        b.iter(|| {
            let count = rt.block_on(async { run_pipeline(ENVELOPE_COUNT, "native", 0).await });
            assert_eq!(count, ENVELOPE_COUNT as u64, "envelope count mismatch");
            count
        });
    });

    group.finish();
}

fn bench_chain(
    c: &mut Criterion,
    group_name: &str,
    runtime: &str,
    counts: &[usize],
    measurement_time: Option<Duration>,
    envelope_count: usize,
) {
    let rt = single_thread_rt();
    let mut group = c.benchmark_group(group_name);
    group.throughput(Throughput::Elements(envelope_count as u64));
    if let Some(d) = measurement_time {
        group.measurement_time(d);
    }
    for &count in counts {
        group.bench_with_input(BenchmarkId::new("transforms", count), &count, |b, &tc| {
            b.iter(|| {
                let received =
                    rt.block_on(async { run_pipeline(envelope_count, runtime, tc).await });
                assert_eq!(received, envelope_count as u64, "envelope count mismatch");
                received
            });
        });
    }
    group.finish();
}

fn bench_all_chains(c: &mut Criterion) {
    bench_chain(
        c,
        "chain_native",
        "native",
        &[1, 3, 5, 10, 20],
        None,
        ENVELOPE_COUNT,
    );
    bench_chain(
        c,
        "chain_rhai",
        "rhai",
        &[1, 3, 5, 10, 20],
        None,
        ENVELOPE_COUNT,
    );
    bench_chain(
        c,
        "chain_lua",
        "lua",
        &[1, 3, 5, 10, 20],
        None,
        ENVELOPE_COUNT,
    );
    bench_chain(
        c,
        "chain_python",
        "python",
        &[1, 3, 5],
        Some(Duration::from_secs(20)),
        PYTHON_ENVELOPE_COUNT,
    );
}

// --- Summary table ---

fn walk_results(dir: &std::path::Path, out: &mut BTreeMap<(String, usize), f64>) {
    utils::visit_criterion_samples(dir, &mut |sample| {
        if let (Some(rt), Some(tc)) = (
            resolve_runtime(sample.group),
            parse_tc(sample.function, sample.value_str),
        ) {
            out.insert((rt.to_string(), tc), sample.throughput);
        }
    });
}

fn resolve_runtime(group: &str) -> Option<&'static str> {
    match group {
        "passthrough" | "chain_native" => Some("native"),
        "chain_rhai" => Some("rhai"),
        "chain_lua" => Some("lua"),
        "chain_python" => Some("python"),
        _ => None,
    }
}

fn parse_tc(function: &str, value_str: Option<&str>) -> Option<usize> {
    match function {
        "0_transforms_source_to_sink" => Some(0),
        "transforms" => value_str?.parse().ok(),
        _ => None,
    }
}

fn print_summary_table() {
    let mut scenarios: BTreeMap<(String, usize), f64> = BTreeMap::new();
    let base = std::path::Path::new("target/criterion");
    if base.exists() {
        walk_results(base, &mut scenarios);
    }
    if scenarios.is_empty() {
        return;
    }

    let runtimes = ["native", "rhai", "lua", "python"];
    let mut counts: Vec<usize> = scenarios.keys().map(|(_, c)| *c).collect();
    counts.sort_unstable();
    counts.dedup();

    let row_keys: Vec<String> = counts.iter().map(|c| c.to_string()).collect();
    let cells: Vec<Vec<String>> = counts
        .iter()
        .map(|&tc| {
            runtimes
                .iter()
                .map(|&rt| match scenarios.get(&(rt.to_string(), tc)) {
                    Some(&thr) => fmt_throughput(thr),
                    None => "—".to_string(),
                })
                .collect()
        })
        .collect();

    print_table(
        "Throughput Summary (envelopes / sec)",
        "Transforms",
        &row_keys,
        &runtimes,
        &cells,
    );
}

// --- Entry point ---

criterion_group! {
    name = benches;
    config = criterion::Criterion::default()
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(8))
        .sample_size(10);
    targets = bench_passthrough, bench_all_chains
}

fn main() {
    benches();
    criterion::Criterion::default()
        .configure_from_args()
        .final_summary();
    print_summary_table();
}
