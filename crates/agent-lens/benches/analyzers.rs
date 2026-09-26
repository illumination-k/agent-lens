//! One benchmark per whole-tree analyzer, each over the same corpus, so a
//! regression is attributed to the analyzer that owns it rather than
//! surfacing only as a slower profile run. The Rust crate carries cross-
//! module calls, `impl` blocks and forwarders; the Go, TypeScript and
//! Python corpora cover each language adapter's parse and extraction path.
//!
//! Every analyzer runs outside an `AnalysisIndexScope`, so each iteration
//! pays its own parses and graph builds, as a single `analyze` call does.

mod support;

use agent_lens::analyze::{
    CohesionAnalyzer, CommunitiesAnalyzer, ComplexityAnalyzer, ContextSpanAnalyzer,
    CouplingAnalyzer, CyclesAnalyzer, DelegationAnalyzer, HubsAnalyzer, LayersAnalyzer,
    OutputFormat, WrapperAnalyzer,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

const JSON: OutputFormat = OutputFormat::Json;

fn bench_rust_analyzers(c: &mut Criterion) {
    let dir = support::corpus(|root| support::write_rust_crate_corpus(root, 32, 32));
    let root = dir.path();
    let mut group = c.benchmark_group("analyzers_rust_1024_functions");

    let complexity = ComplexityAnalyzer::new();
    group.bench_function("complexity", |b| {
        b.iter(|| support::consume(complexity.analyze(root, JSON)));
    });
    let cohesion = CohesionAnalyzer::new();
    group.bench_function("cohesion", |b| {
        b.iter(|| support::consume(cohesion.analyze(root, JSON)));
    });
    let wrapper = WrapperAnalyzer::new();
    group.bench_function("wrapper", |b| {
        b.iter(|| support::consume(wrapper.analyze(root, JSON)));
    });
    let delegation = DelegationAnalyzer::new();
    group.bench_function("delegation", |b| {
        b.iter(|| support::consume(delegation.analyze(root, JSON)));
    });
    let cycles = CyclesAnalyzer::new();
    group.bench_function("cycles", |b| {
        b.iter(|| support::consume(cycles.analyze(root, JSON)));
    });
    let hubs = HubsAnalyzer::new();
    group.bench_function("hubs", |b| {
        b.iter(|| support::consume(hubs.analyze(root, JSON)));
    });
    let layers = LayersAnalyzer::new();
    group.bench_function("layers", |b| {
        b.iter(|| support::consume(layers.analyze(root, JSON)));
    });
    let coupling = CouplingAnalyzer::new();
    group.bench_function("coupling", |b| {
        b.iter(|| support::consume(coupling.analyze(root, JSON)));
    });
    let communities = CommunitiesAnalyzer::new();
    group.bench_function("communities", |b| {
        b.iter(|| support::consume(communities.analyze(root, JSON)));
    });
    let context_span = ContextSpanAnalyzer::new();
    group.bench_function("context_span", |b| {
        b.iter(|| support::consume(context_span.analyze(root, JSON)));
    });
    group.finish();
}

fn bench_language_adapters(c: &mut Criterion) {
    let go = support::corpus(|root| support::write_go_module_corpus(root, 32, 32));
    let ts = support::corpus(|root| support::write_ts_corpus(root, 32, 32));
    let py = support::corpus(|root| support::write_py_corpus(root, 32, 32));
    let complexity = ComplexityAnalyzer::new();
    let cohesion = CohesionAnalyzer::new();
    let mut group = c.benchmark_group("analyzers_language_1024_functions");
    for (lang, dir) in [("go", &go), ("ts", &ts), ("py", &py)] {
        group.bench_with_input(BenchmarkId::new("complexity", lang), dir, |b, dir| {
            b.iter(|| support::consume(complexity.analyze(dir.path(), JSON)));
        });
        group.bench_with_input(BenchmarkId::new("cohesion", lang), dir, |b, dir| {
            b.iter(|| support::consume(cohesion.analyze(dir.path(), JSON)));
        });
    }
    let coupling = CouplingAnalyzer::new();
    group.bench_function("coupling/go", |b| {
        b.iter(|| support::consume(coupling.analyze(go.path(), JSON)));
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(20)
        .warm_up_time(std::time::Duration::from_secs(1));
    targets = bench_rust_analyzers, bench_language_adapters
}
criterion_main!(benches);
