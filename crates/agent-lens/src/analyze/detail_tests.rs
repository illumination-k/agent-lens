//! `--detail` and overflow-line behaviour across analyzers.
//!
//! One corpus, wide enough that every analyzer here has more rows than
//! the concise cap, so the same three questions can be put to each: does
//! `concise` drop the standing note and cap the listing, does an explicit
//! `--top` still win, and does a truncated listing say what was cut and
//! which flag narrows it. Per-analyzer suites keep their own semantics;
//! this one holds what is supposed to be the same everywhere.

use std::fmt::Write as _;
use std::path::Path;

use rstest::rstest;

use super::format::CONCISE_TOP;
use super::{
    CoChangeAnalyzer, CohesionAnalyzer, ComplexityAnalyzer, CouplingAnalyzer, Detail, DetailScope,
    HotspotAnalyzer, HubsAnalyzer, LayersAnalyzer, OutputFormat, ParametersAnalyzer, RiskAnalyzer,
    SimilarityAnalyzer, SingleImplAnalyzer, SingleUseAnalyzer, TestOnlyAnalyzer,
    TestRedundancyAnalyzer,
};
use crate::test_support::{run_git, write_file};

/// Modules in the corpus; every listing below has at least this many
/// rows, which is above both the concise cap and the explicit `--top`
/// the tests ask for.
const MODULES: usize = 8;

/// An explicit `--top` between the concise cap and the row count.
const EXPLICIT_TOP: usize = 7;

/// One module: a two-island `impl` (cohesion), a single-implementor
/// trait, a nested-branch function whose depth grows with `k`
/// (complexity) calling a private helper nothing else calls
/// (single-use), a verbatim pair whose length grows with `k` so pairs
/// do not merge across modules (similarity), and a private function
/// only a pair of same-shaped tests reaches (test-only,
/// test-redundancy).
fn module_source(k: usize) -> String {
    let mut src = format!(
        "pub struct S{k} {{ a: i32, b: i32 }}\n\
         impl S{k} {{\n\
         \x20   pub fn ga(&self) -> i32 {{ self.a }}\n\
         \x20   pub fn gb(&self) -> i32 {{ self.b }}\n\
         }}\n\
         pub trait T{k} {{ fn run(&self) -> i32; }}\n\
         impl T{k} for S{k} {{ fn run(&self) -> i32 {{ self.a }} }}\n\
         pub fn branchy{k}(n: i32) -> i32 {{\n\
         \x20   let mut t = n;\n"
    );
    for i in 0..k + 2 {
        let _ = writeln!(
            src,
            "    if t > {i} {{ if t > {} {{ t += {i}; }} }}",
            i + 10
        );
    }
    let _ = write!(
        src,
        "    helper{k}(t)\n\
         }}\n\
         fn helper{k}(n: i32) -> i32 {{\n\
         \x20   let m = n * 2;\n\
         \x20   m + {k}\n\
         }}\n"
    );
    for side in ["a", "b"] {
        let _ = writeln!(src, "pub fn dup{k}_{side}(n: i32) -> i32 {{");
        src.push_str("    let mut t = n;\n");
        for i in 0..k + 3 {
            let _ = writeln!(src, "    t = t * 3 + {i};");
        }
        src.push_str("    while t > 100 { t -= 7; }\n    t\n}\n");
    }
    let _ = write!(
        src,
        "fn only_tested{k}(n: i32) -> i32 {{\n\
         \x20   let m = n + 1;\n\
         \x20   m\n\
         }}\n\
         #[cfg(test)]\n\
         mod tests {{\n"
    );
    for side in ["a", "b"] {
        let _ = write!(
            src,
            "    #[test]\n\
             \x20   fn reaches_{side}() {{\n\
             \x20       let first = super::only_tested{k}(1);\n\
             \x20       let second = super::only_tested{k}(2);\n\
             \x20       let third = super::only_tested{k}(3);\n\
             \x20       assert_eq!(first + 1, second);\n\
             \x20       assert_eq!(second + 1, third);\n\
             \x20   }}\n"
        );
    }
    src.push_str("}\n");
    src
}

/// The crate root: declares the modules, fans out to every one of them
/// (a god function for hubs, a pair per module for coupling), and
/// passes one value to a parameter at every call site (parameters).
fn lib_source() -> String {
    let mut src = String::new();
    for k in 0..MODULES {
        let _ = writeln!(src, "pub mod m{k};");
    }
    src.push_str(
        "fn scaled_a(n: i32, factor: i32) -> i32 { n * factor }\n\
         fn scaled_b(n: i32, factor: i32) -> i32 { n + factor }\n\
         pub fn all(n: i32) -> i32 {\n\
         \x20   let mut t = scaled_a(n, 2) + scaled_a(n + 1, 2) + scaled_a(n + 2, 2);\n\
         \x20   t += scaled_b(n, 5) + scaled_b(n + 1, 5) + scaled_b(n + 2, 5);\n",
    );
    for k in 0..MODULES {
        let _ = writeln!(src, "    t += m{k}::branchy{k}(n);");
    }
    src.push_str("    t\n}\n");
    src
}

/// The corpus in a git repository whose module files changed together
/// in every commit, so the history analyzers have a pair per two
/// modules and a churn count per file.
fn corpus() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_git(root, &["init", "-q", "-b", "main"]);
    write_file(root, "src/lib.rs", &lib_source());
    for revision in 0..4 {
        for k in 0..MODULES {
            let mut src = module_source(k);
            let _ = writeln!(src, "// revision {revision}");
            write_file(root, &format!("src/m{k}.rs"), &src);
        }
        run_git(root, &["add", "."]);
        run_git(
            root,
            &["commit", "-q", "-m", &format!("revision {revision}")],
        );
    }
    dir
}

/// An analyzer run against the corpus root with a `--top`.
type Run = fn(&Path, Option<usize>, OutputFormat) -> String;

fn complexity(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    ComplexityAnalyzer::new()
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

fn cohesion(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    CohesionAnalyzer::new()
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

/// At a threshold only the verbatim pairs clear (their names differ,
/// so they score just under 1), so each stays a cluster of its own
/// instead of chaining to its neighbours.
fn similarity(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    SimilarityAnalyzer::new()
        .with_threshold(0.95)
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

fn single_use(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    SingleUseAnalyzer::new()
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

fn coupling(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    CouplingAnalyzer::new()
        .with_top(top)
        .analyze(root.join("src/lib.rs"), format)
        .unwrap()
}

fn hotspot(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    HotspotAnalyzer::new()
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

fn co_change(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    CoChangeAnalyzer::new()
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

fn risk(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    RiskAnalyzer::new()
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

fn hubs(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    HubsAnalyzer::new()
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

fn layers(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    LayersAnalyzer::new()
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

fn single_impl(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    SingleImplAnalyzer::new()
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

fn test_only(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    TestOnlyAnalyzer::new()
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

fn test_redundancy(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    TestRedundancyAnalyzer::new()
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

fn parameters(root: &Path, top: Option<usize>, format: OutputFormat) -> String {
    ParametersAnalyzer::new()
        .with_top(top)
        .analyze(root, format)
        .unwrap()
}

/// Lines of `md` that are rows of the listing `is_row` recognises.
fn row_count(md: &str, is_row: fn(&str) -> bool) -> usize {
    md.lines().filter(|line| is_row(line)).count()
}

/// `concise` drops the standing note and caps the listing at
/// [`CONCISE_TOP`]; an explicit `--top` overrides the cap; and none of
/// it reaches JSON, which is the same bytes at either level.
#[rstest]
#[case::complexity(complexity, |l: &str| l.contains(":`branchy") || l.contains(":`dup"), None)]
#[case::cohesion(cohesion, |l: &str| l.starts_with("- src/"), None)]
#[case::similarity(similarity, |l: &str| l.starts_with("- ") && l.contains(", similarity "), None)]
#[case::single_use(
    single_use,
    |l: &str| l.starts_with("- `") && l.contains("::helper"),
    Some("Each row is a function with exactly one resolved production caller"),
)]
#[case::coupling(coupling, |l: &str| l.starts_with("| crate"), None)]
#[case::hotspot(hotspot, |l: &str| l.starts_with("| src/"), None)]
#[case::co_change(
    co_change,
    |l: &str| l.starts_with("| src/"),
    Some("Temporal coupling from git history"),
)]
fn concise_drops_the_note_and_caps_the_listing(
    #[case] run: Run,
    #[case] is_row: fn(&str) -> bool,
    #[case] note: Option<&str>,
) {
    let dir = corpus();
    let root = dir.path();

    let detailed = run(root, None, OutputFormat::Md);
    let everything = run(root, Some(10_000), OutputFormat::Md);
    assert!(
        row_count(&everything, is_row) > EXPLICIT_TOP,
        "the corpus must overflow both caps, got: {everything}",
    );
    let detailed_json = run(root, None, OutputFormat::Json);

    let _concise = DetailScope::enter(Detail::Concise);
    let concise = run(root, None, OutputFormat::Md);
    assert_eq!(row_count(&concise, is_row), CONCISE_TOP, "got: {concise}");
    assert!(
        concise.contains(&format!("not shown (--top {CONCISE_TOP})")),
        "the cap is stated, got: {concise}",
    );
    if let Some(note) = note {
        assert!(detailed.contains(note), "got: {detailed}");
        assert!(!concise.contains(note), "got: {concise}");
    }

    let explicit = run(root, Some(EXPLICIT_TOP), OutputFormat::Md);
    assert_eq!(
        row_count(&explicit, is_row),
        EXPLICIT_TOP,
        "got: {explicit}"
    );

    assert_eq!(run(root, None, OutputFormat::Json), detailed_json);
}

/// A truncated listing closes by naming what was cut and the flag that
/// narrows it; one that fits says nothing. `top` is the cap that
/// truncates: zero for the listings this corpus gives a single row.
#[rstest]
#[case::complexity(
    complexity,
    1,
    "more function(s) not shown (--top 1). Narrow it: `--min-score "
)]
#[case::complexity_rollups(complexity, 1, "more file(s) not shown (--top 1). Narrow it: pass")]
#[case::cohesion(
    cohesion,
    1,
    "more unit(s) not shown (--top 1). Narrow it: `--min-score 3`"
)]
#[case::similarity(
    similarity,
    1,
    "more cluster(s) not shown (--top 1). Narrow it: `--threshold 0.98`"
)]
#[case::single_use(
    single_use,
    1,
    "more candidate(s) not shown (--top 1). Narrow it: `--max-loc "
)]
#[case::coupling(
    coupling,
    1,
    "more lower-IFC module(s) not shown (--top 1). Narrow it: pass"
)]
#[case::coupling_pairs(coupling, 1, "more pair(s) not shown (--top 1). Narrow it: pass")]
#[case::hotspot(
    hotspot,
    1,
    "more file(s) not shown (--top 1). Narrow it: `--since 90.days.ago`"
)]
#[case::co_change(
    co_change,
    1,
    "more pair(s) not shown (--top 1). Narrow it: `--min-support 4`"
)]
#[case::risk(
    risk,
    1,
    "more file(s) not shown (--top 1). Narrow it: `--since 90.days.ago`"
)]
#[case::hubs(hubs, 0, "more function(s) not shown (--top 0). Narrow it: pass")]
#[case::layers(layers, 1, "more entry point(s) not shown (--top 1). Narrow it: pass")]
#[case::layers_modules(layers, 1, "more module(s) not shown (--top 1). Narrow it: pass")]
#[case::single_impl(
    single_impl,
    1,
    "more abstraction(s) not shown (--top 1). Narrow it: pass"
)]
#[case::test_only(test_only, 1, "more function(s) not shown (--top 1). Narrow it: pass")]
#[case::test_redundancy(
    test_redundancy,
    1,
    "more group(s) not shown (--top 1). Narrow it: `--threshold 0.97`"
)]
#[case::parameters(
    parameters,
    1,
    "more finding(s) not shown (--top 1). Narrow it: `--min-call-sites "
)]
fn a_truncated_listing_names_the_cut_and_the_flag_that_narrows_it(
    #[case] run: Run,
    #[case] top: usize,
    #[case] overflow: &str,
) {
    let dir = corpus();
    let root = dir.path();

    let capped = run(root, Some(top), OutputFormat::Md);
    assert!(capped.contains(overflow), "got: {capped}");

    let uncapped = run(root, Some(10_000), OutputFormat::Md);
    assert!(!uncapped.contains("not shown"), "got: {uncapped}");
}

/// A similarity run with no `--top` lists every cluster at `detailed`,
/// so it has no overflow line to write: the cap is concise's alone.
#[test]
fn similarity_without_top_is_uncapped_at_detailed() {
    let dir = corpus();
    let md = similarity(dir.path(), None, OutputFormat::Md);
    assert!(!md.contains("not shown"), "got: {md}");
    assert!(!md.contains("## Top "), "got: {md}");
}
