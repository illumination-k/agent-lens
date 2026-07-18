//! Benchmark for static call-graph construction.
//!
//! Four of the planned graph analyzers rebuild the full function graph
//! per invocation, so construction cost is the shared regression
//! surface. The corpus is a synthetic Rust module tree with
//! cross-module calls exercising every resolver path that matters at
//! scale: lexical hits, `crate::`-prefixed paths, receiver-method
//! fallbacks, duplicate names (ambiguity), and external calls
//! (unresolved).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent_lens::analyze::{FunctionGraphAnalyzer, OutputFormat};
use criterion::{Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

fn bench_function_graph(c: &mut Criterion) {
    let small = graph_bench_corpus(4, 16);
    let large = graph_bench_corpus(32, 32);
    let analyzer = FunctionGraphAnalyzer::new();

    c.bench_function("function_graph_directory_64_functions", |b| {
        b.iter(|| {
            let report = match analyzer.analyze(small.path(), OutputFormat::Json) {
                Ok(report) => report,
                Err(err) => panic!("function-graph benchmark failed: {err}"),
            };
            std::hint::black_box(report.len());
        });
    });

    c.bench_function("function_graph_directory_1024_functions", |b| {
        b.iter(|| {
            let report = match analyzer.analyze(large.path(), OutputFormat::Json) {
                Ok(report) => report,
                Err(err) => panic!("function-graph benchmark failed: {err}"),
            };
            std::hint::black_box(report.len());
        });
    });
}

fn graph_bench_corpus(file_count: usize, functions_per_file: usize) -> TempDir {
    let dir = tempfile::tempdir().unwrap_or_else(|err| {
        panic!("failed to create benchmark tempdir: {err}");
    });
    write_graph_corpus(dir.path(), file_count, functions_per_file).unwrap_or_else(|err| {
        panic!("failed to write benchmark corpus: {err}");
    });
    dir
}

fn write_graph_corpus(
    root: &Path,
    file_count: usize,
    functions_per_file: usize,
) -> std::io::Result<()> {
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"graph-bench\"\n",
    )?;
    let mut lib = String::new();
    for file_idx in 0..file_count {
        let _ = writeln!(lib, "pub mod module_{file_idx:02};");
    }
    std::fs::write(root.join("src/lib.rs"), lib)?;

    for file_idx in 0..file_count {
        let path: PathBuf = root.join(format!("src/module_{file_idx:02}.rs"));
        let mut src = String::new();
        let neighbor = (file_idx + 1) % file_count;
        let _ = writeln!(src, "use crate::module_{neighbor:02};");
        let _ = writeln!(src, "pub struct Widget_{file_idx:02};");
        let _ = writeln!(
            src,
            "impl Widget_{file_idx:02} {{ pub fn refresh(&self) {{}} }}"
        );
        for fn_idx in 0..functions_per_file {
            let next = (fn_idx + 1) % functions_per_file;
            let _ = writeln!(
                src,
                r#"
pub fn generated_{file_idx:02}_{fn_idx:03}(widget: &Widget_{file_idx:02}) -> i64 {{
    // Lexical same-module call plus a cross-module path call.
    let local = generated_{file_idx:02}_{next:03};
    std::hint::black_box(&local);
    crate::module_{neighbor:02}::generated_{neighbor:02}_{fn_idx:03}(widget_of_{neighbor:02}());
    module_{neighbor:02}::shared_helper();
    // Receiver method (last-segment fallback) and an ambiguous name.
    widget.refresh();
    shared_helper();
    // External call stays unresolved.
    external_dependency_call({fn_idx});
    {fn_idx}
}}
"#,
            );
        }
        let _ = writeln!(src, "pub fn shared_helper() {{}}");
        let _ = writeln!(
            src,
            "pub fn widget_of_{neighbor:02}() -> &'static Widget_{neighbor:02} {{ unimplemented!() }}"
        );
        std::fs::write(&path, src)?;
    }
    Ok(())
}

criterion_group!(benches, bench_function_graph);
criterion_main!(benches);
