//! `analyze taint` — untrusted input reaching a dangerous call, across
//! function boundaries.
//!
//! Pattern scanners (`gosec`, Semgrep's community rules) judge one call
//! site at a time: `exec.Command(name)` is flagged whether `name` came
//! from a request or a constant, and a request value that reaches the
//! shell three helpers down is invisible to a rule that only sees the
//! helper. This analyzer answers the question those leave open — *which
//! request inputs actually arrive at which sinks, and through which
//! call chain* — by joining per-function flow summaries from the
//! language adapter ([`lens_golang::extract_taint_flows`]) with the
//! shared call graph's resolution of every call site, then running
//! [`lens_domain::trace_taint`].
//!
//! Go only for now. The flow model is flow-insensitive and field-
//! insensitive, and calls the graph leaves unresolved are treated as
//! library code that passes its inputs through — so it over-reports
//! through a struct that mixes tainted and clean fields, and
//! under-reports through interface dispatch and function values.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use lens_domain::{ArgRef, FunctionFlow, TaintFinding, trace_taint};
use serde::Serialize;

use super::call_graph::model::{CallGraphNode, Resolution};
use super::call_graph::{CallGraph, CallGraphBuilder, delegate_call_graph_builders};
use super::options::analyzer_options;
use super::runner::render_report;
use super::{
    AnalyzeRoots, AnalyzerError, DiffScope, LineRange, OutputFormat, SourceLang, overlaps_any,
};

const SCHEMA_VERSION: u32 = 1;

/// Markdown listing cap when `--top` is not given. JSON always carries
/// every finding.
const DEFAULT_TOP: usize = 30;

const NOTE: &str = "Each finding is a path from an untrusted value (a parameter typed as an \
     HTTP request handle) to an argument of a dangerous call, through resolved workspace calls. \
     Library calls the graph cannot resolve pass their inputs through to their result, and \
     through `&v` arguments and a plain-variable receiver into that variable; numeric parsing \
     and escaping functions sanitize. Flow- and field-insensitive: a variable is tainted if any \
     write to it is, and writing one field taints the whole value. Interface dispatch and \
     function values are not followed. Go only.";

/// Report order of the vulnerability classes: what an attacker gets
/// first.
const KIND_ORDER: &[&str] = &[
    "command-injection",
    "sql-injection",
    "path-traversal",
    "ssrf",
    "xss",
    "open-redirect",
];

analyzer_options! {
    /// `analyze taint` flags, and the `[profile.<name>.taint]` table.
    pub struct TaintOptions {
        @shared(ranking, diff);
        /// Also treat parameters of this type as untrusted input, as
        /// `import/path.Type` (`example.com/api/gen.CreateRequest`);
        /// repeatable. A pointer to it counts too. Adds to the built-in
        /// HTTP request types (`net/http.Request`, gin, echo, fiber,
        /// fasthttp).
        #[arg(long, value_name = "TYPE")]
        pub source_type: Vec<String>,
    }
}

/// Analyzer entry point for `analyze taint`.
#[derive(Debug, Clone)]
pub struct TaintAnalyzer {
    builder: CallGraphBuilder,
    top: Option<usize>,
    diff: DiffScope,
    source_types: Vec<String>,
}

impl Default for TaintAnalyzer {
    fn default() -> Self {
        Self {
            builder: CallGraphBuilder::new(),
            top: None,
            diff: DiffScope::Disabled,
            source_types: Vec::new(),
        }
    }
}

impl TaintAnalyzer {
    /// Apply a whole [`TaintOptions`] group. The CLI flags and the
    /// `[profile.<name>.taint]` table are the same type, so this is the
    /// only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: TaintOptions) -> Self {
        let diff = opts.diff_scope();
        self.with_top(opts.top)
            .with_diff_scope(diff)
            .with_source_types(opts.source_type)
    }

    pub fn new() -> Self {
        Self::default()
    }

    delegate_call_graph_builders! {
        builder,
        /// Keep only test-like files. Findings whose source sits in a
        /// test are never reported, so this leaves an empty report.
        only_tests,
        /// Drop test files from the graph. Tests are not attack surface,
        /// so this changes nothing but the work done.
        exclude_tests,
    }

    /// Cap the markdown listing to the top-N findings. JSON output
    /// always carries every finding.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    /// Keep only findings whose path runs through a function touching an
    /// unstaged changed line — the flows a pending edit created or
    /// changed.
    pub fn with_diff_only(self, diff_only: bool) -> Self {
        self.with_diff_scope(DiffScope::new(diff_only, None))
    }

    /// Same gate as [`Self::with_diff_only`], against an arbitrary diff.
    pub fn with_diff_scope(mut self, diff: DiffScope) -> Self {
        self.diff = diff;
        self
    }

    /// Extra `import/path.Type` names treated as untrusted input.
    pub fn with_source_types(mut self, source_types: Vec<String>) -> Self {
        self.source_types = source_types;
        self
    }

    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, AnalyzerError> {
        let roots = roots.into();
        let graph = self.builder.build(&roots)?;
        let lowered = Lowered::collect(&self.builder, &roots, &graph, &self.source_types)?;
        let changed = self
            .diff
            .is_enabled()
            .then(|| {
                self.builder
                    .changed_line_ranges_by_display_path(&roots, &self.diff)
            })
            .transpose()?;
        let report = Report::build(&roots, &graph, &lowered, changed.as_ref(), &self.diff);
        render_report(&report, format, || format_markdown(&report, self.top))
    }
}

/// Every Go function's flow, joined to its call-graph node, plus each
/// call site's resolved target as an index into the same list.
struct Lowered {
    /// Graph node index of each flow.
    nodes: Vec<usize>,
    flows: Vec<FunctionFlow>,
    callees: Vec<Vec<Option<usize>>>,
    go_file_count: usize,
    parse_skipped_file_count: usize,
}

impl Lowered {
    fn collect(
        builder: &CallGraphBuilder,
        roots: &AnalyzeRoots,
        graph: &CallGraph,
        source_types: &[String],
    ) -> Result<Self, AnalyzerError> {
        let node_at: HashMap<(&str, usize), usize> = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| ((node.file.as_str(), node.start_line), index))
            .collect();
        let mut lowered = Self {
            nodes: Vec::new(),
            flows: Vec::new(),
            callees: Vec::new(),
            go_file_count: 0,
            parse_skipped_file_count: 0,
        };
        builder.visit_source_texts(roots, |file, source| {
            if SourceLang::from_path(Path::new(file)) != Some(SourceLang::Go) {
                return;
            }
            lowered.go_file_count += 1;
            let Ok(flows) = lens_golang::extract_taint_flows(source, source_types) else {
                lowered.parse_skipped_file_count += 1;
                return;
            };
            for flow in flows {
                // A function the graph's test filter dropped has no node.
                if let Some(&node) = node_at.get(&(file, flow.start_line)) {
                    lowered.nodes.push(node);
                    lowered.flows.push(flow);
                }
            }
        })?;
        lowered.callees = lowered.resolve_calls(graph);
        Ok(lowered)
    }

    /// Join each call site to the graph edge with the same caller, callee
    /// name and line; only resolved edges name a target.
    fn resolve_calls(&self, graph: &CallGraph) -> Vec<Vec<Option<usize>>> {
        let flow_of_node: HashMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(flow, &node)| (graph.nodes[node].id.as_str(), flow))
            .collect();
        let mut target: HashMap<(&str, &str, usize), usize> = HashMap::new();
        for edge in &graph.edges {
            if edge.resolution != Resolution::Resolved {
                continue;
            }
            let (Some(from), Some(to), Some(name)) = (
                edge.from.as_deref(),
                edge.to.as_deref(),
                edge.callee_name.as_deref(),
            ) else {
                continue;
            };
            let Some(&callee) = flow_of_node.get(to) else {
                continue;
            };
            for &line in &edge.call_lines {
                target.insert((from, name, line), callee);
            }
        }
        self.flows
            .iter()
            .zip(&self.nodes)
            .map(|(flow, &node)| {
                let from = graph.nodes[node].id.as_str();
                flow.calls
                    .iter()
                    .map(|call| {
                        let name = call.callee_name.as_deref()?;
                        target.get(&(from, name, call.line)).copied()
                    })
                    .collect()
            })
            .collect()
    }
}

/// A function on a finding, by where it is.
#[derive(Debug, Clone, Serialize)]
struct Location {
    function: String,
    file: String,
    line: usize,
}

#[derive(Debug, Serialize)]
struct SourceView {
    #[serde(flatten)]
    at: Location,
    /// The untrusted parameter, as declared (`r *http.Request`).
    label: String,
}

#[derive(Debug, Serialize)]
struct SinkView {
    #[serde(flatten)]
    at: Location,
    /// The call as written (`exec.Command`).
    call: String,
    /// Zero-based position of the tainted argument.
    argument: usize,
}

/// One hop: a call site the tainted value is passed through.
#[derive(Debug, Serialize)]
struct Hop {
    #[serde(flatten)]
    at: Location,
    call: String,
    /// Zero-based position of the argument carrying the taint.
    argument: usize,
}

#[derive(Debug, Serialize)]
struct Finding {
    /// Vulnerability class (`command-injection`, `sql-injection`,
    /// `path-traversal`, `ssrf`, `xss`, `open-redirect`).
    kind: &'static str,
    /// The sink rule that matched (`os/exec.Command`, `.Query`).
    rule: String,
    source: SourceView,
    sink: SinkView,
    /// Workspace calls between the function holding the tainted value
    /// and the one making the sink call; 0 when they are the same.
    hops: usize,
    /// Call sites from the first function that passes the value on,
    /// down to the sink call (always the last entry).
    path: Vec<Hop>,
}

#[derive(Debug, Serialize)]
struct Audit {
    /// Go files read. Other languages are not analysed.
    go_file_count: usize,
    /// Go files that failed to parse (the graph skips them too).
    parse_skipped_file_count: usize,
    /// Go functions lowered — the denominator.
    function_count: usize,
    /// Untrusted parameters found.
    source_count: usize,
    /// Calls matching a sink rule, before any taint is considered.
    sink_call_count: usize,
    /// Call sites resolved to a workspace function and followed.
    resolved_call_count: usize,
    /// Whether the listing was narrowed to a diff.
    diff_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff_range: Option<String>,
    /// Findings before the diff filter ran.
    finding_count_before_diff_filter: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    note: &'static str,
    audit: Audit,
    /// Findings per vulnerability class.
    by_kind: BTreeMap<&'static str, usize>,
    /// Most severe class first, then shortest path.
    findings: Vec<Finding>,
}

impl Report {
    fn build(
        roots: &AnalyzeRoots,
        graph: &CallGraph,
        lowered: &Lowered,
        changed: Option<&BTreeMap<String, Vec<LineRange>>>,
        diff: &DiffScope,
    ) -> Self {
        let raw = trace_taint(&lowered.flows, &lowered.callees);
        let node = |function: usize| &graph.nodes[lowered.nodes[function]];
        let candidates: Vec<&TaintFinding> = raw
            .iter()
            .filter(|finding| !node(finding.source.function).is_test)
            .collect();
        let before = candidates.len();
        let mut findings: Vec<Finding> = candidates
            .into_iter()
            .filter(|finding| changed.is_none_or(|changed| touches_diff(finding, &node, changed)))
            .map(|finding| view(finding, lowered, &node))
            .collect();
        findings.sort_by(|a, b| {
            kind_rank(a.kind)
                .cmp(&kind_rank(b.kind))
                .then_with(|| a.hops.cmp(&b.hops))
                .then_with(|| a.sink.at.file.cmp(&b.sink.at.file))
                .then_with(|| a.sink.at.line.cmp(&b.sink.at.line))
                .then_with(|| a.source.at.file.cmp(&b.source.at.file))
                .then_with(|| a.source.at.line.cmp(&b.source.at.line))
        });
        let mut by_kind = BTreeMap::new();
        for finding in &findings {
            *by_kind.entry(finding.kind).or_insert(0) += 1;
        }
        let audit = Audit {
            go_file_count: lowered.go_file_count,
            parse_skipped_file_count: lowered.parse_skipped_file_count,
            function_count: lowered.flows.len(),
            source_count: lowered.flows.iter().map(|flow| flow.sources.len()).sum(),
            sink_call_count: lowered
                .flows
                .iter()
                .flat_map(|flow| &flow.calls)
                .filter(|call| call.sink.is_some())
                .count(),
            resolved_call_count: lowered.callees.iter().flatten().flatten().count(),
            diff_only: diff.is_enabled(),
            diff_range: match diff {
                DiffScope::Range(range) => Some(range.clone()),
                _ => None,
            },
            finding_count_before_diff_filter: before,
        };
        Self {
            schema_version: SCHEMA_VERSION,
            root: roots.display(),
            language: "go",
            note: NOTE,
            audit,
            by_kind,
            findings,
        }
    }
}

fn kind_rank(kind: &str) -> usize {
    KIND_ORDER
        .iter()
        .position(|known| *known == kind)
        .unwrap_or(KIND_ORDER.len())
}

/// Whether any function the finding runs through — the source's, each
/// hop's caller, the sink's — overlaps a changed line. A whole-function
/// span, not just the call lines: deleting a sanitizer two lines above
/// the call is exactly the edit this gate exists to catch.
fn touches_diff<'g>(
    finding: &TaintFinding,
    node: &impl Fn(usize) -> &'g CallGraphNode,
    changed: &BTreeMap<String, Vec<LineRange>>,
) -> bool {
    std::iter::once(finding.source.function)
        .chain(finding.path.iter().map(|step| step.call.function))
        .any(|function| {
            let node = node(function);
            changed
                .get(&node.file)
                .is_some_and(|ranges| overlaps_any(node.start_line, node.end_line, ranges))
        })
}

fn view<'g>(
    finding: &TaintFinding,
    lowered: &Lowered,
    node: &impl Fn(usize) -> &'g CallGraphNode,
) -> Finding {
    let at = |function: usize, line: usize| {
        let node = node(function);
        Location {
            function: node.qualified_name.clone(),
            file: node.file.clone(),
            line,
        }
    };
    let call_of = |arg: ArgRef| &lowered.flows[arg.call.function].calls[arg.call.call];
    let source = &lowered.flows[finding.source.function].sources[finding.source.source];
    let sink_call = call_of(finding.sink);
    let (kind, rule) = sink_call
        .sink
        .as_ref()
        .map_or(("unknown", String::new()), |spec| {
            (spec.kind, spec.rule.clone())
        });
    Finding {
        kind,
        rule,
        source: SourceView {
            at: at(finding.source.function, source.line),
            label: source.label.clone(),
        },
        sink: SinkView {
            at: at(finding.sink.call.function, sink_call.line),
            call: sink_call.callee_label.clone(),
            argument: finding.sink.argument,
        },
        hops: finding.path.len().saturating_sub(1),
        path: finding
            .path
            .iter()
            .map(|&step| Hop {
                at: at(step.call.function, call_of(step).line),
                call: call_of(step).callee_label.clone(),
                argument: step.argument,
            })
            .collect(),
    }
}

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let mut out = String::new();
    let audit = &report.audit;
    let _ = writeln!(
        out,
        "# Taint flows: {} finding(s) in {} ({} Go function(s), {} source(s), {} sink call(s))",
        report.findings.len(),
        report.root,
        audit.function_count,
        audit.source_count,
        audit.sink_call_count,
    );
    let _ = writeln!(out, "\n{}", report.note);
    if audit.diff_only {
        let _ = writeln!(
            out,
            "\nDiff-gated: {} of {} finding(s) run through a changed function.",
            report.findings.len(),
            audit.finding_count_before_diff_filter,
        );
    }
    if report.findings.is_empty() {
        let _ = writeln!(out, "\nNo untrusted input reaches a sink.");
        return out;
    }
    let cap = top.unwrap_or(DEFAULT_TOP);
    let mut current = "";
    for finding in report.findings.iter().take(cap) {
        if finding.kind != current {
            current = finding.kind;
            let count = report.by_kind.get(current).copied().unwrap_or(0);
            let _ = writeln!(out, "\n## {current} ({count})\n");
        }
        let _ = writeln!(
            out,
            "- `{}` arg {} at {}:{} ({}) <- `{}` {}:{} ({})",
            finding.rule,
            finding.sink.argument,
            finding.sink.at.file,
            finding.sink.at.line,
            finding.sink.at.function,
            finding.source.label,
            finding.source.at.file,
            finding.source.at.line,
            finding.source.at.function,
        );
        if finding.hops > 0 {
            let hops: Vec<String> = finding
                .path
                .iter()
                .map(|hop| {
                    format!(
                        "{}#{} {}:{}",
                        hop.call, hop.argument, hop.at.file, hop.at.line
                    )
                })
                .collect();
            let _ = writeln!(out, "  - via {}", hops.join(" -> "));
        }
    }
    if report.findings.len() > cap {
        let _ = writeln!(
            out,
            "\n{} more finding(s) in the JSON output.",
            report.findings.len() - cap
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};
    use rstest::rstest;
    use serde_json::Value;

    const MAIN: &str = r#"package main

import (
	"net/http"
	"os/exec"

	"example.com/app/store"
)

type Server struct {
	db *store.Store
}

func (s *Server) handleRun(w http.ResponseWriter, r *http.Request) {
	name := r.URL.Query().Get("name")
	s.runTool(name)
}

func (s *Server) runTool(tool string) {
	runCmd(tool)
}

func runCmd(arg string) {
	_ = exec.Command("sh", "-c", arg).Run()
}

func (s *Server) handleUser(w http.ResponseWriter, r *http.Request) {
	s.db.FindUser(r.FormValue("name"))
}

func safe() {
	runCmd("ls")
}
"#;

    const STORE: &str = r#"package store

import (
	"database/sql"
	"fmt"
)

type Store struct{ db *sql.DB }

func (s *Store) FindUser(name string) {
	q := fmt.Sprintf("SELECT * FROM users WHERE name = '%s'", name)
	s.db.Query(q)
}
"#;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "go.mod", "module example.com/app\n\ngo 1.22\n");
        write_file(dir.path(), "main.go", MAIN);
        write_file(dir.path(), "store/store.go", STORE);
        dir
    }

    fn analyze_json(path: &Path, analyzer: TaintAnalyzer) -> Value {
        serde_json::from_str(&analyzer.analyze(path, OutputFormat::Json).unwrap()).unwrap()
    }

    fn summary(report: &Value) -> Vec<(String, String, u64)> {
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["kind"].as_str().unwrap().to_owned(),
                    f["sink"]["function"].as_str().unwrap().to_owned(),
                    f["hops"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn follows_request_input_across_methods_and_packages() {
        let dir = fixture();
        let report = analyze_json(dir.path(), TaintAnalyzer::new());
        assert_eq!(
            summary(&report),
            [
                ("command-injection".to_owned(), "main::runCmd".to_owned(), 2),
                (
                    "sql-injection".to_owned(),
                    "store::Store::FindUser".to_owned(),
                    1
                ),
            ]
        );
        let command = &report["findings"][0];
        assert_eq!(command["source"]["label"], "r *http.Request");
        assert_eq!(command["source"]["function"], "main::Server::handleRun");
        assert_eq!(command["sink"]["argument"], 2);
        let path: Vec<&str> = command["path"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hop| hop["call"].as_str().unwrap())
            .collect();
        assert_eq!(path, ["s.runTool", "runCmd", "exec.Command"]);
        assert_eq!(report["audit"]["source_count"], 2);
        assert_eq!(report["by_kind"]["sql-injection"], 1);
    }

    #[test]
    fn extra_source_types_add_entry_points() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "rpc.go",
            "package rpc\n\nimport (\n\t\"os\"\n\n\tpb \"example.com/gen\"\n)\n\n\
             type S struct{}\n\n\
             func (s *S) Load(req *pb.LoadRequest) { os.ReadFile(req.Path) }\n",
        );
        let plain = analyze_json(dir.path(), TaintAnalyzer::new());
        assert_eq!(plain["findings"].as_array().unwrap().len(), 0);
        let extended = analyze_json(
            dir.path(),
            TaintAnalyzer::new().with_source_types(vec!["example.com/gen.LoadRequest".to_owned()]),
        );
        assert_eq!(
            summary(&extended),
            [("path-traversal".to_owned(), "rpc::S::Load".to_owned(), 0)]
        );
    }

    #[test]
    fn markdown_lists_each_finding_with_its_path() {
        let dir = fixture();
        let md = TaintAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("## command-injection (1)"), "{md}");
        assert!(
            md.contains(
                "via s.runTool#0 main.go:16 -> runCmd#0 main.go:20 -> exec.Command#2 main.go:24"
            ),
            "{md}"
        );
        assert!(md.contains("## sql-injection (1)"), "{md}");
    }

    #[rstest]
    #[case::cap_below_the_count(1, true)]
    #[case::cap_at_the_count(2, false)]
    fn top_caps_the_markdown_listing(#[case] top: usize, #[case] truncated: bool) {
        let dir = fixture();
        let md = TaintAnalyzer::new()
            .with_top(Some(top))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert_eq!(
            md.contains("1 more finding(s) in the JSON output."),
            truncated,
            "{md}"
        );
        assert_eq!(md.contains("## sql-injection"), !truncated, "{md}");
    }

    #[test]
    fn a_same_function_finding_has_no_via_line() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "h.go",
            "package h\n\nimport (\n\t\"net/http\"\n\t\"os/exec\"\n)\n\n\
             func h(w http.ResponseWriter, r *http.Request) { exec.Command(r.FormValue(\"c\")).Run() }\n",
        );
        let md = TaintAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("## command-injection (1)"), "{md}");
        assert!(!md.contains("  - via"), "{md}");
    }

    #[test]
    fn the_audit_counts_go_files_and_parse_failures() {
        let dir = fixture();
        write_file(dir.path(), "broken.go", "package main\n\nfunc ( {\n");
        let report = analyze_json(dir.path(), TaintAnalyzer::new());
        assert_eq!(report["audit"]["go_file_count"], 3, "{report}");
        assert_eq!(report["audit"]["parse_skipped_file_count"], 1, "{report}");
    }

    #[test]
    fn a_diff_range_is_echoed_in_the_audit() {
        let dir = fixture();
        run_git(dir.path(), &["init", "-q"]);
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "init"]);
        let path = dir.path().join("main.go");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            text.replace("runCmd(tool)\n", "runCmd(tool)\n\t_ = 0\n"),
        )
        .unwrap();
        run_git(dir.path(), &["commit", "-q", "-am", "edit"]);
        let report = analyze_json(
            dir.path(),
            TaintAnalyzer::new().with_diff_scope(DiffScope::Range("HEAD~1..HEAD".to_owned())),
        );
        assert_eq!(report["audit"]["diff_range"], "HEAD~1..HEAD", "{report}");
        assert_eq!(report["findings"].as_array().unwrap().len(), 1, "{report}");
    }

    #[rstest]
    #[case::edit_on_the_path("main.go", "\tname := r.URL.Query().Get(\"name\")\n", 1)]
    #[case::edit_elsewhere("main.go", "\trunCmd(\"ls\")\n", 0)]
    fn diff_only_keeps_findings_running_through_a_changed_function(
        #[case] file: &str,
        #[case] line: &str,
        #[case] expected: usize,
    ) {
        let dir = fixture();
        run_git(dir.path(), &["init", "-q"]);
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "init"]);
        let path = dir.path().join(file);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace(line, &format!("{}\t_ = 0\n", line))).unwrap();
        let report = analyze_json(dir.path(), TaintAnalyzer::new().with_diff_only(true));
        let commands = summary(&report)
            .into_iter()
            .filter(|(kind, ..)| kind == "command-injection")
            .count();
        assert_eq!(commands, expected);
        assert_eq!(report["audit"]["diff_only"], true);
    }

    #[test]
    fn non_go_trees_report_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "pub fn f() {}\n");
        let report = analyze_json(dir.path(), TaintAnalyzer::new());
        assert_eq!(report["audit"]["go_file_count"], 0);
        assert!(report["findings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn output_is_deterministic() {
        let dir = fixture();
        let analyzer = TaintAnalyzer::new();
        let a = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        let b = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        assert_eq!(a, b);
    }
}
