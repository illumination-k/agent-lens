use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent_lens::analyze::{OutputFormat, SimilarityAnalyzer};
use criterion::{Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

fn bench_similarity(c: &mut Criterion) {
    let small = bench_corpus(2, 16);
    let medium = bench_corpus(16, 16);
    let large = bench_corpus(32, 32);
    let analyzer = SimilarityAnalyzer::new();

    c.bench_function("similarity_directory_cartesian_32_functions", |b| {
        b.iter(|| {
            let report = match analyzer.analyze(small.path(), OutputFormat::Json) {
                Ok(report) => report,
                Err(err) => panic!("similarity benchmark failed: {err}"),
            };
            std::hint::black_box(report.len());
        });
    });

    c.bench_function("similarity_directory_lsh_256_functions", |b| {
        b.iter(|| {
            let report = match analyzer.analyze(medium.path(), OutputFormat::Json) {
                Ok(report) => report,
                Err(err) => panic!("similarity benchmark failed: {err}"),
            };
            std::hint::black_box(report.len());
        });
    });

    c.bench_function("similarity_directory_lsh_1024_functions", |b| {
        b.iter(|| {
            let report = match analyzer.analyze(large.path(), OutputFormat::Json) {
                Ok(report) => report,
                Err(err) => panic!("similarity benchmark failed: {err}"),
            };
            std::hint::black_box(report.len());
        });
    });
}

fn bench_corpus(file_count: usize, functions_per_file: usize) -> TempDir {
    let dir = tempfile::tempdir().unwrap_or_else(|err| {
        panic!("failed to create benchmark tempdir: {err}");
    });
    write_corpus(dir.path(), file_count, functions_per_file).unwrap_or_else(|err| {
        panic!("failed to write benchmark corpus: {err}");
    });
    dir
}

fn write_corpus(root: &Path, file_count: usize, functions_per_file: usize) -> std::io::Result<()> {
    for file_idx in 0..file_count {
        let path: PathBuf = root.join(format!("module_{file_idx:02}.rs"));
        let mut src = String::new();
        for fn_idx in 0..functions_per_file {
            let global = file_idx * functions_per_file + fn_idx;
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
