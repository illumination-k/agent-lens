//! Benchmarks for `analyze similarity` across its strategies (cartesian,
//! LSH), methods (TSED, token, PDG), targets (functions, types, blocks)
//! and language adapters.
//!
//! The Go idiom corpus is the shape behind issue #562: blocks sharing one
//! skeleton with disjoint values, where a value-blind candidate stage
//! turned scoring quadratic. It runs at two sizes so a regression back to
//! that path shows up as the larger one outgrowing the smaller.

mod support;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent_lens::analyze::{OutputFormat, SimilarityAnalyzer, SimilarityMethod, SimilarityTarget};
use criterion::{Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

fn bench_similarity(c: &mut Criterion) {
    let small = support::corpus(|root| write_dense_corpus(root, 2, 16));
    let medium = support::corpus(|root| write_dense_corpus(root, 16, 16));
    let large_dense = support::corpus(|root| write_dense_corpus(root, 32, 32));
    let large_sparse = support::corpus(|root| write_sparse_corpus(root, 32, 32));
    let types = support::corpus(|root| write_types_corpus(root, 32, 16));
    let go_idiom_small = support::corpus(|root| support::write_go_idiom_corpus(root, 8));
    let go_idiom_large = support::corpus(|root| support::write_go_idiom_corpus(root, 16));
    let go = support::corpus(|root| support::write_go_module_corpus(root, 32, 32));
    let ts = support::corpus(|root| support::write_ts_corpus(root, 32, 32));
    let py = support::corpus(|root| support::write_py_corpus(root, 32, 32));
    let analyzer = SimilarityAnalyzer::new();
    let token_analyzer = SimilarityAnalyzer::new().with_method(SimilarityMethod::Token);
    let pdg_analyzer = SimilarityAnalyzer::new().with_method(SimilarityMethod::Pdg);
    let types_analyzer = SimilarityAnalyzer::new().with_target(SimilarityTarget::Types);
    let blocks_analyzer = SimilarityAnalyzer::new().with_target(SimilarityTarget::Blocks);

    let cases: [(&str, &SimilarityAnalyzer, &TempDir); 13] = [
        (
            "similarity_directory_cartesian_32_functions",
            &analyzer,
            &small,
        ),
        ("similarity_directory_lsh_256_functions", &analyzer, &medium),
        (
            "similarity_directory_lsh_dense_1024_functions",
            &analyzer,
            &large_dense,
        ),
        (
            "similarity_directory_lsh_sparse_1024_functions",
            &analyzer,
            &large_sparse,
        ),
        // Types always take the cartesian path (LSH is gated off for small
        // trees), so 512 units ≈ 130k pairs is the shape a real repo hits.
        ("similarity_directory_types_512", &types_analyzer, &types),
        // Blocks multiply the unit count: the dense corpus's 256 functions
        // mint several thousand statement windows, which is the shape that
        // decides whether the target is usable on a real repo at all.
        (
            "similarity_directory_blocks_256_functions",
            &blocks_analyzer,
            &medium,
        ),
        (
            "similarity_blocks_go_idiom_64_functions",
            &blocks_analyzer,
            &go_idiom_small,
        ),
        (
            "similarity_blocks_go_idiom_128_functions",
            &blocks_analyzer,
            &go_idiom_large,
        ),
        (
            "similarity_token_directory_lsh_dense_1024_functions",
            &token_analyzer,
            &large_dense,
        ),
        (
            "similarity_pdg_directory_lsh_dense_1024_functions",
            &pdg_analyzer,
            &large_dense,
        ),
        ("similarity_go_lsh_1024_functions", &analyzer, &go),
        ("similarity_ts_lsh_1024_functions", &analyzer, &ts),
        ("similarity_py_lsh_1024_functions", &analyzer, &py),
    ];
    for (id, analyzer, dir) in cases {
        c.bench_function(id, |b| {
            b.iter(|| support::consume(analyzer.analyze(dir.path(), OutputFormat::Json)));
        });
    }
}

fn write_types_corpus(
    root: &Path,
    file_count: usize,
    types_per_file: usize,
) -> std::io::Result<()> {
    const FIELD_TYPES: [&str; 6] = ["u64", "String", "Vec<String>", "bool", "Option<i64>", "f64"];
    for file_idx in 0..file_count {
        let path: PathBuf = root.join(format!("types_{file_idx:02}.rs"));
        let mut src = String::new();
        for type_idx in 0..types_per_file {
            let global = file_idx * types_per_file + type_idx;
            let field_count = 3 + global % 5;
            let _ = writeln!(src, "pub struct Generated{global:03} {{");
            for field_idx in 0..field_count {
                let ty = FIELD_TYPES[(global + field_idx) % FIELD_TYPES.len()];
                let _ = writeln!(src, "    pub field_{field_idx}: {ty},");
            }
            let _ = writeln!(src, "}}\n");
        }
        std::fs::write(path, src)?;
    }
    Ok(())
}

fn write_dense_corpus(
    root: &Path,
    file_count: usize,
    functions_per_file: usize,
) -> std::io::Result<()> {
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

fn write_sparse_corpus(
    root: &Path,
    file_count: usize,
    functions_per_file: usize,
) -> std::io::Result<()> {
    for file_idx in 0..file_count {
        let path: PathBuf = root.join(format!("module_{file_idx:02}.rs"));
        let mut src = String::new();
        for fn_idx in 0..functions_per_file {
            let global = file_idx * functions_per_file + fn_idx;
            write_sparse_function(&mut src, global);
        }
        std::fs::write(path, src)?;
    }
    Ok(())
}

fn write_sparse_function(src: &mut String, global: usize) {
    let salt = global % 29;
    match global % 12 {
        0 => write_checked_sum(src, global, salt),
        1 => write_match_classifier(src, global, salt),
        2 => write_iterator_fold(src, global, salt),
        3 => write_while_state_machine(src, global, salt),
        4 => write_result_style(src, global, salt),
        5 => write_tuple_accumulator(src, global, salt),
        6 => write_nested_loop(src, global, salt),
        7 => write_option_pipeline(src, global, salt),
        8 => write_bitwise_counter(src, global, salt),
        9 => write_slice_scan(src, global, salt),
        10 => write_guarded_recursion_shape(src, global, salt),
        _ => write_branch_table(src, global, salt),
    }
}

fn write_checked_sum(src: &mut String, global: usize, salt: usize) {
    let _ = writeln!(
        src,
        r#"
pub fn sparse_{global:03}(input: i64) -> i64 {{
    let mut total = input;
    for value in [1_i64, 3, 5, 7, {salt}].iter() {{
        total = total.checked_add(*value).unwrap_or(total - value);
    }}
    total
}}
"#,
    );
}

fn write_match_classifier(src: &mut String, global: usize, salt: usize) {
    let _ = writeln!(
        src,
        r#"
pub fn sparse_{global:03}(input: i64) -> i64 {{
    match input.rem_euclid(5) {{
        0 => input + {salt},
        1 | 2 => input * 2 - {salt},
        3 if input > 20 => input / 3,
        _ => input - 7,
    }}
}}
"#,
    );
}

fn write_iterator_fold(src: &mut String, global: usize, salt: usize) {
    let _ = writeln!(
        src,
        r#"
pub fn sparse_{global:03}(input: i64) -> i64 {{
    [input, {salt}, input / 2, input * 3]
        .into_iter()
        .filter(|value| value % 2 == 0)
        .map(|value| value.abs())
        .fold(0, |acc, value| acc + value)
}}
"#,
    );
}

fn write_while_state_machine(src: &mut String, global: usize, salt: usize) {
    let _ = writeln!(
        src,
        r#"
pub fn sparse_{global:03}(input: i64) -> i64 {{
    let mut state = input;
    let mut step = 0;
    while step < {limit} {{
        state = match state & 3 {{
            0 => state + step,
            1 => state - step,
            _ => state ^ step,
        }};
        step += 1;
    }}
    state
}}
"#,
        limit = 3 + salt % 6,
    );
}

fn write_result_style(src: &mut String, global: usize, salt: usize) {
    let _ = writeln!(
        src,
        r#"
pub fn sparse_{global:03}(input: i64) -> i64 {{
    let parsed = input
        .checked_mul({factor})
        .and_then(|value| value.checked_sub({salt}));
    if let Some(value) = parsed {{
        value
    }} else {{
        input
    }}
}}
"#,
        factor = 2 + salt % 5,
    );
}

fn write_tuple_accumulator(src: &mut String, global: usize, salt: usize) {
    let _ = writeln!(
        src,
        r#"
pub fn sparse_{global:03}(input: i64) -> i64 {{
    let (mut left, mut right) = (input, {salt});
    for idx in 0..4 {{
        let next = left + right + idx;
        left = right - idx;
        right = next;
    }}
    left + right
}}
"#,
    );
}

fn write_nested_loop(src: &mut String, global: usize, salt: usize) {
    let _ = writeln!(
        src,
        r#"
pub fn sparse_{global:03}(input: i64) -> i64 {{
    let mut total = 0;
    for outer in 0..{outer} {{
        for inner in 0..outer {{
            total += input + outer * inner;
        }}
    }}
    total
}}
"#,
        outer = 2 + salt % 5,
    );
}

fn write_option_pipeline(src: &mut String, global: usize, salt: usize) {
    let _ = writeln!(
        src,
        r#"
pub fn sparse_{global:03}(input: i64) -> i64 {{
    Some(input)
        .filter(|value| *value > {salt})
        .map(|value| value - {salt})
        .unwrap_or_else(|| input + {salt})
}}
"#,
    );
}

fn write_bitwise_counter(src: &mut String, global: usize, salt: usize) {
    let _ = writeln!(
        src,
        r#"
pub fn sparse_{global:03}(input: i64) -> i64 {{
    let mut mask = input as u64;
    let mut count = 0_i64;
    while mask != 0 {{
        count += (mask & 1) as i64;
        mask >>= 1;
    }}
    count + {salt}
}}
"#,
    );
}

fn write_slice_scan(src: &mut String, global: usize, salt: usize) {
    let _ = writeln!(
        src,
        r#"
pub fn sparse_{global:03}(input: i64) -> i64 {{
    let items = [input, input + 1, {salt}, input - 3];
    let mut best = items[0];
    for item in items {{
        if item > best {{
            best = item;
        }}
    }}
    best
}}
"#,
    );
}

fn write_guarded_recursion_shape(src: &mut String, global: usize, salt: usize) {
    let _ = writeln!(
        src,
        r#"
pub fn sparse_{global:03}(input: i64) -> i64 {{
    fn helper(value: i64, depth: i64) -> i64 {{
        if depth == 0 {{
            value
        }} else {{
            helper(value + depth, depth - 1)
        }}
    }}
    helper(input, {depth})
}}
"#,
        depth = 2 + salt % 4,
    );
}

fn write_branch_table(src: &mut String, global: usize, salt: usize) {
    let _ = writeln!(
        src,
        r#"
pub fn sparse_{global:03}(input: i64) -> i64 {{
    let table = [input + 1, input - 1, input * 2, input / 2];
    let index = input.rem_euclid(table.len() as i64) as usize;
    match table.get(index) {{
        Some(value) if *value > {salt} => *value,
        Some(value) => *value + {salt},
        None => input,
    }}
}}
"#,
    );
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_similarity
}
criterion_main!(benches);
