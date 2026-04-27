use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent_lens::analyze::{OutputFormat, SimilarityAnalyzer};
use criterion::{Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

fn bench_similarity(c: &mut Criterion) {
    let corpus = bench_corpus();
    let analyzer = SimilarityAnalyzer::new();
    c.bench_function("similarity_directory_lsh_256_functions", |b| {
        b.iter(|| {
            let report = match analyzer.analyze(corpus.path(), OutputFormat::Json) {
                Ok(report) => report,
                Err(err) => panic!("similarity benchmark failed: {err}"),
            };
            std::hint::black_box(report.len());
        });
    });
}

fn bench_corpus() -> TempDir {
    let dir = tempfile::tempdir().unwrap_or_else(|err| {
        panic!("failed to create benchmark tempdir: {err}");
    });
    write_corpus(dir.path()).unwrap_or_else(|err| {
        panic!("failed to write benchmark corpus: {err}");
    });
    dir
}

fn write_corpus(root: &Path) -> std::io::Result<()> {
    for file_idx in 0..16 {
        let path: PathBuf = root.join(format!("module_{file_idx:02}.rs"));
        let mut src = String::new();
        for fn_idx in 0..16 {
            let global = file_idx * 16 + fn_idx;
            let variant = global % 8;
            let salt = global % 17;
            let _ = writeln!(
                src,
                r#"
pub fn generated_{global:03}(input: i64) -> i64 {{
    let mut acc = input + {salt};
    for step in 0..{loop_bound} {{
        if (acc + step) % {modulus} == 0 {{
            acc += step * {then_factor};
        }} else {{
            acc -= step + {else_bias};
        }}
    }}
    if acc > {limit} {{
        acc / {divisor}
    }} else {{
        acc + {tail}
    }}
}}
"#,
                loop_bound = 3 + variant,
                modulus = 2 + variant,
                then_factor = 1 + variant,
                else_bias = salt,
                limit = 40 + global,
                divisor = 2 + (variant % 3),
                tail = 5 + variant,
            );
        }
        std::fs::write(path, src)?;
    }
    Ok(())
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_similarity
}
criterion_main!(benches);
