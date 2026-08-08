//! `analyze wrapper` — surface thin forwarding wrappers in source files.
//!
//! Accepts either a single source file or a directory. When the input is a
//! directory the analyzer walks it recursively (respecting `.gitignore`
//! via the `ignore` crate, the same one used by ripgrep), parses every
//! supported file, and reports wrappers grouped by file. Output is JSON by
//! default; the markdown mode emits a compact summary tuned for LLM
//! context windows rather than for humans, in line with the project's
//! "agent-friendly lint" ethos.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

use lens_domain::{InterfaceShape, ReuseMetrics, WrapperFinding};
use serde::Serialize;

use super::options::analyzer_options;
use super::runner::{
    FilterConfig, PerFileReport, PerFileShape, delegate_filter_builders, render_report,
};
use super::{AnalyzeRoots, AnalyzerError, OutputFormat, SourceFile, SourceLang, read_source};

analyzer_options! {
    /// `analyze wrapper` flags, and the `[profile.<name>.wrapper]` table.
    pub struct WrapperOptions {
        @shared(ranking, diff);
    }
}

/// Wrappers listed in markdown when `--top` is not given. JSON always
/// carries every finding; this only bounds the rendered listing, which
/// grows with the codebase and is the half that lands in an agent's
/// context.
const DEFAULT_TOP: usize = 20;

/// Analyzer entry point. Stateless today; kept as a struct so per-run
/// configuration (filters, thresholds) can be added without breaking the
/// CLI surface.
#[derive(Debug, Default, Clone)]
pub struct WrapperAnalyzer {
    filter: FilterConfig,
    top: Option<usize>,
}

impl WrapperAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    delegate_filter_builders!(filter);

    /// Cap the markdown listing to the first N wrappers, in file order.
    /// JSON output always carries every finding. `None` uses the
    /// markdown default of 20.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    /// Apply a whole [`WrapperOptions`] group. The CLI flags and the
    /// `[profile.<name>.wrapper]` table are the same type, so this is the
    /// only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: WrapperOptions) -> Self {
        self.with_top(opts.top).with_diff_only(opts.diff_only)
    }

    /// Walk `roots`, analyze them, and produce a report in `format`.
    /// Accepts a single path or several — see [`AnalyzeRoots`].
    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, AnalyzerError> {
        let roots = roots.into();
        let (files, scanned_file_count) = self.collect_file_reports(&roots)?;
        let report = build_report(&roots, scanned_file_count, &files);
        render_report(&report, format, || format_markdown(&report, self.top))
    }

    /// Resolve `roots` to a list of per-file reports plus the number of
    /// source files scanned to produce them. Single-file inputs produce
    /// a one-element vec; directory inputs walk recursively, honouring
    /// `.gitignore`. Files with no findings are dropped so the output
    /// stays signal-dense, but they still count as scanned.
    fn collect_file_reports(
        &self,
        roots: &AnalyzeRoots,
    ) -> Result<(Vec<FileReport>, usize), AnalyzerError> {
        // Pass 1: produce wrapper findings AND a call-site index for
        // every supported source file. Splitting it from the metric
        // rollup lets reuse metrics see calls in files that themselves
        // contain no wrappers.
        let scan = self
            .filter
            .collect_per_file(roots, |sf| self.scan_file(sf))?;
        let mut per_files = scan.reports;
        // Interface satisfaction is matched against every interface the
        // walk saw, in single-file mode as much as directory mode: "the
        // analyzed tree" is whatever was scanned.
        annotate_interface_satisfaction(&mut per_files);
        // Reuse metrics are workspace-wide by construction. A
        // single-file input only sees calls inside that one file, so
        // every cross-file rollup would trivially be 0. To avoid
        // emitting that misleading "0 sites" signal we leave reuse at
        // `None` in single-file mode and only annotate when the input
        // path is a directory.
        if roots.paths().iter().any(|root| root.is_dir()) {
            annotate_reuse(&mut per_files);
        }
        let files = per_files
            .into_iter()
            .filter_map(PerFile::into_report)
            .collect();
        Ok((files, scan.scanned_file_count))
    }

    /// Walk a single file, returning the per-file slice (wrappers +
    /// call sites) used by `collect_file_reports`. Files with neither
    /// a wrapper nor a call site are dropped at the next stage.
    fn scan_file(&self, file: &SourceFile) -> Result<Option<PerFile>, AnalyzerError> {
        let (lang, source) = read_source(&file.path)?;
        let mut findings = run_wrappers(lang, &source).map_err(AnalyzerError::Parse)?;
        self.filter
            .retain_changed(&mut findings, &file.path, |f| (f.start_line, f.end_line));
        // Call references seed the reuse-metrics pass. Every supported
        // language exposes a call index (Rust via `extract_call_sites`,
        // the rest via `extract_call_shapes_with_module`), so wrappers
        // in any language now get reuse metrics in directory mode —
        // previously this was a Rust-only signal.
        let calls = extract_calls(lang, &source).map_err(AnalyzerError::Parse)?;
        // Go interface method sets seed the may-satisfy annotation: a
        // Go method wrapper matching one by name and arity may exist to
        // satisfy the interface, so the fix is embedding, not deletion.
        // The empty module keeps the report's interface names bare.
        let interfaces = match lang {
            SourceLang::Go => lens_golang::extract_interface_shapes_with_module(&source, "")
                .map_err(|e| AnalyzerError::Parse(Box::new(e)))?,
            _ => Vec::new(),
        };
        if findings.is_empty() && calls.is_empty() && interfaces.is_empty() {
            return Ok(None);
        }
        Ok(Some(PerFile {
            file: file.display_path.clone(),
            findings,
            calls,
            interfaces,
        }))
    }
}

/// Pass-1 row: a file that may carry wrappers, call references,
/// interface declarations, or any mix. Files with none are dropped
/// before the row is built.
struct PerFile {
    file: String,
    findings: Vec<WrapperFinding>,
    calls: Vec<CallRef>,
    /// Named interface method sets declared in this file (Go only).
    interfaces: Vec<InterfaceShape>,
}

/// Language-neutral projection of a call site: the bare callee name
/// (last path segment) and the identity of the function the call is
/// written inside. Both are keyed the same way regardless of source
/// language so the reuse rollup can merge call indices across a mixed
/// directory.
struct CallRef {
    /// Last path segment of the callee (`foo` for `a::b::foo()`), or
    /// `None` when the callee isn't a plain named path.
    callee: Option<String>,
    /// Identity of the enclosing function, in the same qualified form
    /// the wrapper detector uses for `WrapperFinding::name` (`Owner::m`
    /// or a bare name). `None` for calls at module scope.
    caller: Option<String>,
}

/// Extract the call index for `source` as language-neutral [`CallRef`]s.
///
/// Rust keeps its dedicated `extract_call_sites` visitor (whose
/// `caller_name` is already the bare-qualified form the wrapper detector
/// uses). The other adapters go through `extract_call_shapes_with_module`
/// with an empty module prefix, which makes `caller_qualified_name`
/// collapse to the same `Owner::method` / bare-name shape as their
/// wrapper findings — so the self-reference filter and unique-caller
/// counting line up across languages.
fn extract_calls(lang: SourceLang, source: &str) -> Result<Vec<CallRef>, BoxedError> {
    match lang {
        SourceLang::Rust => {
            let sites = lens_rust::extract_call_sites(source).map_err(boxed)?;
            Ok(sites
                .into_iter()
                .map(|s| CallRef {
                    callee: s.callee_name,
                    caller: s.caller_name,
                })
                .collect())
        }
        SourceLang::TypeScript(dialect) => {
            let shapes =
                lens_ts::extract_call_shapes_with_module(source, dialect, "").map_err(boxed)?;
            Ok(shapes.into_iter().map(CallRef::from_shape).collect())
        }
        SourceLang::Python => {
            let shapes = lens_py::extract_call_shapes_with_module(source, "").map_err(boxed)?;
            Ok(shapes.into_iter().map(CallRef::from_shape).collect())
        }
        SourceLang::Go => {
            let shapes = lens_golang::extract_call_shapes_with_module(source, "").map_err(boxed)?;
            Ok(shapes.into_iter().map(CallRef::from_shape).collect())
        }
    }
}

impl CallRef {
    fn from_shape(shape: lens_domain::CallShape) -> Self {
        Self {
            callee: shape.callee_name().map(str::to_owned),
            caller: shape.caller_qualified_name().map(str::to_owned),
        }
    }
}

fn boxed(e: impl std::error::Error + Send + Sync + 'static) -> BoxedError {
    Box::new(e) as BoxedError
}

impl PerFile {
    /// Drop the call-site auxiliary data and convert into the
    /// presentation-side [`FileReport`]. Files whose findings list ended
    /// up empty (only call sites, no wrappers) are filtered out — the
    /// report exists to surface wrappers, not raw call indices.
    fn into_report(self) -> Option<FileReport> {
        if self.findings.is_empty() {
            return None;
        }
        Some(FileReport {
            file: self.file,
            findings: self.findings,
        })
    }
}

/// Walk every wrapper finding across `per_files` and populate its
/// [`ReuseMetrics`] from the merged call-site index. Every supported
/// language contributes a call index, so all findings are annotated in
/// directory mode.
fn annotate_reuse(per_files: &mut [PerFile]) {
    // callee_name -> Vec<(file_index, caller_name)>. Owned keys so the
    // index doesn't keep `per_files` borrowed when we re-walk to write
    // back the metrics.
    let mut index: HashMap<String, Vec<(usize, Option<String>)>> = HashMap::new();
    for (idx, per) in per_files.iter().enumerate() {
        for site in &per.calls {
            let Some(name) = site.callee.as_deref() else {
                continue;
            };
            index
                .entry(name.to_owned())
                .or_default()
                .push((idx, site.caller.clone()));
        }
    }
    let file_paths: Vec<String> = per_files.iter().map(|p| p.file.clone()).collect();
    for (idx, per) in per_files.iter_mut().enumerate() {
        let host_file = file_paths[idx].as_str();
        for finding in &mut per.findings {
            let last_segment = name_last_segment(&finding.name);
            let buckets = index.get(last_segment).cloned().unwrap_or_default();
            // Drop self-references: a call to the wrapper from
            // *inside* the wrapper itself doesn't represent reuse,
            // it's the wrapper's own body. (Recursion on a trivial
            // forwarder is unusual but possible.)
            let buckets: Vec<_> = buckets
                .into_iter()
                .filter(|(_, caller)| caller.as_deref() != Some(finding.name.as_str()))
                .collect();
            let call_sites = buckets.len();
            let same_file_only = buckets.iter().all(|(file_idx, _)| {
                file_paths.get(*file_idx).map(String::as_str) == Some(host_file)
            });
            // Distinct callers: pair each call site with `(file, caller)`.
            // Buckets with `caller = None` still count as one
            // anonymous caller per file (top-level references in a
            // `const` initialiser, etc.), so a wrapper used only at
            // module scope doesn't mis-report 0 callers.
            let callers: std::collections::HashSet<(usize, Option<String>)> =
                buckets.iter().cloned().collect();
            finding.reuse = Some(ReuseMetrics {
                call_sites,
                unique_callers: callers.len(),
                same_file_only,
            });
        }
    }
}

/// Strip qualifier prefixes from a wrapper's `name` to get the bare
/// last segment that appears at every call site (`Service::handle` →
/// `handle`).
fn name_last_segment(name: &str) -> &str {
    name.rsplit_once("::").map_or(name, |(_, last)| last)
}

/// Match every method wrapper against the interface method sets the
/// walk collected, by name and parameter-slot count — the same
/// structural "may satisfy" rule the visibility analyzer applies. Only
/// findings carrying a `param_count` participate (Go methods today);
/// everything else is left untouched.
fn annotate_interface_satisfaction(per_files: &mut [PerFile]) {
    // Method name → arity → interface names, deduplicated and sorted so
    // the annotation order is stable across runs.
    let mut by_method: BTreeMap<String, BTreeMap<usize, BTreeSet<String>>> = BTreeMap::new();
    for interface in per_files.iter().flat_map(|per| &per.interfaces) {
        let Some(name) = interface.qualified_name.known_value() else {
            continue;
        };
        for method in &interface.methods {
            by_method
                .entry(method.name.clone())
                .or_default()
                .entry(method.param_count)
                .or_default()
                .insert(name.clone());
        }
    }
    if by_method.is_empty() {
        return;
    }
    for finding in per_files.iter_mut().flat_map(|per| &mut per.findings) {
        let Some(param_count) = finding.param_count else {
            continue;
        };
        let Some(names) = by_method
            .get(name_last_segment(&finding.name))
            .and_then(|by_arity| by_arity.get(&param_count))
        else {
            continue;
        };
        finding.may_satisfy_interfaces = names.iter().cloned().collect();
    }
}

type BoxedError = Box<dyn std::error::Error + Send + Sync>;

fn run_wrappers(lang: SourceLang, source: &str) -> Result<Vec<WrapperFinding>, BoxedError> {
    super::dispatch_lens!(lang, source, find_wrappers)
}

/// Per-file slice of the report. Owns the display path so directory mode
/// can attach a path relative to the walk root without storing the original
/// `PathBuf`.
#[derive(Debug)]
struct FileReport {
    file: String,
    findings: Vec<WrapperFinding>,
}

/// Names wrapper's two report fields for the shared per-file report.
#[derive(Debug)]
struct WrapperShape;

impl PerFileShape for WrapperShape {
    const COUNT_FIELD: &'static str = "wrapper_count";
    const ITEMS_FIELD: &'static str = "wrappers";
}

type Report<'a> = PerFileReport<'a, WrapperShape, WrapperView<'a>>;

fn build_report<'a>(
    roots: &AnalyzeRoots,
    scanned_file_count: usize,
    files: &'a [FileReport],
) -> Report<'a> {
    let views = files
        .iter()
        .map(|f| {
            super::runner::FileView::new(
                f.file.as_str(),
                f.findings.iter().map(WrapperView::from).collect(),
            )
        })
        .collect();
    PerFileReport::new(roots, scanned_file_count, views)
}

#[derive(Debug, Serialize)]
struct WrapperView<'a> {
    name: &'a str,
    start_line: usize,
    end_line: usize,
    callee: &'a str,
    adapters: &'a [String],
    statement_count: usize,
    /// Workspace-wide reuse metrics. `null` when the finding came from
    /// a single-file run (the call-site universe was not enumerated).
    #[serde(skip_serializing_if = "Option::is_none")]
    reuse: Option<ReuseView>,
    /// Interfaces declared in the scanned tree whose method set names
    /// this wrapper by name and arity (Go). A non-empty list means the
    /// method may exist to satisfy the interface: prefer replacing the
    /// forwarding with embedding over deleting the method. Omitted when
    /// empty.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    may_satisfy_interfaces: &'a [String],
}

/// JSON-facing mirror of [`ReuseMetrics`]. Defined locally so the
/// output schema stays under this analyzer's control even if the
/// domain type grows fields.
#[derive(Debug, Serialize)]
struct ReuseView {
    call_sites: usize,
    unique_callers: usize,
    same_file_only: bool,
}

impl<'a> From<&'a WrapperFinding> for WrapperView<'a> {
    fn from(f: &'a WrapperFinding) -> Self {
        Self {
            name: f.name.as_str(),
            start_line: f.start_line,
            end_line: f.end_line,
            callee: f.callee.as_str(),
            adapters: &f.adapters,
            statement_count: f.statement_count,
            may_satisfy_interfaces: &f.may_satisfy_interfaces,
            reuse: f.reuse.as_ref().map(|r| ReuseView {
                call_sites: r.call_sites,
                unique_callers: r.unique_callers,
                same_file_only: r.same_file_only,
            }),
        }
    }
}

fn format_markdown(report: &Report<'_>, top: Option<usize>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    // The header counts *scanned* files, not files with findings, so a
    // clean run reads "8 file(s) scanned, 0 wrapper(s)" instead of the
    // misleading "0 file(s)" that looks like nothing was analyzed.
    let mut out = format!(
        "# Wrapper report: {} ({} file(s) scanned, {} wrapper(s))\n",
        report.root(),
        report.scanned_file_count(),
        report.item_count(),
    );
    if report.item_count() == 0 {
        out.push_str("\n_No thin forwarding wrappers found._\n");
        return out;
    }
    // The listing has no ranking to take a top-N of, so the cap is spent
    // in file order and what it cut is stated at the end: an agent that
    // needs the rest can raise `--top` or read the JSON, but it has to
    // know there is a rest.
    let mut budget = limit;
    for file in report.files() {
        if budget == 0 {
            break;
        }
        let shown = file.count().min(budget);
        budget -= shown;
        // writeln! into a String cannot fail; the result is swallowed
        // deliberately rather than unwrapped to satisfy the workspace's
        // `unwrap_used` lint.
        let _ = writeln!(
            out,
            "\n## {} ({})",
            file.file(),
            file_heading_count(shown, file.count()),
        );
        for w in file.items().iter().take(shown) {
            let _ = writeln!(out, "{}", wrapper_row(w));
        }
    }
    let omitted = report.item_count().saturating_sub(limit);
    if omitted > 0 {
        let _ = writeln!(
            out,
            "\n_{omitted} more wrapper(s) not shown (--top {limit}); raise --top or use \
             --format json for the full list._",
        );
    }
    out
}

/// A file section's wrapper count. A partially listed file says so: the
/// total is the file's, and an agent reading only that section must not
/// take the rows it can see for all of them.
fn file_heading_count(shown: usize, total: usize) -> String {
    if shown < total {
        format!("{shown} of {total} wrapper(s)")
    } else {
        format!("{total} wrapper(s)")
    }
}

/// One wrapper's markdown line: name and span, the forwarding body, and
/// the two optional annotations that change what to do about it.
fn wrapper_row(w: &WrapperView<'_>) -> String {
    // Body shape: callee chain plus optional adapter suffix.
    let body = if w.adapters.is_empty() {
        format!("-> {}", w.callee)
    } else {
        format!("-> {} [via {}]", w.callee, w.adapters.join(""))
    };
    // Reuse chip: only attached when the finding had reuse metrics
    // (directory mode). Kept terse so the line stays scannable at
    // agent-context density.
    let reuse = match &w.reuse {
        Some(r) => format!(
            "  \u{2022} {} site(s), {} caller(s), {}",
            r.call_sites,
            r.unique_callers,
            if r.same_file_only {
                "same-file"
            } else {
                "cross-file"
            },
        ),
        None => String::new(),
    };
    // Interface chip: the annotation that flips the fix from "delete the
    // method" to "embed the inner value".
    let interfaces = if w.may_satisfy_interfaces.is_empty() {
        String::new()
    } else {
        format!(
            "  \u{2022} may satisfy `{}` — embedding could replace the forwarding",
            w.may_satisfy_interfaces.join("`, `"),
        )
    };
    format!(
        "- `{}` (L{}-{}) {body}{reuse}{interfaces}",
        w.name, w.start_line, w.end_line,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};
    use std::path::Path;

    #[test]
    fn json_report_lists_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn render(x: &str) -> String { internal_render(x) }
fn meaningful(x: i32) -> i32 { let y = x + 1; y * 2 }
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let json = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["wrapper_count"], 1);
        assert_eq!(parsed["file_count"], 1);
        let files = parsed["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        let wrappers = files[0]["wrappers"].as_array().unwrap();
        assert_eq!(wrappers[0]["name"], "render");
        assert_eq!(wrappers[0]["callee"], "internal_render");
        let names: Vec<&str> = wrappers
            .iter()
            .map(|w| w["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"meaningful"));
    }

    #[test]
    fn json_report_includes_adapter_chain() {
        let dir = tempfile::tempdir().unwrap();
        let src = "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n";
        let file = write_file(dir.path(), "lib.rs", src);
        let json = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let adapters = parsed["files"][0]["wrappers"][0]["adapters"]
            .as_array()
            .unwrap();
        let joined: String = adapters
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("");
        assert!(joined.contains(".unwrap()"));
        assert!(joined.contains(".into()"));
    }

    #[test]
    fn markdown_report_lists_wrappers_and_adapter_chain() {
        let dir = tempfile::tempdir().unwrap();
        let src = "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n";
        let file = write_file(dir.path(), "lib.rs", src);
        let md = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Wrapper report"));
        assert!(md.contains("shim"));
        assert!(md.contains("compute"));
        assert!(md.contains("via"));
        assert!(md.contains(".unwrap()"));
        assert!(md.contains(".into()"));
    }

    /// The listing has no ranking, so `--top` spends its budget in file
    /// order — and must say what it left out, both per partially-shown
    /// file and in total.
    #[test]
    fn markdown_listing_is_capped_by_top_and_reports_what_it_dropped() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.rs",
            "fn one(x: &str) -> String { inner_one(x) }\n\
             fn two(x: &str) -> String { inner_two(x) }\n",
        );
        write_file(
            dir.path(),
            "b.rs",
            "fn three(x: &str) -> String { inner_three(x) }\n",
        );

        let md = WrapperAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("3 wrapper(s)"), "header keeps the total: {md}");
        assert!(md.contains("## a.rs (1 of 2 wrapper(s))"), "got: {md}");
        assert!(md.contains("`one`"), "got: {md}");
        assert!(!md.contains("`two`"), "got: {md}");
        // The second file never starts: the budget ran out inside a.rs.
        assert!(!md.contains("## b.rs"), "got: {md}");
        assert!(md.contains("_2 more wrapper(s) not shown"), "got: {md}");

        // A budget above the finding count lists everything, with no
        // "of N" qualifier and no trailing note.
        let md = WrapperAnalyzer::new()
            .with_top(Some(10))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("## a.rs (2 wrapper(s))"), "got: {md}");
        assert!(md.contains("## b.rs (1 wrapper(s))"), "got: {md}");
        assert!(!md.contains("not shown"), "got: {md}");
    }

    /// JSON is the machine-readable half and must stay complete: `--top`
    /// is a markdown-rendering cap, not a filter on the analysis.
    #[test]
    fn top_does_not_touch_the_json_report() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.rs",
            "fn one(x: &str) -> String { inner_one(x) }\n\
             fn two(x: &str) -> String { inner_two(x) }\n",
        );
        let json = WrapperAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["wrapper_count"], 2, "got {parsed}");
    }

    #[test]
    fn empty_report_when_no_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn alpha(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs { total += *x; }
    total
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let md = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("No thin forwarding wrappers"));
        // Even with nothing found, the header must say the file was
        // scanned rather than the misleading "0 file(s)".
        assert!(md.contains("1 file(s) scanned"), "got: {md}");
    }

    #[test]
    fn directory_with_no_wrappers_still_reports_scanned_files() {
        // Regression test for issue #140: a directory whose files parse
        // fine but contain no wrappers used to report "0 file(s)",
        // indistinguishable from an extension/path-filter problem. The
        // report must count scanned files independently of findings.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "app.ts",
            "export function meaningful(x: number): number { const y = x + 1; return y * 2; }\n",
        );
        write_file(
            dir.path(),
            "lib.rs",
            r#"
fn alpha(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs { total += *x; }
    total
}
"#,
        );

        let md = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("2 file(s) scanned, 0 wrapper(s)"), "got: {md}");
        assert!(md.contains("No thin forwarding wrappers"), "got: {md}");

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["scanned_file_count"], 2, "got {parsed}");
        assert_eq!(parsed["file_count"], 0, "got {parsed}");
        assert_eq!(parsed["wrapper_count"], 0, "got {parsed}");
    }

    #[test]
    fn unknown_extension_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "notes.txt", "hello");
        let err = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::UnsupportedExtension { .. }));
    }

    #[test]
    fn python_wrapper_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let src = "
def render(x):
    return internal_render(x)

def meaningful(x):
    y = x + 1
    return y * 2
";
        let file = write_file(dir.path(), "lib.py", src);
        let json = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["wrapper_count"], 1);
        let wrapper = &parsed["files"][0]["wrappers"][0];
        assert_eq!(wrapper["name"], "render");
        assert_eq!(wrapper["callee"], "internal_render");
    }

    #[test]
    fn missing_file_surfaces_not_found_error() {
        let err = WrapperAnalyzer::new()
            .analyze(
                Path::new("/definitely/does/not/exist.rs"),
                OutputFormat::Json,
            )
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::PathNotFound { .. }), "{err:?}");
    }

    #[test]
    fn invalid_rust_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "broken.rs", "fn ??? {");
        let err = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::Parse(_)));
    }

    #[test]
    fn diff_only_filters_to_changed_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
fn render(x: &str) -> String { internal_render(x) }
fn passthrough(x: i32) -> i32 { compute(x) }
"#,
        );
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        run_git(dir.path(), &["add", "lib.rs"]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        write_file(
            dir.path(),
            "lib.rs",
            r#"
fn render(x: &str) -> String { internal_render(x).into() }
fn passthrough(x: i32) -> i32 { compute(x) }
"#,
        );
        let json = WrapperAnalyzer::new()
            .with_diff_only(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["wrapper_count"], 1);
        assert_eq!(parsed["files"][0]["wrappers"][0]["name"], "render");
    }

    #[test]
    fn directory_mode_groups_wrappers_per_file() {
        // Two wrappers split across two files: only visible to the
        // analyzer once it walks the directory. The output shape is
        // grouped per file so the agent can attribute each finding.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.rs",
            "fn render(x: &str) -> String { internal_render(x) }\n",
        );
        write_file(
            dir.path(),
            "nested/b.rs",
            "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n",
        );
        // A file with no wrappers should not appear in the report at all.
        write_file(
            dir.path(),
            "noop.rs",
            r#"
fn meaningful(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs { total += *x; }
    total
}
"#,
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["wrapper_count"], 2);
        assert_eq!(parsed["file_count"], 2);
        // The wrapper-less noop.rs is excluded from `files` but still
        // counted as scanned.
        assert_eq!(parsed["scanned_file_count"], 3);
        let files = parsed["files"].as_array().unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f["file"].as_str().unwrap()).collect();
        assert!(paths.contains(&"a.rs"), "got {paths:?}");
        assert!(paths.contains(&"nested/b.rs"), "got {paths:?}");
        assert!(!paths.contains(&"noop.rs"), "got {paths:?}");
    }

    #[test]
    fn directory_mode_skips_unsupported_extensions_and_gitignored_files() {
        // `.gitignore` should be honoured (the `ignore` walker is
        // gitignore-aware out of the box), and unsupported extensions
        // should be silently skipped.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.rs",
            "fn render(x: &str) -> String { internal_render(x) }\n",
        );
        write_file(
            dir.path(),
            "ignored.rs",
            "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n",
        );
        write_file(dir.path(), "notes.txt", "not a source file");
        write_file(dir.path(), ".gitignore", "ignored.rs\n");

        // The `ignore` crate honours .gitignore only inside a git repo
        // by default; bootstrap one so the test exercises the gitignore
        // path rather than just the extension filter.
        let status = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["wrapper_count"], 1, "got {parsed}");
        assert_eq!(parsed["file_count"], 1, "got {parsed}");
        assert_eq!(parsed["files"][0]["file"], "a.rs");
    }

    #[test]
    fn directory_mode_markdown_renders_per_file_sections() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.rs",
            "fn render(x: &str) -> String { internal_render(x) }\n",
        );
        write_file(
            dir.path(),
            "nested/b.rs",
            "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n",
        );

        let md = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Wrapper report"));
        assert!(md.contains("2 file(s)"));
        assert!(md.contains("2 wrapper(s)"));
        assert!(md.contains("## a.rs"));
        assert!(md.contains("## nested/b.rs"));
        assert!(md.contains("render"));
        assert!(md.contains("shim"));
    }

    #[test]
    fn path_filters_apply_to_directory_walks() {
        let dir = tempfile::tempdir().unwrap();
        let wrapper = "fn render(x: &str) -> String { internal_render(x) }\n";
        write_file(dir.path(), "src/lib.rs", wrapper);
        write_file(dir.path(), "tests/lib_test.rs", wrapper);
        write_file(dir.path(), "src/generated.rs", wrapper);

        let only_tests = WrapperAnalyzer::new()
            .with_only_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&only_tests).unwrap();
        assert_eq!(parsed["file_count"], 1);
        assert_eq!(parsed["files"][0]["file"], "tests/lib_test.rs");

        let exclude_tests = WrapperAnalyzer::new()
            .with_exclude_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exclude_tests).unwrap();
        let files: Vec<&str> = parsed["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["file"].as_str().unwrap())
            .collect();
        assert!(files.contains(&"src/lib.rs"));
        assert!(!files.contains(&"tests/lib_test.rs"));

        let exclude_generated = WrapperAnalyzer::new()
            .with_exclude_patterns(vec!["generated.rs".to_owned()])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exclude_generated).unwrap();
        let files: Vec<&str> = parsed["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["file"].as_str().unwrap())
            .collect();
        assert!(!files.contains(&"src/generated.rs"));
    }

    #[test]
    fn directory_mode_populates_reuse_metrics_across_files() {
        // Two files: `wrap.rs` defines `render` (a wrapper). `caller.rs`
        // calls it from `consumer`. The wrapper itself is not called
        // from inside `wrap.rs`, so reuse spans one cross-file caller.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "wrap.rs",
            "fn render(x: &str) -> String { internal_render(x) }\n",
        );
        write_file(
            dir.path(),
            "caller.rs",
            "fn consumer() { let _ = render(\"hi\"); }\n",
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let wrappers = parsed["files"][0]["wrappers"].as_array().unwrap();
        let render = wrappers
            .iter()
            .find(|w| w["name"] == "render")
            .expect("render wrapper missing");
        assert_eq!(render["statement_count"], 1);
        let reuse = &render["reuse"];
        assert_eq!(reuse["call_sites"], 1);
        assert_eq!(reuse["unique_callers"], 1);
        assert_eq!(reuse["same_file_only"], false);
    }

    #[test]
    fn directory_mode_populates_reuse_metrics_for_python() {
        // The reuse pass is no longer Rust-only: a Python wrapper
        // called from another file must report cross-file reuse.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "wrap.py",
            "def render(x):\n    return internal_render(x)\n",
        );
        write_file(
            dir.path(),
            "caller.py",
            "def consumer():\n    return render(\"hi\")\n",
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let render = parsed["files"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|f| f["wrappers"].as_array().unwrap())
            .find(|w| w["name"] == "render")
            .expect("render wrapper missing");
        let reuse = &render["reuse"];
        assert_eq!(reuse["call_sites"], 1, "got {parsed}");
        assert_eq!(reuse["unique_callers"], 1, "got {parsed}");
        assert_eq!(reuse["same_file_only"], false, "got {parsed}");
    }

    #[test]
    fn directory_mode_populates_reuse_metrics_for_go() {
        // Same cross-language reuse guarantee for Go.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "wrap.go",
            "package p\n\nfunc Render(x int) int { return internalRender(x) }\n",
        );
        write_file(
            dir.path(),
            "caller.go",
            "package p\n\nfunc consumer() int { return Render(1) }\n",
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let render = parsed["files"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|f| f["wrappers"].as_array().unwrap())
            .find(|w| w["name"] == "Render")
            .expect("Render wrapper missing");
        let reuse = &render["reuse"];
        assert_eq!(reuse["call_sites"], 1, "got {parsed}");
        assert_eq!(reuse["unique_callers"], 1, "got {parsed}");
        assert_eq!(reuse["same_file_only"], false, "got {parsed}");
    }

    /// A Go method wrapper matching an in-tree interface's method by
    /// name and arity is annotated rather than reported bare: the
    /// method may exist to satisfy the interface, and the fix is
    /// embedding the inner value, not deleting the method. The
    /// interface lives in another file on purpose — the match is
    /// against everything the walk scanned.
    #[test]
    fn go_method_wrappers_matching_an_interface_carry_the_annotation() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "store.go",
            "package p\n\ntype Store interface {\n\tSave(id int) error\n\tDrop(id int, hard bool) error\n}\n",
        );
        write_file(
            dir.path(),
            "wrap.go",
            "package p\n\ntype Wrapped struct{ inner Store }\n\n\
             func (w Wrapped) Save(id int) error { return w.inner.Save(id) }\n\n\
             func (w Wrapped) Drop(id int) error { return w.inner.Drop(id) }\n\n\
             func Save(id int) error { return realSave(id) }\n",
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let wrapper = |name: &str| {
            parsed["files"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|f| f["wrappers"].as_array().unwrap())
                .find(|w| w["name"] == name)
                .unwrap_or_else(|| panic!("{name} wrapper missing: {parsed}"))
                .clone()
        };
        assert_eq!(
            wrapper("Wrapped::Save")["may_satisfy_interfaces"],
            serde_json::json!(["Store"]),
            "got {parsed}",
        );
        // Same method name, wrong arity: the interface declares two
        // parameters for Drop, the wrapper takes one.
        assert!(
            wrapper("Wrapped::Drop")["may_satisfy_interfaces"].is_null(),
            "got {parsed}",
        );
        // Free functions never satisfy an interface, whatever the name.
        assert!(
            wrapper("Save")["may_satisfy_interfaces"].is_null(),
            "got {parsed}",
        );

        let md = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("may satisfy `Store` — embedding could replace the forwarding"),
            "got: {md}",
        );
    }

    #[test]
    fn directory_mode_marks_same_file_only_when_caller_is_local() {
        // A wrapper used only inside its own file: `same_file_only` is
        // true and the caller count is 1. This is the canonical "low
        // reuse, low blast radius" finding the agent should treat as
        // safe to inline.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "lib.rs",
            r#"
fn render(x: &str) -> String { internal_render(x) }
fn consumer() { let _ = render("hi"); }
"#,
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let render = parsed["files"][0]["wrappers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["name"] == "render")
            .expect("render wrapper missing");
        let reuse = &render["reuse"];
        assert_eq!(reuse["call_sites"], 1);
        assert_eq!(reuse["unique_callers"], 1);
        assert_eq!(reuse["same_file_only"], true);
    }

    #[test]
    fn directory_mode_zero_call_sites_for_unused_wrapper() {
        // A wrapper that nothing else in the tree calls: `call_sites`
        // is 0. `same_file_only` is `true` by convention (the empty
        // call set trivially satisfies "all calls are local"), and the
        // agent reads it together with the count rather than in
        // isolation.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "wrap.rs",
            "fn unused(x: &str) -> String { internal_render(x) }\n",
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let reuse = &parsed["files"][0]["wrappers"][0]["reuse"];
        assert_eq!(reuse["call_sites"], 0);
        assert_eq!(reuse["unique_callers"], 0);
        assert_eq!(reuse["same_file_only"], true);
    }

    #[test]
    fn single_file_mode_leaves_reuse_unset() {
        // With a single source file as the input there is no
        // workspace to enumerate calls across, so `reuse` is omitted
        // from the JSON entirely (the field is `Option` and skipped
        // when None).
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            "fn render(x: &str) -> String { internal_render(x) }\n",
        );
        let json = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let render = &parsed["files"][0]["wrappers"][0];
        assert_eq!(render["statement_count"], 1);
        assert!(render.get("reuse").is_none_or(|v| v.is_null()));
    }

    #[test]
    fn directory_mode_excludes_self_recursive_calls_from_reuse() {
        // A pathological wrapper that recurses on itself shouldn't
        // double-count its own body as reuse. The recursive call is
        // dropped from the bucket so call_sites stays 0.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "wrap.rs",
            "fn render(x: &str) -> String { render(x) }\n",
        );
        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let reuse = &parsed["files"][0]["wrappers"][0]["reuse"];
        assert_eq!(reuse["call_sites"], 0);
    }

    #[test]
    fn markdown_directory_report_renders_reuse_suffix() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "wrap.rs",
            "fn render(x: &str) -> String { internal_render(x) }\n",
        );
        write_file(
            dir.path(),
            "caller.rs",
            "fn consumer() { let _ = render(\"hi\"); }\n",
        );

        let md = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        // The reuse rollup is rendered as a terse trailing chip after
        // the body so the line stays scannable. Format details (bullet
        // glyph, exact wording) may shift; check for the count and the
        // cross-file marker.
        assert!(md.contains("1 site"), "missing reuse count: {md}");
        assert!(md.contains("cross-file"), "missing locality: {md}");
    }
}
