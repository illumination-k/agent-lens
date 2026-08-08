//! Comparing a stored snapshot against a fresh one — the ratchet.
//!
//! [`super`] answers "what do these metrics read today". This module
//! answers the question a check is actually made of: *did this change
//! make anything worse than it already was*. That is the whole point of
//! storing a snapshot — a repository can adopt a threshold without first
//! paying off its existing debt, because the threshold is "no worse than
//! the last commit" rather than an absolute number nobody can hit yet.
//!
//! Three decisions shape everything below.
//!
//! * **A metric only gates if it has a direction.** A snapshot mixes
//!   quality figures with figures that merely size the measured surface,
//!   and only the first kind can regress. See [`direction`].
//! * **An absent metric is never a verdict.** A metric the baseline
//!   carries and this run does not is reported as `missing`, not as an
//!   improvement to zero; the reverse is `new`, not a regression from
//!   zero. Both are facts about coverage, not about the code.
//! * **The ratchet never loosens.** [`ratchet`] writes improvements back
//!   into the stored snapshot and keeps the stored value wherever this
//!   run was worse, so a regression cannot be laundered into the new
//!   baseline by re-running with `--update`.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Number;

use super::{Baseline, Metrics, ToolBaseline, number_from_f64};
use crate::analyze::OutputFormat;

/// Exit status for "the comparison ran, and it found regressions".
///
/// Distinct from the `1` every other failure exits with: a CI step needs
/// to tell "the code got worse" from "the tool could not run", and the
/// two call for different responses.
pub const REGRESSION_EXIT_CODE: u8 = 2;

/// Which way a metric has to move to be worse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// The common case: an extreme or a total that a change should not
    /// grow.
    LowerIsBetter,
    /// A floor, worse when it falls.
    HigherIsBetter,
    /// Not a quality figure at all — recorded, reported when it moves,
    /// never gated.
    Context,
}

/// How `metric` is judged.
///
/// Two families are deliberately [`Direction::Context`] rather than
/// gates:
///
/// * **Surface size** (`file_count`, `function_count`, `unit_count`,
///   `module_count`, `edge_count`, `loc_total`). A growing codebase moves
///   all of them without anything getting worse, so gating on them would
///   only mean "no new code".
/// * **History** (`commits_max`, `score_max`). Hotspot churn accumulates
///   with every commit and never falls, and the hotspot score is that
///   churn multiplied by complexity. A ratchet on either would fail on
///   the next commit to the hottest file and keep failing — a check that
///   cannot be satisfied by improving the code is not a check.
///
/// Metric names are shared across analyzers on purpose (`cognitive_max`
/// means the same thing in `complexity` and `hotspot`), so the table is
/// keyed by name alone.
pub fn direction(metric: &str) -> Direction {
    classified(metric).unwrap_or(Direction::LowerIsBetter)
}

/// The explicit half of [`direction`], kept separate so a test can assert
/// that every metric a summarizer emits is classified on purpose.
///
/// An unclassified name falls back to `LowerIsBetter` rather than
/// `Context`: a new metric that gates when it should not is a failing
/// build somebody investigates, while one that silently never gates is a
/// check quietly doing nothing.
fn classified(metric: &str) -> Option<Direction> {
    Some(match metric {
        "file_count" | "function_count" | "unit_count" | "module_count" | "edge_count"
        | "loc_total" | "commits_max" | "score_max" => Direction::Context,
        "maintainability_index_min" => Direction::HigherIsBetter,
        "cyclomatic_max"
        | "cognitive_max"
        | "cognitive_p95"
        | "max_nesting_max"
        | "lcom4_max"
        | "split_unit_count"
        | "cycle_count"
        | "fan_in_max"
        | "fan_out_max"
        | "ifc_max"
        | "transitive_max"
        | "transitive_sum"
        | "cluster_count"
        | "clustered_unit_count"
        | "cluster_max_size" => Direction::LowerIsBetter,
        _ => return None,
    })
}

/// What happened to one metric between the two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Moved the wrong way on a gated metric. The only failing verdict.
    Regressed,
    /// Moved the right way on a gated metric.
    Improved,
    /// Identical in both snapshots.
    Held,
    /// A context metric that changed — reported so a reader can see the
    /// codebase grew, never gated.
    Moved,
    /// This run measured it; the baseline did not.
    New,
    /// The baseline carries it; this run did not measure it.
    Missing,
}

/// Why two snapshots cannot be compared at all.
#[derive(Debug, thiserror::Error)]
pub enum CompareError {
    /// Field meanings changed between schema versions, so reading the old
    /// document against the new one would compare different quantities.
    #[error(
        "baseline schema version {found} is not the {expected} this build understands; \
         regenerate it with `agent-lens baseline create`"
    )]
    SchemaMismatch { found: u32, expected: u32 },
    /// Profiles differ in tools, filters, and target, so their metrics
    /// are not the same measurement under the same name.
    #[error("baseline was taken for profile {baseline:?}, but this run is profile {current:?}")]
    ProfileMismatch { baseline: String, current: String },
    #[error("failed to serialize comparison: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// One side of the comparison, as the snapshot described itself.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapshotRef {
    /// Analyzed path, as written in the profile.
    pub target: String,
    /// Commit the snapshot describes, when it could name one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// `agent-lens` version that produced it. Metric definitions can
    /// change between releases, so two versions here is a caveat on the
    /// whole comparison.
    pub tool_version: String,
}

impl SnapshotRef {
    fn of(baseline: &Baseline) -> Self {
        Self {
            target: baseline.target.clone(),
            commit: baseline.commit.clone(),
            tool_version: baseline.tool_version.clone(),
        }
    }
}

/// One metric, on both sides.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricComparison {
    pub metric: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<Number>,
    /// `current - baseline`, present only when both sides are.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<Number>,
    pub direction: Direction,
    pub verdict: Verdict,
}

/// One analyzer's metrics, in name order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolComparison {
    pub tool: String,
    pub metrics: Vec<MetricComparison>,
}

/// How many metrics landed on each verdict.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ComparisonSummary {
    pub regressed: usize,
    pub improved: usize,
    pub held: usize,
    pub moved: usize,
    pub new: usize,
    pub missing: usize,
}

impl ComparisonSummary {
    fn record(&mut self, verdict: Verdict) {
        let slot = match verdict {
            Verdict::Regressed => &mut self.regressed,
            Verdict::Improved => &mut self.improved,
            Verdict::Held => &mut self.held,
            Verdict::Moved => &mut self.moved,
            Verdict::New => &mut self.new,
            Verdict::Missing => &mut self.missing,
        };
        *slot += 1;
    }
}

/// A stored snapshot measured against a fresh one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    pub profile: String,
    pub baseline: SnapshotRef,
    pub current: SnapshotRef,
    pub summary: ComparisonSummary,
    /// Every metric on both sides, in the fresh run's tool order, with
    /// baseline-only tools appended. JSON carries all of them; the
    /// markdown report leads with what moved.
    pub tools: Vec<ToolComparison>,
}

impl Comparison {
    /// Whether anything gated moved the wrong way — the check's answer.
    pub fn regressed(&self) -> bool {
        self.summary.regressed > 0
    }

    pub fn render(&self, format: OutputFormat) -> Result<String, CompareError> {
        match format {
            OutputFormat::Json => Ok(serde_json::to_string_pretty(self)?),
            OutputFormat::Md => Ok(self.render_md()),
        }
    }

    /// Every metric with `verdict`, paired with the tool that owns it.
    fn with_verdict(&self, verdict: Verdict) -> Vec<(&str, &MetricComparison)> {
        self.tools
            .iter()
            .flat_map(|tool| {
                tool.metrics
                    .iter()
                    .filter(move |metric| metric.verdict == verdict)
                    .map(move |metric| (tool.tool.as_str(), metric))
            })
            .collect()
    }

    fn render_md(&self) -> String {
        let mut out = format!("# Baseline compare: {}\n\n", self.profile);
        let _ = writeln!(
            &mut out,
            "- baseline: {} (agent-lens {}, {})",
            describe_commit(self.baseline.commit.as_deref()),
            self.baseline.tool_version,
            self.baseline.target,
        );
        let _ = writeln!(
            &mut out,
            "- current: {} (agent-lens {}, {})",
            describe_commit(self.current.commit.as_deref()),
            self.current.tool_version,
            self.current.target,
        );
        let summary = &self.summary;
        let _ = writeln!(
            &mut out,
            "- {} regressed, {} improved, {} held, {} context moved, {} new, {} not measured",
            summary.regressed,
            summary.improved,
            summary.held,
            summary.moved,
            summary.new,
            summary.missing,
        );

        // Regressions first and unconditionally: an empty section is the
        // answer a reader came for, and a report that omits it reads like
        // the check never ran.
        let regressed = self.with_verdict(Verdict::Regressed);
        out.push_str("\n## Regressed\n\n");
        if regressed.is_empty() {
            out.push_str("_Nothing regressed against the baseline._\n");
        } else {
            render_change_table(&mut out, &regressed);
        }

        for (title, verdict) in [
            ("Improved", Verdict::Improved),
            ("Context moved (not gated)", Verdict::Moved),
        ] {
            let rows = self.with_verdict(verdict);
            if rows.is_empty() {
                continue;
            }
            let _ = writeln!(&mut out, "\n## {title}\n");
            render_change_table(&mut out, &rows);
        }

        for (title, verdict, column) in [
            ("Not measured this run", Verdict::Missing, "baseline"),
            ("New in this run", Verdict::New, "current"),
        ] {
            let rows = self.with_verdict(verdict);
            if rows.is_empty() {
                continue;
            }
            let _ = writeln!(&mut out, "\n## {title}\n");
            let _ = writeln!(&mut out, "| tool | metric | {column} |");
            let _ = writeln!(&mut out, "| --- | --- | ---: |");
            for (tool, metric) in rows {
                let _ = writeln!(
                    &mut out,
                    "| {tool} | {} | {} |",
                    metric.metric,
                    render_number(metric.baseline.as_ref().or(metric.current.as_ref())),
                );
            }
        }
        out
    }
}

fn render_change_table(out: &mut String, rows: &[(&str, &MetricComparison)]) {
    let _ = writeln!(out, "| tool | metric | baseline | current | delta |");
    let _ = writeln!(out, "| --- | --- | ---: | ---: | ---: |");
    for (tool, metric) in rows {
        let _ = writeln!(
            out,
            "| {tool} | {} | {} | {} | {} |",
            metric.metric,
            render_number(metric.baseline.as_ref()),
            render_number(metric.current.as_ref()),
            render_delta(metric.delta.as_ref()),
        );
    }
}

fn describe_commit(commit: Option<&str>) -> String {
    // Short hashes are what a reader recognises, and the full one is in
    // the JSON for anything that needs to check out the tree.
    commit.map_or_else(
        || "no commit".to_owned(),
        |commit| commit.chars().take(12).collect(),
    )
}

fn render_number(value: Option<&Number>) -> String {
    value.map_or_else(|| "-".to_owned(), Number::to_string)
}

/// Deltas carry an explicit sign, so a column of them reads as movement
/// rather than as a second set of values.
fn render_delta(delta: Option<&Number>) -> String {
    delta.map_or_else(
        || "-".to_owned(),
        |delta| {
            let rendered = delta.to_string();
            if rendered.starts_with('-') {
                rendered
            } else {
                format!("+{rendered}")
            }
        },
    )
}

/// Measure `current` against the stored `baseline`.
///
/// Both snapshots must describe the same profile at the same schema
/// version; anything else is a mistake worth reporting rather than a
/// comparison worth trusting. A differing `tool_version` or `target` is
/// *not* rejected — both are recorded on the report so the caller can
/// warn — because a version bump that leaves the metrics alone is the
/// common case and refusing it would make upgrades painful.
pub fn compare(baseline: &Baseline, current: &Baseline) -> Result<Comparison, CompareError> {
    if baseline.schema_version != current.schema_version {
        return Err(CompareError::SchemaMismatch {
            found: baseline.schema_version,
            expected: current.schema_version,
        });
    }
    if baseline.profile != current.profile {
        return Err(CompareError::ProfileMismatch {
            baseline: baseline.profile.clone(),
            current: current.profile.clone(),
        });
    }

    let mut summary = ComparisonSummary::default();
    let mut tools = Vec::new();
    for tool in tool_order(baseline, current) {
        let stored = metrics_for(baseline, &tool);
        let fresh = metrics_for(current, &tool);
        let mut metrics = Vec::new();
        for name in metric_names(stored, fresh) {
            let comparison = compare_metric(
                &name,
                stored.and_then(|m| m.get(&name)),
                fresh.and_then(|m| m.get(&name)),
            );
            summary.record(comparison.verdict);
            metrics.push(comparison);
        }
        tools.push(ToolComparison { tool, metrics });
    }

    Ok(Comparison {
        profile: current.profile.clone(),
        baseline: SnapshotRef::of(baseline),
        current: SnapshotRef::of(current),
        summary,
        tools,
    })
}

/// The fresh run's tools in their profile order, with tools only the
/// baseline knows appended — dropping those would hide a tool that
/// vanished from the profile, which is exactly when a gate stops gating.
fn tool_order(baseline: &Baseline, current: &Baseline) -> Vec<String> {
    let mut tools: Vec<String> = current.tools.iter().map(|t| t.tool.clone()).collect();
    for tool in &baseline.tools {
        if !tools.contains(&tool.tool) {
            tools.push(tool.tool.clone());
        }
    }
    tools
}

fn metrics_for<'a>(baseline: &'a Baseline, tool: &str) -> Option<&'a Metrics> {
    baseline
        .tools
        .iter()
        .find(|entry| entry.tool == tool)
        .map(|entry| &entry.metrics)
}

/// The union of both sides' metric names, in sorted order — a metric
/// present on one side only still gets a row.
fn metric_names(stored: Option<&Metrics>, fresh: Option<&Metrics>) -> Vec<String> {
    stored
        .into_iter()
        .chain(fresh)
        .flat_map(|metrics| metrics.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn compare_metric(
    name: &str,
    baseline: Option<&Number>,
    current: Option<&Number>,
) -> MetricComparison {
    let direction = direction(name);
    let delta = match (
        baseline.and_then(Number::as_f64),
        current.and_then(Number::as_f64),
    ) {
        (Some(before), Some(after)) => number_from_f64(after - before),
        _ => None,
    };
    MetricComparison {
        metric: name.to_owned(),
        baseline: baseline.cloned(),
        current: current.cloned(),
        delta,
        direction,
        verdict: verdict(direction, baseline, current),
    }
}

fn verdict(direction: Direction, baseline: Option<&Number>, current: Option<&Number>) -> Verdict {
    let (Some(before), Some(after)) = (baseline, current) else {
        // A name reaches this function from the union of both sides, so
        // exactly one of these holds when the pair is incomplete.
        return if current.is_some() {
            Verdict::New
        } else {
            Verdict::Missing
        };
    };
    if before.as_f64() == after.as_f64() {
        return Verdict::Held;
    }
    match direction {
        Direction::Context => Verdict::Moved,
        _ if is_worse(direction, before, after) => Verdict::Regressed,
        _ => Verdict::Improved,
    }
}

/// Whether `after` is worse than `before` on a metric with `direction`.
///
/// A value that cannot be read as a number is not worse than anything:
/// an unreadable metric is a gap in the snapshot, and a gate must not
/// fire on one.
fn is_worse(direction: Direction, before: &Number, after: &Number) -> bool {
    let (Some(before), Some(after)) = (before.as_f64(), after.as_f64()) else {
        return false;
    };
    match direction {
        Direction::LowerIsBetter => after > before,
        Direction::HigherIsBetter => after < before,
        Direction::Context => false,
    }
}

/// Tighten `baseline` with everything `current` did better — the ratchet
/// itself.
///
/// Monotone by construction: a gated metric takes the better of the two
/// values, so re-running with `--update` after a regression cannot
/// rewrite the bar downwards. What the fresh run *does* own outright:
///
/// * **Context metrics.** Nothing gates on them, so tracking the truth is
///   more useful than holding an old number.
/// * **The header.** Commit, tool version, target, and the skipped-tool
///   list describe the run that produced the file.
///
/// Metrics and whole tools the fresh run did not measure are carried over
/// untouched. Dropping them would quietly retire a gate the moment an
/// analyzer failed to produce its summary; a tool deliberately removed
/// from the profile is retired with a fresh `baseline create` instead.
pub fn ratchet(baseline: &Baseline, current: &Baseline) -> Baseline {
    let mut tools = Vec::new();
    for tool in tool_order(baseline, current) {
        let stored = metrics_for(baseline, &tool);
        let fresh = metrics_for(current, &tool);
        let mut metrics = stored.cloned().unwrap_or_default();
        for name in metric_names(stored, fresh) {
            let Some(after) = fresh.and_then(|m| m.get(&name)) else {
                continue;
            };
            let keep_stored = metrics
                .get(&name)
                .is_some_and(|before| is_worse(direction(&name), before, after));
            if !keep_stored {
                metrics.insert(name, after.clone());
            }
        }
        tools.push(ToolBaseline { tool, metrics });
    }

    Baseline {
        schema_version: current.schema_version,
        tool_version: current.tool_version.clone(),
        profile: current.profile.clone(),
        target: current.target.clone(),
        commit: current.commit.clone(),
        tools,
        skipped: current.skipped.clone(),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use rstest::rstest;
    use serde_json::json;

    use super::*;
    use crate::baseline::{SCHEMA_VERSION, summarizer};
    use crate::config::ToolName;

    /// Every analyzer a snapshot covers, with a report carrying every
    /// field its summarizer reads.
    const SUMMARIZED: [ToolName; 6] = [
        ToolName::Complexity,
        ToolName::Cohesion,
        ToolName::Coupling,
        ToolName::ContextSpan,
        ToolName::Hotspot,
        ToolName::Similarity,
    ];

    fn full_report() -> serde_json::Value {
        json!({
            "file_count": 4,
            "function_count": 31,
            "unit_count": 9,
            "module_count": 3,
            "edge_count": 5,
            "cycle_count": 1,
            "cluster_count": 2,
            "summary": {
                "cyclomatic_max": 16,
                "cognitive_max": 24,
                "cognitive_p95": 9,
                "max_nesting_max": 5,
                "loc_total": 812,
                "maintainability_index_min": 41.5,
                "score_max": 144,
                "commits_max": 14,
            },
            "files": [{ "units": [{ "lcom4": 3 }] }],
            "modules": [{ "fan_in": 7, "fan_out": 4, "ifc": 49, "transitive": 11 }],
            "clusters": [{ "size": 8 }, { "size": 2 }],
        })
    }

    fn snapshot(tool: &str, metrics: &[(&str, i64)]) -> Baseline {
        Baseline {
            schema_version: SCHEMA_VERSION,
            tool_version: "0.2.0".to_owned(),
            profile: "audit".to_owned(),
            target: "src".to_owned(),
            commit: None,
            tools: vec![ToolBaseline {
                tool: tool.to_owned(),
                metrics: metrics
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), Number::from(*value)))
                    .collect(),
            }],
            skipped: Vec::new(),
        }
    }

    fn verdict_of(comparison: &Comparison, metric: &str) -> Option<Verdict> {
        comparison
            .tools
            .iter()
            .flat_map(|tool| &tool.metrics)
            .find(|entry| entry.metric == metric)
            .map(|entry| entry.verdict)
    }

    fn metric_of(baseline: &Baseline, tool: &str, metric: &str) -> Option<f64> {
        metrics_for(baseline, tool)
            .and_then(|metrics| metrics.get(metric))
            .and_then(Number::as_f64)
    }

    /// The direction table is the whole gate: a metric that reaches it
    /// unclassified is judged by a fallback nobody chose for it.
    #[test]
    fn every_summarized_metric_is_classified_on_purpose() {
        let report = full_report();
        for tool in SUMMARIZED {
            let metrics = summarizer(tool).unwrap()(&report);
            assert!(!metrics.is_empty(), "{} produced nothing", tool.as_str());
            for name in metrics.keys() {
                assert!(
                    classified(name).is_some(),
                    "{name} ({}) has no direction",
                    tool.as_str(),
                );
            }
        }
    }

    #[rstest]
    #[case::worse_extreme("cognitive_max", 24, 31, Verdict::Regressed)]
    #[case::better_extreme("cognitive_max", 24, 20, Verdict::Improved)]
    #[case::unchanged("cognitive_max", 24, 24, Verdict::Held)]
    // A floor is the one metric that regresses by falling.
    #[case::floor_falls("maintainability_index_min", 42, 30, Verdict::Regressed)]
    #[case::floor_rises("maintainability_index_min", 42, 55, Verdict::Improved)]
    // Surface size and git history move on their own; neither gates.
    #[case::surface_grows("function_count", 31, 44, Verdict::Moved)]
    #[case::churn_grows("commits_max", 14, 15, Verdict::Moved)]
    #[case::hotspot_score_grows("score_max", 144, 180, Verdict::Moved)]
    fn a_metric_is_judged_by_its_direction(
        #[case] metric: &str,
        #[case] before: i64,
        #[case] after: i64,
        #[case] expected: Verdict,
    ) {
        let comparison = compare(
            &snapshot("complexity", &[(metric, before)]),
            &snapshot("complexity", &[(metric, after)]),
        )
        .unwrap();
        assert_eq!(verdict_of(&comparison, metric), Some(expected));
        assert_eq!(comparison.regressed(), expected == Verdict::Regressed);
    }

    #[test]
    fn a_metric_only_one_side_carries_is_coverage_not_movement() {
        let comparison = compare(
            &snapshot(
                "complexity",
                &[("cognitive_max", 24), ("cyclomatic_max", 9)],
            ),
            &snapshot("complexity", &[("cognitive_max", 24), ("lcom4_max", 3)]),
        )
        .unwrap();
        // Neither counts as a regression: an unmeasured metric is a gap
        // in coverage, and a new one has nothing to be measured against.
        assert_eq!(
            verdict_of(&comparison, "cyclomatic_max"),
            Some(Verdict::Missing)
        );
        assert_eq!(verdict_of(&comparison, "lcom4_max"), Some(Verdict::New));
        assert!(!comparison.regressed());
        assert_eq!(comparison.summary.missing, 1);
        assert_eq!(comparison.summary.new, 1);
    }

    #[test]
    fn deltas_are_reported_only_where_both_sides_have_a_value() {
        let comparison = compare(
            &snapshot(
                "complexity",
                &[("cognitive_max", 24), ("cyclomatic_max", 9)],
            ),
            &snapshot("complexity", &[("cognitive_max", 31)]),
        )
        .unwrap();
        let metrics = &comparison.tools[0].metrics;
        let cognitive = metrics
            .iter()
            .find(|m| m.metric == "cognitive_max")
            .unwrap();
        assert_eq!(cognitive.delta.as_ref().and_then(Number::as_f64), Some(7.0));
        let cyclomatic = metrics
            .iter()
            .find(|m| m.metric == "cyclomatic_max")
            .unwrap();
        assert_eq!(cyclomatic.delta, None);
    }

    /// A tool that vanished from the profile keeps its row: silently
    /// dropping it is how a gate stops gating without anyone noticing.
    #[test]
    fn a_tool_the_fresh_run_lacks_is_still_reported() {
        let baseline = snapshot("cohesion", &[("lcom4_max", 4)]);
        let current = snapshot("complexity", &[("cognitive_max", 24)]);
        let comparison = compare(&baseline, &current).unwrap();
        assert_eq!(
            comparison
                .tools
                .iter()
                .map(|tool| tool.tool.as_str())
                .collect::<Vec<_>>(),
            ["complexity", "cohesion"],
        );
        assert_eq!(verdict_of(&comparison, "lcom4_max"), Some(Verdict::Missing));
    }

    #[test]
    fn a_snapshot_of_another_profile_is_refused() {
        let mut baseline = snapshot("complexity", &[("cognitive_max", 24)]);
        baseline.profile = "web".to_owned();
        let err = compare(&baseline, &snapshot("complexity", &[("cognitive_max", 24)]))
            .expect_err("profiles differ");
        assert!(
            matches!(err, CompareError::ProfileMismatch { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn a_snapshot_from_another_schema_is_refused() {
        let mut baseline = snapshot("complexity", &[("cognitive_max", 24)]);
        baseline.schema_version = SCHEMA_VERSION + 1;
        let err = compare(&baseline, &snapshot("complexity", &[("cognitive_max", 24)]))
            .expect_err("schema versions differ");
        assert!(
            matches!(err, CompareError::SchemaMismatch { .. }),
            "got: {err}"
        );
    }

    /// A version bump is a caveat a reader needs, not a reason to refuse
    /// the comparison.
    #[test]
    fn a_snapshot_from_another_tool_version_still_compares() {
        let mut baseline = snapshot("complexity", &[("cognitive_max", 24)]);
        baseline.tool_version = "0.1.0".to_owned();
        let comparison = compare(&baseline, &snapshot("complexity", &[("cognitive_max", 24)]))
            .expect("versions are recorded, not enforced");
        assert_eq!(comparison.baseline.tool_version, "0.1.0");
        assert_eq!(comparison.current.tool_version, "0.2.0");
    }

    #[test]
    fn ratchet_takes_the_improvement_and_keeps_the_stricter_bar() {
        let baseline = snapshot(
            "complexity",
            &[("cognitive_max", 24), ("cyclomatic_max", 9)],
        );
        let current = snapshot(
            "complexity",
            &[("cognitive_max", 31), ("cyclomatic_max", 6)],
        );
        let tightened = ratchet(&baseline, &current);
        // Worse than stored: the bar stays where it was.
        assert_eq!(
            metric_of(&tightened, "complexity", "cognitive_max"),
            Some(24.0)
        );
        // Better than stored: the bar moves down.
        assert_eq!(
            metric_of(&tightened, "complexity", "cyclomatic_max"),
            Some(6.0)
        );
    }

    #[test]
    fn ratchet_tracks_context_metrics_and_the_fresh_header() {
        let baseline = snapshot("complexity", &[("function_count", 31)]);
        let mut current = snapshot("complexity", &[("function_count", 44)]);
        current.commit = Some("deadbeef".to_owned());
        let tightened = ratchet(&baseline, &current);
        // Nothing gates on it, so the snapshot records what is true now.
        assert_eq!(
            metric_of(&tightened, "complexity", "function_count"),
            Some(44.0)
        );
        assert_eq!(tightened.commit.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn ratchet_keeps_what_the_fresh_run_never_measured() {
        let baseline = snapshot("cohesion", &[("lcom4_max", 4)]);
        let current = snapshot("complexity", &[("cognitive_max", 24)]);
        let tightened = ratchet(&baseline, &current);
        assert_eq!(metric_of(&tightened, "cohesion", "lcom4_max"), Some(4.0));
        assert_eq!(
            metric_of(&tightened, "complexity", "cognitive_max"),
            Some(24.0)
        );
    }

    #[test]
    fn md_report_leads_with_regressions_and_names_the_metric() {
        let comparison = compare(
            &snapshot(
                "complexity",
                &[("cognitive_max", 24), ("function_count", 31)],
            ),
            &snapshot(
                "complexity",
                &[("cognitive_max", 31), ("function_count", 44)],
            ),
        )
        .unwrap();
        let md = comparison.render(OutputFormat::Md).unwrap();
        assert!(md.contains("# Baseline compare: audit"), "got: {md}");
        assert!(md.contains("## Regressed"), "got: {md}");
        assert!(
            md.contains("| complexity | cognitive_max | 24 | 31 | +7 |"),
            "got: {md}"
        );
        // A context move is reported under its own heading, never as a
        // regression.
        assert!(md.contains("## Context moved (not gated)"), "got: {md}");
        assert!(
            md.contains("| complexity | function_count | 31 | 44 | +13 |"),
            "got: {md}"
        );
    }

    #[test]
    fn md_report_says_so_when_nothing_regressed() {
        let comparison = compare(
            &snapshot("complexity", &[("cognitive_max", 24)]),
            &snapshot("complexity", &[("cognitive_max", 20)]),
        )
        .unwrap();
        let md = comparison.render(OutputFormat::Md).unwrap();
        assert!(
            md.contains("_Nothing regressed against the baseline._"),
            "got: {md}"
        );
        assert!(
            md.contains("| complexity | cognitive_max | 24 | 20 | -4 |"),
            "got: {md}"
        );
    }

    #[test]
    fn json_report_carries_every_metric_with_its_direction() {
        let comparison = compare(
            &snapshot("complexity", &[("cognitive_max", 24)]),
            &snapshot("complexity", &[("cognitive_max", 31)]),
        )
        .unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&comparison.render(OutputFormat::Json).unwrap()).unwrap();
        assert_eq!(value["profile"], "audit");
        assert_eq!(value["summary"]["regressed"], 1);
        let metric = &value["tools"][0]["metrics"][0];
        assert_eq!(metric["metric"], "cognitive_max");
        assert_eq!(metric["direction"], "lower-is-better");
        assert_eq!(metric["verdict"], "regressed");
        assert_eq!(metric["delta"], 7);
    }

    proptest! {
        /// The ratchet is monotone: whatever this run measured, a gated
        /// metric present in both snapshots never ends up worse than the
        /// stored one. This is what stops `--update` from laundering a
        /// regression into the new bar.
        #[test]
        fn ratcheting_never_loosens_a_gated_metric(
            before in 0i64..1_000,
            after in 0i64..1_000,
            floor_before in 0i64..1_000,
            floor_after in 0i64..1_000,
        ) {
            let baseline = snapshot(
                "complexity",
                &[("cognitive_max", before), ("maintainability_index_min", floor_before)],
            );
            let current = snapshot(
                "complexity",
                &[("cognitive_max", after), ("maintainability_index_min", floor_after)],
            );
            let tightened = ratchet(&baseline, &current);
            prop_assert_eq!(
                metric_of(&tightened, "complexity", "cognitive_max"),
                Some(before.min(after) as f64),
            );
            prop_assert_eq!(
                metric_of(&tightened, "complexity", "maintainability_index_min"),
                Some(floor_before.max(floor_after) as f64),
            );
        }

        /// And the fixed point that follows: comparing a tightened
        /// snapshot against the run it was built from can only ever
        /// report improvements the ratchet already absorbed — never a
        /// regression.
        #[test]
        fn a_tightened_snapshot_never_regresses_against_its_own_run(
            before in 0i64..1_000,
            after in 0i64..1_000,
        ) {
            let baseline = snapshot("complexity", &[("cognitive_max", before)]);
            let current = snapshot("complexity", &[("cognitive_max", after)]);
            let tightened = ratchet(&baseline, &current);
            let comparison = compare(&tightened, &tightened).unwrap();
            prop_assert!(!comparison.regressed());
        }
    }
}
