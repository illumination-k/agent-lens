//! Compact metric snapshots of a project — the input a ratchet needs.
//!
//! An analyzer report answers "what is wrong here, right now". A
//! baseline answers a different question: "did this get worse than it
//! was". That comparison needs a stored artifact, and storing whole
//! reports does not work — they run to megabytes, churn on every
//! unrelated edit, and pin down a per-analyzer schema that is still
//! moving. So a baseline keeps only scalars: per analyzer, a handful of
//! named numbers that summarise the whole run.
//!
//! Two properties follow from being an artifact rather than a report:
//!
//! * **Deterministic.** The same tree at the same commit produces a
//!   byte-identical file. Nothing here reads the clock — the commit
//!   recorded in the snapshot is the anchor, and a wall-clock timestamp
//!   would only make every regeneration look like a change.
//! * **Diffable.** Metrics live in a sorted map and the document is
//!   pretty-printed, so a regression shows up as a one-line diff naming
//!   the metric that moved.
//!
//! Metrics are extracted from each analyzer's JSON report rather than
//! from its internal types: the analyzers hand back a rendered `String`,
//! and the JSON shape is the surface they already commit to. A metric
//! that is absent from a report (an empty scan has no `summary`) is
//! omitted rather than defaulted to zero — "no functions were measured"
//! and "the worst function scored 0" are different facts, and only one
//! of them should survive into a comparison.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

use crate::config::ToolName;

/// Schema version of the baseline document. Bumped whenever an existing
/// field changes meaning, so a comparison can refuse a snapshot it does
/// not understand instead of silently mis-reading it.
pub const SCHEMA_VERSION: u32 = 1;

/// Metric names carry no units and no direction here; the analyzer that
/// owns each one defines it (see the README's baseline table).
pub type Metrics = BTreeMap<String, Number>;

/// One profile's metric snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub schema_version: u32,
    /// `agent-lens` version that produced the snapshot. Metric
    /// definitions can change between releases, so a comparison across
    /// two versions is not automatically apples-to-apples.
    pub tool_version: String,
    /// Profile the tools and filters came from.
    pub profile: String,
    /// Analyzed path, as written in the profile.
    pub target: String,
    /// Commit the snapshot describes, when the target sits in a git
    /// tree. `None` outside one — the snapshot is still valid, it just
    /// cannot say what it was taken against.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub commit: Option<String>,
    /// Per-analyzer metrics, in the profile's tool order.
    pub tools: Vec<ToolBaseline>,
    /// Tools the profile listed that have no baseline summary yet.
    /// Recorded rather than dropped: a snapshot must be able to say what
    /// it does *not* cover.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped: Vec<SkippedTool>,
}

/// One analyzer's contribution to a snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolBaseline {
    pub tool: String,
    pub metrics: Metrics,
}

/// A profile tool that ran (or would have run) but has no summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkippedTool {
    pub tool: String,
    pub reason: String,
}

impl Baseline {
    /// Serialize as the on-disk/stdout form: pretty-printed, one
    /// trailing newline. Compact JSON is the convention for analyzer
    /// reports because they are piped into a context window; a baseline
    /// is instead read as a diff, where one metric per line is the
    /// whole point.
    pub fn render(&self) -> Result<String, serde_json::Error> {
        let mut out = serde_json::to_string_pretty(self)?;
        out.push('\n');
        Ok(out)
    }
}

/// Reason recorded for a tool with no summary defined.
pub const NO_SUMMARY_REASON: &str = "no baseline summary defined for this analyzer";

/// Reduces one analyzer's JSON report to its baseline metrics.
pub type Summarizer = fn(&Value) -> Metrics;

/// The summarizer for `tool`, or `None` when a snapshot cannot cover it.
///
/// Handing back the function rather than the metrics lets the caller ask
/// "is this tool covered" *before* paying to run the analyzer, and keeps
/// one list of covered analyzers instead of a support check that can
/// drift from the extraction.
///
/// Covered today are the six analyzers whose reports are whole-project
/// aggregates over a stable shape; the graph analyzers (hubs, impact,
/// layers, …) rank individual symbols, and what a *snapshot* of one
/// should be is not yet settled. An uncovered tool is recorded under
/// [`Baseline::skipped`] rather than given invented metrics.
pub fn summarizer(tool: ToolName) -> Option<Summarizer> {
    match tool {
        ToolName::Complexity => Some(complexity_metrics),
        ToolName::Cohesion => Some(cohesion_metrics),
        ToolName::Coupling => Some(coupling_metrics),
        ToolName::ContextSpan => Some(context_span_metrics),
        ToolName::Hotspot => Some(hotspot_metrics),
        ToolName::Similarity => Some(similarity_metrics),
        ToolName::Cycles
        | ToolName::Delegation
        | ToolName::FunctionGraph
        | ToolName::GraphQuery
        | ToolName::Hubs
        | ToolName::Impact
        | ToolName::Layers
        | ToolName::Risk
        | ToolName::Untested
        | ToolName::Visibility
        | ToolName::Wrapper => None,
    }
}

/// Size of the measured surface plus the worst-case and aggregate
/// complexity figures. `maintainability_index_min` is the one metric
/// here that is worse when it *falls*.
fn complexity_metrics(report: &Value) -> Metrics {
    let mut metrics = Metrics::new();
    put(&mut metrics, "file_count", scalar(report, &["file_count"]));
    put(
        &mut metrics,
        "function_count",
        scalar(report, &["function_count"]),
    );
    for name in [
        "cyclomatic_max",
        "cognitive_max",
        "cognitive_p95",
        "max_nesting_max",
        "loc_total",
        "maintainability_index_min",
    ] {
        put(&mut metrics, name, scalar(report, &["summary", name]));
    }
    metrics
}

/// LCOM4 counts one unit's disconnected method groups, so `lcom4 >= 2`
/// is the "this type does more than one thing" population — tracked as a
/// count next to the worst single unit.
fn cohesion_metrics(report: &Value) -> Metrics {
    let mut metrics = Metrics::new();
    put(&mut metrics, "file_count", scalar(report, &["file_count"]));
    put(&mut metrics, "unit_count", scalar(report, &["unit_count"]));
    let lcom4 = collect(report, &["files", "*", "units", "*", "lcom4"]);
    put(&mut metrics, "lcom4_max", max(&lcom4));
    put(
        &mut metrics,
        "split_unit_count",
        count(&lcom4, |value| value >= 2.0),
    );
    metrics
}

fn coupling_metrics(report: &Value) -> Metrics {
    let mut metrics = Metrics::new();
    for name in ["module_count", "edge_count", "cycle_count"] {
        put(&mut metrics, name, scalar(report, &[name]));
    }
    for name in ["fan_in", "fan_out", "ifc"] {
        put(
            &mut metrics,
            &format!("{name}_max"),
            max(&collect(report, &["modules", "*", name])),
        );
    }
    metrics
}

/// The span sum is the total reading cost of the crate: shrinking one
/// module's span while inflating every other one should not read as an
/// improvement, which a max alone would allow.
fn context_span_metrics(report: &Value) -> Metrics {
    let mut metrics = Metrics::new();
    put(
        &mut metrics,
        "module_count",
        scalar(report, &["module_count"]),
    );
    let transitive = collect(report, &["modules", "*", "transitive"]);
    put(&mut metrics, "transitive_max", max(&transitive));
    put(&mut metrics, "transitive_sum", sum(&transitive));
    metrics
}

fn hotspot_metrics(report: &Value) -> Metrics {
    let mut metrics = Metrics::new();
    put(&mut metrics, "file_count", scalar(report, &["file_count"]));
    for name in ["score_max", "commits_max", "cognitive_max"] {
        put(&mut metrics, name, scalar(report, &["summary", name]));
    }
    metrics
}

/// `cluster_count` alone hides whether clusters are growing: two
/// clusters of two and two of twenty are the same count and very
/// different amounts of duplication, so the clustered-unit total and the
/// largest cluster travel with it.
fn similarity_metrics(report: &Value) -> Metrics {
    let mut metrics = Metrics::new();
    put(&mut metrics, "unit_count", scalar(report, &["unit_count"]));
    put(
        &mut metrics,
        "cluster_count",
        scalar(report, &["cluster_count"]),
    );
    let sizes = collect(report, &["clusters", "*", "size"]);
    put(&mut metrics, "clustered_unit_count", sum(&sizes));
    put(&mut metrics, "cluster_max_size", max(&sizes));
    metrics
}

/// Insert `value` under `name` when the report actually carried it.
fn put(metrics: &mut Metrics, name: &str, value: Option<Number>) {
    if let Some(value) = value {
        metrics.insert(name.to_owned(), value);
    }
}

/// Read the number at `path`, keeping the report's own integer/float
/// spelling. A missing key, or a key holding something other than a
/// number, yields `None`.
fn scalar(report: &Value, path: &[&str]) -> Option<Number> {
    let mut node = report;
    for key in path {
        node = node.get(key)?;
    }
    node.as_number().cloned()
}

/// The numbers found under one report path, plus whether the report
/// actually carried the collection they live in.
///
/// The distinction is what keeps an empty aggregate honest: a report
/// listing zero clusters *did* measure duplication and found none, while
/// a report with no `clusters` key at all measured nothing. Both give an
/// empty `values`, and only the first may be summarised as `0`.
struct Population {
    values: Vec<f64>,
    measured: bool,
}

/// Collect every number reachable through `path`, where a `*` segment
/// descends into each element of an array. Non-numeric and missing
/// values (an optional metric left `null`) are skipped, so the values
/// are the population that was actually measured.
fn collect(report: &Value, path: &[&str]) -> Population {
    let mut population = Population {
        values: Vec::new(),
        measured: false,
    };
    walk(report, path, &mut population);
    population
}

fn walk(node: &Value, path: &[&str], population: &mut Population) {
    let Some((segment, rest)) = path.split_first() else {
        if let Some(value) = node.as_f64() {
            population.values.push(value);
        }
        return;
    };
    if *segment == "*" {
        if let Some(items) = node.as_array() {
            population.measured = true;
            for item in items {
                walk(item, rest, population);
            }
        }
        return;
    }
    if let Some(child) = node.get(segment) {
        walk(child, rest, population);
    }
}

/// `None` for an empty population: the maximum of nothing is not zero,
/// and a comparison must not read it as one.
fn max(population: &Population) -> Option<Number> {
    population
        .values
        .iter()
        .copied()
        .reduce(f64::max)
        .and_then(number_from_f64)
}

/// Unlike a maximum, a total over an empty-but-present collection is
/// well defined and worth recording: "no duplication today" is exactly
/// the state a ratchet wants to hold.
fn sum(population: &Population) -> Option<Number> {
    if !population.measured {
        return None;
    }
    number_from_f64(population.values.iter().sum())
}

fn count(population: &Population, predicate: impl Fn(f64) -> bool) -> Option<Number> {
    if !population.measured {
        return None;
    }
    Some(Number::from(
        population
            .values
            .iter()
            .copied()
            .filter(|value| predicate(*value))
            .count(),
    ))
}

/// Render a computed aggregate, keeping whole numbers whole so a count
/// of clusters serializes as `12` rather than `12.0`.
fn number_from_f64(value: f64) -> Option<Number> {
    if value.is_finite() && value.fract() == 0.0 && value.abs() <= i64::MAX as f64 {
        return Some(Number::from(value as i64));
    }
    Number::from_f64(value)
}

/// The commit the snapshot describes: `git rev-parse HEAD` run inside
/// the target's working tree.
///
/// Deliberately a `git` subprocess rather than reading `.git/HEAD` by
/// hand — the ref indirection, packed refs, and detached-HEAD cases are
/// git's job, and unlike the hook path this runs once per snapshot. Any
/// failure (no git on `PATH`, not a repository, an empty repository with
/// no commit yet) is `None`: a baseline without a commit is still a
/// usable baseline.
pub fn head_commit(target: &Path) -> Option<String> {
    let root = crate::paths::git_repo_root(target)?;
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8(output.stdout).ok()?;
    let commit = commit.trim();
    (!commit.is_empty()).then(|| commit.to_owned())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    /// Every analyzer a snapshot covers.
    const SUMMARIZED: [ToolName; 6] = [
        ToolName::Complexity,
        ToolName::Cohesion,
        ToolName::Coupling,
        ToolName::ContextSpan,
        ToolName::Hotspot,
        ToolName::Similarity,
    ];

    fn metric(metrics: &Metrics, name: &str) -> Option<f64> {
        metrics.get(name).and_then(Number::as_f64)
    }

    #[test]
    fn complexity_summary_pulls_counts_and_summary_block() {
        let report = json!({
            "root": "src",
            "scanned_file_count": 9,
            "file_count": 4,
            "function_count": 31,
            "summary": {
                "cyclomatic_max": 16,
                "cyclomatic_p95": 7,
                "cognitive_max": 24,
                "cognitive_p95": 9,
                "cognitive_median": 1,
                "max_nesting_max": 5,
                "loc_total": 812,
                "maintainability_index_min": 41.5,
            },
            "files": [],
        });
        let metrics = summarizer(ToolName::Complexity).unwrap()(&report);
        assert_eq!(metric(&metrics, "file_count"), Some(4.0));
        assert_eq!(metric(&metrics, "function_count"), Some(31.0));
        assert_eq!(metric(&metrics, "cognitive_max"), Some(24.0));
        assert_eq!(metric(&metrics, "loc_total"), Some(812.0));
        assert_eq!(metric(&metrics, "maintainability_index_min"), Some(41.5));
        // Percentiles that are not part of the baseline set stay out of
        // it, so the snapshot does not grow by accident.
        assert!(!metrics.contains_key("cyclomatic_p95"), "got: {metrics:?}");
        assert!(
            !metrics.contains_key("cognitive_median"),
            "got: {metrics:?}",
        );
    }

    #[test]
    fn complexity_summary_omits_metrics_an_empty_scan_never_measured() {
        let report = json!({ "file_count": 0, "function_count": 0, "files": [] });
        let metrics = summarizer(ToolName::Complexity).unwrap()(&report);
        assert_eq!(metric(&metrics, "file_count"), Some(0.0));
        assert!(!metrics.contains_key("cognitive_max"), "got: {metrics:?}");
    }

    #[test]
    fn cohesion_summary_aggregates_lcom4_across_nested_units() {
        let report = json!({
            "file_count": 2,
            "unit_count": 3,
            "files": [
                { "units": [ { "lcom4": 1, "lcom96": null }, { "lcom4": 4 } ] },
                { "units": [ { "lcom4": 2 } ] },
            ],
        });
        let metrics = summarizer(ToolName::Cohesion).unwrap()(&report);
        assert_eq!(metric(&metrics, "unit_count"), Some(3.0));
        assert_eq!(metric(&metrics, "lcom4_max"), Some(4.0));
        // lcom4 of 4 and 2 are split units; the cohesive 1 is not.
        assert_eq!(metric(&metrics, "split_unit_count"), Some(2.0));
    }

    #[test]
    fn coupling_summary_takes_the_worst_module_on_each_axis() {
        let report = json!({
            "module_count": 3,
            "edge_count": 5,
            "cycle_count": 0,
            "modules": [
                { "fan_in": 0, "fan_out": 4, "ifc": 0, "instability": 1.0 },
                { "fan_in": 7, "fan_out": 1, "ifc": 49, "instability": 0.125 },
            ],
        });
        let metrics = summarizer(ToolName::Coupling).unwrap()(&report);
        assert_eq!(metric(&metrics, "module_count"), Some(3.0));
        assert_eq!(metric(&metrics, "cycle_count"), Some(0.0));
        assert_eq!(metric(&metrics, "fan_in_max"), Some(7.0));
        assert_eq!(metric(&metrics, "fan_out_max"), Some(4.0));
        assert_eq!(metric(&metrics, "ifc_max"), Some(49.0));
    }

    #[test]
    fn context_span_summary_reports_both_worst_and_total_span() {
        let report = json!({
            "module_count": 3,
            "modules": [
                { "transitive": 2 },
                { "transitive": 11 },
                { "transitive": 0 },
            ],
        });
        let metrics = summarizer(ToolName::ContextSpan).unwrap()(&report);
        assert_eq!(metric(&metrics, "transitive_max"), Some(11.0));
        assert_eq!(metric(&metrics, "transitive_sum"), Some(13.0));
    }

    #[test]
    fn hotspot_summary_reads_the_summary_block() {
        let report = json!({
            "file_count": 78,
            "summary": { "score_max": 144, "commits_max": 14, "cognitive_max": 24 },
            "files": [],
        });
        let metrics = summarizer(ToolName::Hotspot).unwrap()(&report);
        assert_eq!(metric(&metrics, "file_count"), Some(78.0));
        assert_eq!(metric(&metrics, "score_max"), Some(144.0));
        assert_eq!(metric(&metrics, "commits_max"), Some(14.0));
    }

    #[test]
    fn similarity_summary_counts_clustered_units_not_just_clusters() {
        let report = json!({
            "unit_count": 1849,
            "threshold": 0.85,
            "cluster_count": 2,
            "clusters": [ { "size": 8 }, { "size": 2 } ],
        });
        let metrics = summarizer(ToolName::Similarity).unwrap()(&report);
        assert_eq!(metric(&metrics, "unit_count"), Some(1849.0));
        assert_eq!(metric(&metrics, "cluster_count"), Some(2.0));
        assert_eq!(metric(&metrics, "clustered_unit_count"), Some(10.0));
        assert_eq!(metric(&metrics, "cluster_max_size"), Some(8.0));
        // Run settings are not metrics: a threshold change must not read
        // as a regression.
        assert!(!metrics.contains_key("threshold"), "got: {metrics:?}");
    }

    #[test]
    fn an_empty_but_present_collection_still_yields_its_totals() {
        // A clean run is a measurement, not a gap: the totals are zero
        // and recorded, while "the largest cluster" has no answer.
        let report = json!({ "unit_count": 40, "cluster_count": 0, "clusters": [] });
        let metrics = summarizer(ToolName::Similarity).unwrap()(&report);
        assert_eq!(metric(&metrics, "clustered_unit_count"), Some(0.0));
        assert!(
            !metrics.contains_key("cluster_max_size"),
            "got: {metrics:?}"
        );
    }

    #[test]
    fn unreadable_values_inside_a_collection_invent_no_extremes() {
        let report = json!({ "files": [ { "units": [ { "lcom4": "n/a" } ] } ] });
        let metrics = summarizer(ToolName::Cohesion).unwrap()(&report);
        assert!(!metrics.contains_key("lcom4_max"), "got: {metrics:?}");
    }

    #[test]
    fn a_missing_collection_yields_no_totals_at_all() {
        let report = json!({ "unit_count": 40, "cluster_count": 0 });
        let metrics = summarizer(ToolName::Similarity).unwrap()(&report);
        assert!(
            !metrics.contains_key("clustered_unit_count"),
            "got: {metrics:?}",
        );
    }

    #[rstest]
    #[case(ToolName::Complexity)]
    #[case(ToolName::Cohesion)]
    #[case(ToolName::Coupling)]
    #[case(ToolName::ContextSpan)]
    #[case(ToolName::Hotspot)]
    #[case(ToolName::Similarity)]
    fn summarized_tools_never_panic_on_a_report_shape_they_did_not_expect(#[case] tool: ToolName) {
        for report in [
            json!({}),
            json!({ "files": "not an array", "modules": 3, "clusters": null }),
            json!({ "file_count": "many", "summary": [] }),
        ] {
            // Nothing measurable is there, so nothing is claimed: a
            // shape the extractor does not recognise must produce no
            // metrics rather than zeroes a comparison would trust.
            let metrics = summarizer(tool).unwrap()(&report);
            assert!(metrics.is_empty(), "got: {metrics:?}");
        }
    }

    #[rstest]
    #[case(ToolName::Cycles)]
    #[case(ToolName::Delegation)]
    #[case(ToolName::FunctionGraph)]
    #[case(ToolName::GraphQuery)]
    #[case(ToolName::Hubs)]
    #[case(ToolName::Impact)]
    #[case(ToolName::Layers)]
    #[case(ToolName::Risk)]
    #[case(ToolName::Untested)]
    #[case(ToolName::Visibility)]
    #[case(ToolName::Wrapper)]
    fn tools_without_a_summary_are_reported_as_such(#[case] tool: ToolName) {
        assert!(summarizer(tool).is_none());
    }

    #[test]
    fn aggregates_keep_whole_numbers_whole() {
        let report = json!({ "modules": [ { "transitive": 2 }, { "transitive": 3 } ] });
        let metrics = summarizer(ToolName::ContextSpan).unwrap()(&report);
        // Exact, not a substring check: `5.0` would also contain `5`,
        // and a count rendered as a float is the bug this guards.
        assert_eq!(
            serde_json::to_string(&metrics).unwrap(),
            r#"{"transitive_max":3,"transitive_sum":5}"#,
        );
    }

    #[test]
    fn a_fractional_aggregate_keeps_its_fraction() {
        let report = json!({ "modules": [ { "transitive": 0.5 }, { "transitive": 1.0 } ] });
        let metrics = summarizer(ToolName::ContextSpan).unwrap()(&report);
        assert_eq!(metric(&metrics, "transitive_sum"), Some(1.5));
        assert_eq!(
            serde_json::to_string(&metrics).unwrap(),
            r#"{"transitive_max":1,"transitive_sum":1.5}"#,
        );
    }

    #[test]
    fn an_aggregate_past_i64_stays_a_float_instead_of_saturating() {
        // `as i64` saturates at `i64::MAX`, which would silently pin a
        // runaway total to a fixed number that looks like a real
        // measurement. Past that range the value stays a float.
        let report = json!({ "modules": [ { "transitive": 1e19 } ] });
        let metrics = summarizer(ToolName::ContextSpan).unwrap()(&report);
        assert_eq!(metric(&metrics, "transitive_sum"), Some(1e19));
        assert_eq!(metric(&metrics, "transitive_max"), Some(1e19));
    }

    #[test]
    fn render_round_trips_and_ends_with_one_newline() {
        let baseline = Baseline {
            schema_version: SCHEMA_VERSION,
            tool_version: "0.2.0".to_owned(),
            profile: "baseline".to_owned(),
            target: "crates/agent-lens".to_owned(),
            commit: Some("deadbeef".to_owned()),
            tools: vec![ToolBaseline {
                tool: "complexity".to_owned(),
                metrics: Metrics::from([("cognitive_max".to_owned(), Number::from(24))]),
            }],
            skipped: vec![SkippedTool {
                tool: "hubs".to_owned(),
                reason: NO_SUMMARY_REASON.to_owned(),
            }],
        };
        let rendered = baseline.render().unwrap();
        assert!(rendered.ends_with("}\n"), "got: {rendered}");
        assert!(!rendered.ends_with("}\n\n"), "got: {rendered}");
        assert_eq!(
            serde_json::from_str::<Baseline>(&rendered).unwrap(),
            baseline,
        );
    }

    #[test]
    fn empty_optional_sections_stay_out_of_the_document() {
        let baseline = Baseline {
            schema_version: SCHEMA_VERSION,
            tool_version: "0.2.0".to_owned(),
            profile: "baseline".to_owned(),
            target: "src".to_owned(),
            commit: None,
            tools: Vec::new(),
            skipped: Vec::new(),
        };
        let rendered = baseline.render().unwrap();
        assert!(!rendered.contains("commit"), "got: {rendered}");
        assert!(!rendered.contains("skipped"), "got: {rendered}");
        // …and the trimmed document still parses back.
        assert_eq!(
            serde_json::from_str::<Baseline>(&rendered).unwrap(),
            baseline,
        );
    }

    #[test]
    fn head_commit_reads_the_target_repository() {
        let dir = tempfile::tempdir().unwrap();
        crate::test_support::write_file(dir.path(), "src/lib.rs", "fn f() {}\n");
        crate::test_support::run_git(dir.path(), &["init", "-q", "-b", "main"]);
        crate::test_support::run_git(dir.path(), &["add", "."]);
        crate::test_support::run_git(dir.path(), &["commit", "-q", "-m", "init"]);

        let commit = head_commit(&dir.path().join("src")).unwrap();
        assert_eq!(commit.len(), 40, "got: {commit}");
        assert!(
            commit.chars().all(|c| c.is_ascii_hexdigit()),
            "got: {commit}"
        );
    }

    #[test]
    fn head_commit_is_absent_outside_a_git_tree() {
        let dir = tempfile::tempdir().unwrap();
        crate::test_support::write_file(dir.path(), "src/lib.rs", "fn f() {}\n");
        assert_eq!(head_commit(dir.path()), None);
    }

    /// Arbitrary JSON, weighted towards the shapes the extractors reach
    /// into: nested objects and arrays holding numbers, strings, and
    /// nulls. The extractors read reports produced by seventeen
    /// analyzers across four languages, and a report shape that shifts
    /// under them must degrade to "no metric" rather than to a panic or
    /// to a nonsense number.
    fn arbitrary_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i32>().prop_map(Value::from),
            (-1e6f64..1e6).prop_map(Value::from),
            "[a-z_]{0,8}".prop_map(Value::from),
        ];
        leaf.prop_recursive(4, 32, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::from),
                prop::collection::hash_map(
                    prop::sample::select(vec![
                        "files",
                        "units",
                        "modules",
                        "clusters",
                        "summary",
                        "lcom4",
                        "size",
                        "transitive",
                        "fan_in",
                        "file_count",
                        "cognitive_max",
                    ]),
                    inner,
                    0..5,
                )
                .prop_map(|map| {
                    Value::Object(
                        map.into_iter()
                            .map(|(key, value)| (key.to_owned(), value))
                            .collect(),
                    )
                }),
            ]
        })
    }

    proptest! {
        /// Whatever the report looks like, a summary is finite,
        /// serializable JSON — a `NaN` or an infinity would render as
        /// `null` and quietly turn a metric into a hole.
        #[test]
        fn summaries_are_always_finite_over_arbitrary_reports(report in arbitrary_json()) {
            for tool in SUMMARIZED {
                let metrics = summarizer(tool).unwrap()(&report);
                for (name, value) in &metrics {
                    prop_assert!(
                        value.as_f64().is_some_and(f64::is_finite),
                        "{name} is not a finite number: {value}",
                    );
                }
                prop_assert!(serde_json::to_string(&metrics).is_ok());
            }
        }

        /// A maximum must come from the population it summarises: it is
        /// present only when there were values, and then it is one of
        /// them.
        #[test]
        fn extremes_are_drawn_from_the_reported_values(
            spans in prop::collection::vec(0i64..1_000, 0..16),
        ) {
            let report = json!({
                "modules": spans.iter().map(|s| json!({ "transitive": s })).collect::<Vec<_>>(),
            });
            let metrics = summarizer(ToolName::ContextSpan).unwrap()(&report);
            match metric(&metrics, "transitive_max") {
                Some(max) => prop_assert!(spans.contains(&(max as i64))),
                None => prop_assert!(spans.is_empty()),
            }
            // The total is measured whenever the collection was, empty
            // or not.
            prop_assert_eq!(
                metric(&metrics, "transitive_sum"),
                Some(spans.iter().sum::<i64>() as f64),
            );
        }
    }
}
