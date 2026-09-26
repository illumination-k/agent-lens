//! Benchmark for static call-graph construction.
//!
//! Four of the planned graph analyzers rebuild the full function graph
//! per invocation, so construction cost is the shared regression
//! surface. The Rust corpus exercises every resolver path that matters
//! at scale (see `support::write_rust_graph_corpus`); the Go, TypeScript
//! and Python corpora cover the other language adapters' resolvers.

mod support;

use agent_lens::analyze::{FunctionGraphAnalyzer, OutputFormat};
use criterion::{Criterion, criterion_group, criterion_main};

fn bench_function_graph(c: &mut Criterion) {
    let small = support::corpus(|root| support::write_rust_graph_corpus(root, 4, 16));
    let large = support::corpus(|root| support::write_rust_graph_corpus(root, 32, 32));
    let go = support::corpus(|root| support::write_go_module_corpus(root, 32, 32));
    let ts = support::corpus(|root| support::write_ts_corpus(root, 32, 32));
    let py = support::corpus(|root| support::write_py_corpus(root, 32, 32));
    let analyzer = FunctionGraphAnalyzer::new();

    for (id, dir) in [
        ("function_graph_directory_64_functions", &small),
        ("function_graph_directory_1024_functions", &large),
        ("function_graph_go_1024_functions", &go),
        ("function_graph_ts_1024_functions", &ts),
        ("function_graph_py_1024_functions", &py),
    ] {
        c.bench_function(id, |b| {
            b.iter(|| support::consume(analyzer.analyze(dir.path(), OutputFormat::Json)));
        });
    }
}

criterion_group!(benches, bench_function_graph);
criterion_main!(benches);
