//! `analyze untested` — production functions with no static call path
//! from any test function.
//!
//! Multi-source breadth-first traversal from every `is_test` node of the
//! shared call graph, forward over **resolved** edges: whatever the walk
//! reaches is test-exercised in the static sense, and the complement over
//! production nodes is this report. It is a structural complement to
//! coverage — no execution, no instrumentation, same answer in every
//! supported language — telling an agent which code it cannot rely on
//! tests to guard while editing.
//!
//! What it measures is **"no resolved call path from a test function"**,
//! not "uncovered". The two differ in both directions and the report says
//! so:
//!
//! - Integration tests that drive the built binary (or any out-of-process
//!   entry point) reach functions that have no in-graph test caller, so
//!   those functions are listed here while being covered in practice.
//! - Only resolved edges are traversable. An unresolved or ambiguous call
//!   site inside test-reached code may hide a real path, which makes the
//!   listing an **upper bound**. Untested functions named as a candidate
//!   of an ambiguous edge leaving test-reached code are flagged
//!   individually, and the outbound unresolved/ambiguous call-site counts
//!   of the reached set are reported as the global bound.
//!
//! Test roots are the graph's own notion of test code — `#[test]` /
//! `#[cfg(test)]` items and their module-local helpers for Rust, the
//! per-adapter naming rules elsewhere, plus everything under a test-like
//! path. Rust `#[cfg(test)]` call sites must be in the graph for the
//! traversal to see anything, which is the default; `--exclude-tests`
//! removes every root and is reported as such rather than silently
//! listing the whole codebase.
//!
//! Findings are grouped by module and ranked by untested LOC: the biggest
//! unguarded body is the one worth a test first.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use super::call_graph::algo::bfs;
use super::call_graph::model::{ModuleResolutionSummary, NodeVisibility, Resolution};
use super::call_graph::{CallGraph, CallGraphBuilder};
use super::format::render_module_confidence;
use super::{AnalyzerError, OutputFormat};

const SCHEMA_VERSION: u32 = 1;

/// Module sections rendered in markdown when `--top` is not given. JSON
/// always carries every module.
const DEFAULT_TOP: usize = 20;

/// Functions listed per module section in markdown. JSON carries all of
/// them.
const FUNCTIONS_PER_MODULE: usize = 10;

/// What the verdict means, stated in the output itself because the gap
/// between "no static call path from a test" and "uncovered" is exactly
/// where an agent would otherwise over-read the result.
const NOTE: &str = "Structural, not coverage: this is \"no resolved call path from a test \
     function\", measured on the static call graph. Integration tests that drive the built \
     binary reach functions with no in-graph test caller, so those are listed here despite \
     being exercised. Only resolved edges are traversable, so an unresolved or ambiguous call \
     site in test-reached code can hide a real path — the listing is an upper bound, and \
     functions an ambiguous edge might reach are flagged per row.";

/// Analyzer entry point for `analyze untested`.
#[derive(Debug, Default, Clone)]
pub struct UntestedAnalyzer {
    builder: CallGraphBuilder,
    top: Option<usize>,
}

impl UntestedAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.builder = self.builder.with_only_tests(only_tests);
        self
    }

    /// Accepted for CLI uniformity, but it removes the traversal's own
    /// starting points: with no test functions in the graph every
    /// production function is untested by construction, and the report
    /// says so instead of presenting the whole codebase as a finding.
    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.builder = self.builder.with_exclude_tests(exclude_tests);
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.builder = self.builder.with_exclude_patterns(exclude);
        self
    }

    /// Cap the markdown module sections to the top-N entries. JSON output
    /// always carries every row.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let graph = self.builder.build(path)?;
        let report = Report::build(path, &graph);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).map_err(AnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&report, self.top)),
        }
    }
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    /// What every verdict on this report is relative to.
    note: &'static str,
    test_roots: TestRoots,
    /// Modules holding at least one untested function, largest untested
    /// body first.
    modules: Vec<ModuleGroup>,
    bounds: Bounds,
    /// Per-module call-site resolution counts — the calibration layer: a
    /// module whose call sites mostly failed to resolve contributes
    /// functions that may be test-reached invisibly.
    resolution: Vec<ModuleResolutionSummary>,
    summary: Summary,
}

/// The traversal's starting set. Every "untested" verdict is relative to
/// it, so it is emitted rather than assumed.
#[derive(Debug, Serialize)]
struct TestRoots {
    /// Functions the graph flagged as test code.
    function_count: usize,
    file_count: usize,
    module_count: usize,
    /// No test function reached the graph at all: either the tree holds
    /// none, or `--exclude-tests` dropped them. Every production function
    /// below is then untested by construction, not by evidence.
    absent: bool,
}

/// One module's untested functions.
#[derive(Debug, Serialize)]
struct ModuleGroup {
    module: String,
    function_count: usize,
    /// Total source lines of the untested functions — the ranking key.
    loc: usize,
    /// Of `function_count`, how many carry an ambiguous inbound edge from
    /// test-reached code and so may be reached invisibly.
    possibly_test_reached_count: usize,
    functions: Vec<UntestedFunction>,
}

#[derive(Debug, Serialize)]
struct UntestedFunction {
    id: String,
    qualified_name: String,
    file: String,
    start_line: usize,
    end_line: usize,
    loc: usize,
    visibility: NodeVisibility,
    /// Distinct resolved callers, all of them outside the test-reached
    /// set by construction. Zero means nothing in the analyzed tree calls
    /// it either — a dead-code question rather than a testing one.
    fan_in: usize,
    cyclomatic_complexity: Option<u32>,
    /// Ambiguous call sites leaving test-reached code whose candidate set
    /// names this function: the resolver could not decide, so a real test
    /// path may exist. Non-zero means "possibly reached", never "reached".
    ambiguous_inbound_from_reached: usize,
}

/// The two directions in which this listing is wrong, quantified.
#[derive(Debug, Serialize)]
struct Bounds {
    /// Call sites in test-reached code whose callee did not resolve to
    /// any function. Each could reach further, so the listing is an upper
    /// bound.
    unresolved_call_count_in_reached: usize,
    /// Call sites in test-reached code whose callee resolved to several
    /// candidates and was therefore not traversed.
    ambiguous_call_count_in_reached: usize,
    /// Untested functions named by at least one of those ambiguous call
    /// sites.
    possibly_test_reached_function_count: usize,
    /// Call sites the graph could not attribute to an enclosing function
    /// (top-level and module-initialisation code). They are invisible to
    /// the traversal in both directions.
    caller_unattributed_call_count: usize,
}

#[derive(Debug, Serialize)]
struct Summary {
    /// Non-test functions in scope — the denominator.
    prod_function_count: usize,
    /// Of those, how many the traversal reached.
    test_reached_function_count: usize,
    untested_function_count: usize,
    /// Source lines held by the untested functions.
    untested_loc: usize,
    /// `untested_function_count / prod_function_count`, 0.0 on an empty
    /// corpus.
    untested_share: f64,
    /// Modules holding at least one untested function.
    module_count: usize,
}

impl Report {
    fn build(root: &Path, graph: &CallGraph) -> Self {
        let adjacency = graph.resolved_adjacency();
        let roots: Vec<usize> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.is_test)
            .map(|(idx, _)| idx)
            .collect();

        let mut reached = vec![false; graph.nodes.len()];
        for visit in bfs(&adjacency, &roots) {
            reached[visit.node] = true;
        }
        let untested: Vec<usize> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|&(idx, node)| !node.is_test && !reached[idx])
            .map(|(idx, _)| idx)
            .collect();

        let edges = EdgeScan::run(graph, &reached, &untested);
        let modules = module_groups(graph, &untested, &edges.ambiguous_inbound);

        let prod_function_count = graph.nodes.iter().filter(|node| !node.is_test).count();
        let test_reached_function_count = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|&(idx, node)| !node.is_test && reached[idx])
            .count();
        let summary = Summary {
            prod_function_count,
            test_reached_function_count,
            untested_function_count: untested.len(),
            untested_loc: modules.iter().map(|m| m.loc).sum(),
            untested_share: if prod_function_count == 0 {
                0.0
            } else {
                untested.len() as f64 / prod_function_count as f64
            },
            module_count: modules.len(),
        };

        Self {
            schema_version: SCHEMA_VERSION,
            root: root.display().to_string(),
            language: graph.language,
            note: NOTE,
            test_roots: test_roots(graph, &roots),
            bounds: Bounds {
                unresolved_call_count_in_reached: edges.unresolved_in_reached,
                ambiguous_call_count_in_reached: edges.ambiguous_in_reached,
                possibly_test_reached_function_count: modules
                    .iter()
                    .map(|m| m.possibly_test_reached_count)
                    .sum(),
                caller_unattributed_call_count: edges.caller_unattributed,
            },
            modules,
            resolution: graph.module_summary.clone(),
            summary,
        }
    }
}

fn test_roots(graph: &CallGraph, roots: &[usize]) -> TestRoots {
    let mut files: BTreeSet<&str> = BTreeSet::new();
    let mut modules: BTreeSet<&str> = BTreeSet::new();
    for &idx in roots {
        files.insert(graph.nodes[idx].file.as_str());
        modules.insert(graph.nodes[idx].module.as_str());
    }
    TestRoots {
        function_count: roots.len(),
        file_count: files.len(),
        module_count: modules.len(),
        absent: roots.is_empty(),
    }
}

/// One pass over the edge list collecting everything that depends on the
/// reached set: the bound counters, and which untested functions an
/// ambiguous edge leaving reached code could have targeted.
struct EdgeScan {
    unresolved_in_reached: usize,
    ambiguous_in_reached: usize,
    caller_unattributed: usize,
    /// Ambiguous inbound call sites per untested node index.
    ambiguous_inbound: BTreeMap<usize, usize>,
}

impl EdgeScan {
    fn run(graph: &CallGraph, reached: &[bool], untested: &[usize]) -> Self {
        let index_by_id = graph.node_index_by_id();
        let untested_set: BTreeSet<usize> = untested.iter().copied().collect();
        let mut scan = Self {
            unresolved_in_reached: 0,
            ambiguous_in_reached: 0,
            caller_unattributed: 0,
            ambiguous_inbound: BTreeMap::new(),
        };

        for edge in &graph.edges {
            let Some(from) = edge.from.as_deref() else {
                // No enclosing function, so the site is unreachable by
                // the traversal from either end. An anonymous callee is
                // not a named function to begin with, so it is not a
                // missed path — only the rest is worth reporting.
                if edge.resolution != Resolution::Anonymous {
                    scan.caller_unattributed += edge.call_count;
                }
                continue;
            };
            let Some(&from_idx) = index_by_id.get(from) else {
                continue;
            };
            if !reached[from_idx] {
                continue;
            }
            match edge.resolution {
                Resolution::Unresolved => scan.unresolved_in_reached += edge.call_count,
                Resolution::Ambiguous => {
                    scan.ambiguous_in_reached += edge.call_count;
                    for candidate in &edge.candidates {
                        let Some(&candidate_idx) = index_by_id.get(candidate.as_str()) else {
                            continue;
                        };
                        if untested_set.contains(&candidate_idx) {
                            *scan.ambiguous_inbound.entry(candidate_idx).or_default() +=
                                edge.call_count;
                        }
                    }
                }
                Resolution::Resolved | Resolution::Anonymous => {}
            }
        }
        scan
    }
}

/// Group the untested nodes by module, biggest untested body first. Rows
/// inside a module follow the same rule, so the first line of the first
/// section is the largest unguarded function in the corpus.
fn module_groups(
    graph: &CallGraph,
    untested: &[usize],
    ambiguous_inbound: &BTreeMap<usize, usize>,
) -> Vec<ModuleGroup> {
    let mut by_module: BTreeMap<&str, Vec<UntestedFunction>> = BTreeMap::new();
    for &idx in untested {
        let node = &graph.nodes[idx];
        by_module
            .entry(node.module.as_str())
            .or_default()
            .push(UntestedFunction {
                id: node.id.clone(),
                qualified_name: node.qualified_name.clone(),
                file: node.file.clone(),
                start_line: node.start_line,
                end_line: node.end_line,
                loc: node.weights.loc,
                visibility: node.visibility,
                fan_in: node.weights.fan_in,
                cyclomatic_complexity: node.weights.cyclomatic_complexity,
                ambiguous_inbound_from_reached: ambiguous_inbound
                    .get(&idx)
                    .copied()
                    .unwrap_or_default(),
            });
    }

    let mut groups: Vec<ModuleGroup> = by_module
        .into_iter()
        .map(|(module, mut functions)| {
            functions.sort_by(|a, b| (Reverse(a.loc), &a.id).cmp(&(Reverse(b.loc), &b.id)));
            ModuleGroup {
                module: module.to_owned(),
                function_count: functions.len(),
                loc: functions.iter().map(|f| f.loc).sum(),
                possibly_test_reached_count: functions
                    .iter()
                    .filter(|f| f.ambiguous_inbound_from_reached > 0)
                    .count(),
                functions,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        (Reverse(a.loc), Reverse(a.function_count), &a.module).cmp(&(
            Reverse(b.loc),
            Reverse(b.function_count),
            &b.module,
        ))
    });
    groups
}

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    let summary = &report.summary;
    let mut out = format!(
        "# Untested functions: {} ({}/{} production function(s), {} LOC, across {} module(s))\n",
        report.root,
        summary.untested_function_count,
        summary.prod_function_count,
        summary.untested_loc,
        summary.module_count,
    );
    let _ = writeln!(out, "\n{}", report.note);

    if report.test_roots.absent {
        out.push_str(
            "\n**No test function reached the graph**, so every production function below is \
             untested by construction rather than by evidence. `--exclude-tests` removes the \
             traversal's own starting points; drop it, or point the analyzer at a tree that \
             contains the tests.\n",
        );
    } else {
        let _ = writeln!(
            out,
            "\nTest roots: {} function(s) in {} file(s), {} module(s); they reach {} of {} \
             production function(s).",
            report.test_roots.function_count,
            report.test_roots.file_count,
            report.test_roots.module_count,
            summary.test_reached_function_count,
            summary.prod_function_count,
        );
    }

    if summary.prod_function_count == 0 {
        out.push_str("\n_No production functions in scope._\n");
        return out;
    }
    if report.modules.is_empty() {
        out.push_str("\n_Every production function has a resolved call path from a test._\n");
        return out;
    }

    render_bounds(&mut out, &report.bounds);
    render_modules(&mut out, &report.modules, limit);
    render_module_confidence(
        &mut out,
        &report.resolution,
        "Call sites in these modules resolved worst, so a test path through them is the most \
         likely to have been missed — their functions are the least certain rows above.",
    );
    out
}

fn render_bounds(out: &mut String, bounds: &Bounds) {
    let _ = writeln!(
        out,
        "\nUpper-bound support: {} unresolved and {} ambiguous call site(s) leave test-reached \
         code, {} of the functions below are named by one of those ambiguous sites, and {} call \
         site(s) had no enclosing function to attribute them to.",
        bounds.unresolved_call_count_in_reached,
        bounds.ambiguous_call_count_in_reached,
        bounds.possibly_test_reached_function_count,
        bounds.caller_unattributed_call_count,
    );
}

fn render_modules(out: &mut String, modules: &[ModuleGroup], limit: usize) {
    let shown = modules.len().min(limit);
    let _ = writeln!(
        out,
        "\n## Untested by module (largest body first; {shown} of {} module(s))",
        modules.len(),
    );
    for group in modules.iter().take(limit) {
        let _ = writeln!(
            out,
            "\n### `{}` — {} function(s), {} LOC",
            group.module, group.function_count, group.loc,
        );
        for f in group.functions.iter().take(FUNCTIONS_PER_MODULE) {
            let _ = writeln!(out, "- {}", render_function(f));
        }
        let overflow = group.function_count.saturating_sub(FUNCTIONS_PER_MODULE);
        if overflow > 0 {
            let _ = writeln!(out, "- +{overflow} more (JSON output carries every row)");
        }
    }
    let module_overflow = modules.len() - shown;
    if module_overflow > 0 {
        let _ = writeln!(
            out,
            "\n+{module_overflow} more module(s) not shown (raise `--top`; JSON carries every \
             row)."
        );
    }
}

fn render_function(f: &UntestedFunction) -> String {
    let mut row = format!(
        "`{}` ({}:{}, {} LOC, fan-in {}",
        f.qualified_name, f.file, f.start_line, f.loc, f.fan_in,
    );
    if let Some(cyclomatic) = f.cyclomatic_complexity {
        let _ = write!(row, ", cyclomatic {cyclomatic}");
    }
    row.push(')');
    if f.ambiguous_inbound_from_reached > 0 {
        let _ = write!(
            row,
            " — may be test-reached: {} ambiguous call site(s) name it",
            f.ambiguous_inbound_from_reached,
        );
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use rstest::rstest;
    use serde_json::Value;

    fn analyze_json(path: &Path) -> Value {
        let json = UntestedAnalyzer::new()
            .analyze(path, OutputFormat::Json)
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn analyze_md(path: &Path) -> String {
        UntestedAnalyzer::new()
            .analyze(path, OutputFormat::Md)
            .unwrap()
    }

    /// The qualified names reported as untested, in report order.
    fn untested_names(report: &Value) -> Vec<String> {
        report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["functions"].as_array().unwrap())
            .map(|f| f["qualified_name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// A test calls `covered`, which calls `covered_indirect`; `orphan`
    /// is called only by production code and `lonely` by nothing.
    const RUST_SOURCE: &str = "pub fn covered() { covered_indirect(); }\n\
         fn covered_indirect() {}\n\
         pub fn orphan() { lonely_callee(); }\n\
         fn lonely_callee() {}\n\
         #[cfg(test)]\n\
         mod tests {\n\
         use super::*;\n\
         fn helper() { covered(); }\n\
         #[test]\n\
         fn t() { helper(); }\n\
         }\n";

    #[test]
    fn reports_only_functions_with_no_call_path_from_a_test() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", RUST_SOURCE);

        let report = analyze_json(dir.path());
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["language"], "rust");
        // Transitive reach counts: `covered_indirect` is two hops from
        // the test and must not be listed.
        assert_eq!(
            untested_names(&report),
            ["crate::lonely_callee", "crate::orphan"],
        );
        assert_eq!(report["summary"]["prod_function_count"], 4);
        assert_eq!(report["summary"]["test_reached_function_count"], 2);
        assert_eq!(report["summary"]["untested_function_count"], 2);
        assert_eq!(report["summary"]["module_count"], 1);
        assert_eq!(report["test_roots"]["absent"], false);
        // `#[cfg(test)]` helpers count as roots alongside `#[test]` fns.
        assert_eq!(report["test_roots"]["function_count"], 2);
    }

    #[test]
    fn ranks_modules_and_functions_by_untested_loc() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/small.rs",
            "pub fn tiny() {}\npub fn tiny_two() {}\n",
        );
        write_file(
            dir.path(),
            "src/big.rs",
            "pub fn medium() {\nlet a = 1;\nlet b = 2;\nlet _ = a + b;\n}\n\
             pub fn large() {\nlet a = 1;\nlet b = 2;\nlet c = 3;\nlet d = 4;\nlet _ = a + b + c + d;\n}\n",
        );

        let report = analyze_json(dir.path());
        let modules: Vec<&str> = report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["module"].as_str().unwrap())
            .collect();
        assert_eq!(modules, ["crate::big", "crate::small"]);
        assert_eq!(
            untested_names(&report),
            [
                "crate::big::large",
                "crate::big::medium",
                "crate::small::tiny",
                "crate::small::tiny_two",
            ],
        );
        // 5 lines of `medium` plus 7 of `large`, the module's ranking key.
        assert_eq!(report["modules"][0]["loc"], 12);
    }

    #[test]
    fn an_ambiguous_call_from_a_test_flags_its_candidates_as_possibly_reached() {
        let dir = tempfile::tempdir().unwrap();
        // Two same-named methods on different owners: a bare `target()`
        // call from the test cannot pick one, so both stay untested but
        // are flagged rather than asserted unreachable.
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub struct A;\npub struct B;\n\
             impl A { pub fn target(&self) -> usize { 1 } }\n\
             impl B { pub fn target(&self) -> usize { 2 } }\n\
             #[cfg(test)]\n\
             mod tests {\n\
             #[test]\n\
             fn t() { target(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let flagged: Vec<u64> = report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["functions"].as_array().unwrap())
            .map(|f| f["ambiguous_inbound_from_reached"].as_u64().unwrap())
            .collect();
        assert_eq!(flagged, [1, 1], "both candidates are flagged");
        assert_eq!(
            report["bounds"]["possibly_test_reached_function_count"], 2,
            "report: {report}",
        );
        assert_eq!(report["bounds"]["ambiguous_call_count_in_reached"], 1);

        let md = analyze_md(dir.path());
        assert!(md.contains("may be test-reached"), "got: {md}");
    }

    #[test]
    fn unresolved_calls_from_test_reached_code_are_counted_as_the_upper_bound() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn covered() { external_thing(); }\n\
             pub fn never_called() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
             use super::*;\n\
             #[test]\n\
             fn t() { covered(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["bounds"]["unresolved_call_count_in_reached"], 1);
        assert_eq!(untested_names(&report), ["crate::never_called"]);
    }

    /// Every adapter's own notion of a test function has to seed the
    /// traversal, so each case is one language's canonical shape: a
    /// production function the test calls, one it does not, and the
    /// language's idiomatic test declaration.
    #[rstest]
    #[case::rust(
        &[(
            "src/lib.rs",
            "pub fn covered() -> usize { 1 }\n\
             pub fn untested_one() -> usize { 2 }\n\
             #[cfg(test)]\n\
             mod tests {\n\
             use super::*;\n\
             #[test]\n\
             fn t() { covered(); }\n\
             }\n",
        )],
        "crate::untested_one"
    )]
    #[case::python(
        &[(
            "src/lib.py",
            "def covered():\n    return 1\n\n\
             def untested_one():\n    return 2\n\n\
             def test_drives():\n    covered()\n",
        )],
        "src::lib::untested_one"
    )]
    #[case::go(
        &[
            (
                "src/lib.go",
                "package lib\n\nfunc Covered() int { return 1 }\n\nfunc UntestedOne() int { return 2 }\n",
            ),
            (
                "src/lib_test.go",
                "package lib\n\nimport \"testing\"\n\nfunc TestDrives(t *testing.T) { Covered() }\n",
            ),
        ],
        "src::UntestedOne"
    )]
    #[case::typescript(
        &[(
            "src/lib.ts",
            "export function covered(): number { return 1; }\n\
             export function untestedOne(): number { return 2; }\n\
             export function test_drives(): void { covered(); }\n",
        )],
        "src::lib::untestedOne"
    )]
    fn test_roots_come_from_every_supported_language(
        #[case] files: &[(&str, &str)],
        #[case] expected_untested: &str,
    ) {
        let dir = tempfile::tempdir().unwrap();
        for (path, source) in files {
            write_file(dir.path(), path, source);
        }

        let report = analyze_json(dir.path());
        assert_eq!(report["test_roots"]["absent"], false, "report: {report}");
        assert_eq!(report["summary"]["prod_function_count"], 2);
        assert_eq!(report["summary"]["test_reached_function_count"], 1);
        assert_eq!(untested_names(&report), [expected_untested]);
    }

    #[test]
    fn excluding_tests_reports_the_missing_roots_instead_of_the_whole_codebase() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", RUST_SOURCE);

        let json = UntestedAnalyzer::new()
            .with_exclude_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let report: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(report["test_roots"]["absent"], true);
        assert_eq!(report["test_roots"]["function_count"], 0);
        assert_eq!(report["summary"]["untested_function_count"], 4);

        let md = UntestedAnalyzer::new()
            .with_exclude_tests(true)
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("No test function reached the graph"),
            "got: {md}"
        );
        assert!(md.contains("--exclude-tests"), "got: {md}");
    }

    #[test]
    fn only_tests_leaves_no_production_functions_to_judge() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", RUST_SOURCE);

        let md = UntestedAnalyzer::new()
            .with_only_tests(true)
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("_No production functions in scope._"),
            "got: {md}"
        );
    }

    #[test]
    fn a_fully_reached_corpus_says_so_instead_of_rendering_an_empty_section() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn covered() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
             use super::*;\n\
             #[test]\n\
             fn t() { covered(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["summary"]["untested_function_count"], 0);
        assert_eq!(report["summary"]["untested_share"], 0.0);
        let md = analyze_md(dir.path());
        assert!(
            md.contains("Every production function has a resolved call path from a test."),
            "got: {md}",
        );
    }

    #[test]
    fn markdown_states_the_bounds_and_caps_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = String::from("#[cfg(test)]\nmod tests {\n#[test]\nfn t() {}\n}\n");
        for i in 0..(FUNCTIONS_PER_MODULE + 3) {
            let _ = writeln!(source, "pub fn f{i}() {{}}");
        }
        write_file(dir.path(), "src/lib.rs", &source);

        let md = analyze_md(dir.path());
        assert!(md.contains("Structural, not coverage"), "got: {md}");
        assert!(md.contains("Upper-bound support"), "got: {md}");
        assert!(
            md.contains("+3 more (JSON output carries every row)"),
            "got: {md}"
        );
        assert_eq!(
            md.lines().filter(|l| l.starts_with("- `")).count(),
            FUNCTIONS_PER_MODULE,
        );
    }

    #[test]
    fn an_empty_corpus_reports_no_production_functions() {
        let dir = tempfile::tempdir().unwrap();
        let report = analyze_json(dir.path());
        assert_eq!(report["summary"]["prod_function_count"], 0);
        assert_eq!(report["summary"]["untested_share"], 0.0);
        assert_eq!(report["language"], "unknown");
        assert!(report["modules"].as_array().unwrap().is_empty());
    }
}
