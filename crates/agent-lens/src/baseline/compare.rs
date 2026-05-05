//! Pure regression-detection over two [`Snapshot`]s.
//!
//! Kept in a dedicated module — and dependency-free except for the
//! shared types in [`super`] — so the policy logic can be exhaustively
//! unit-tested without touching the filesystem, `git`, or any analyzer
//! state. The CLI / runner layer wires this into the user-visible exit
//! code.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::{FailOn, Item, ItemId, NewItemPolicy, Snapshot};

/// Outcome of [`compare`]: every regression that should be visible to
/// the user, plus parallel listings for the other diff buckets so an
/// agent can see the full picture in one JSON payload.
#[derive(Debug, Clone, Serialize)]
pub struct RegressionReport {
    pub analyzer: String,
    pub baseline_path: String,
    pub summary: ReportSummary,
    pub regressions: Vec<Regression>,
    pub improvements: Vec<Regression>,
    pub new_items: Vec<Item>,
    pub removed_items: Vec<Item>,
}

impl RegressionReport {
    /// True when the report should flip the exit code under `fail_on`.
    /// `extras`-based regressions (e.g. new coupling cycles) are
    /// classified as `RegressionKind::NewCycle` and counted under
    /// "regressions" for fail-on purposes.
    pub fn has_failures(&self, fail_on: FailOn) -> bool {
        let regressed = self
            .regressions
            .iter()
            .any(|r| !matches!(r.kind, RegressionKind::New));
        let new = self
            .regressions
            .iter()
            .any(|r| matches!(r.kind, RegressionKind::New));
        match fail_on {
            FailOn::Any => regressed || new,
            FailOn::Regression => regressed,
            FailOn::NewItem => new,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ReportSummary {
    pub regressed: usize,
    pub new_items: usize,
    pub improved: usize,
    pub unchanged: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Regression {
    pub id: ItemId,
    pub kind: RegressionKind,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub location: BTreeMap<String, serde_json::Value>,
    pub deltas: Vec<MetricDelta>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RegressionKind {
    /// An existing item's metric got worse.
    Worsened,
    /// An item not present in the baseline appeared in the current
    /// run, and policy classified it as new debt.
    New,
    /// `extras`-only regression — currently used for new coupling
    /// dependency cycles.
    NewCycle,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricDelta {
    pub metric: String,
    /// `None` for new items (no baseline value to compare against).
    pub baseline: Option<f64>,
    pub current: f64,
    pub delta: f64,
}

/// Per-call ratchet configuration. Built by the CLI runner from the
/// `--metric`, `--new-item-policy`, and `--fail-on` flags plus the
/// adapter's defaults.
#[derive(Debug, Clone)]
pub struct RatchetConfig {
    /// Metrics to compare for existing items. All metrics in this list
    /// are treated as "lower is better".
    pub metrics: Vec<String>,
    /// Metric the new-item policy reads from each new item.
    pub primary_metric: String,
    /// Threshold under [`NewItemPolicy::Threshold`] for the primary
    /// metric. `None` means "no threshold" — under that policy, no new
    /// item triggers.
    pub new_item_threshold: Option<f64>,
    pub policy: NewItemPolicy,
}

/// Diff `current` against `baseline`. Pure: no I/O, no clock.
pub fn compare(
    baseline: &Snapshot,
    current: &Snapshot,
    baseline_path: &str,
    cfg: &RatchetConfig,
) -> RegressionReport {
    let baseline_by_id: BTreeMap<&ItemId, &Item> =
        baseline.items.iter().map(|i| (&i.id, i)).collect();
    let current_by_id: BTreeMap<&ItemId, &Item> =
        current.items.iter().map(|i| (&i.id, i)).collect();

    let mut regressions = Vec::new();
    let mut improvements = Vec::new();
    let mut new_items = Vec::new();
    let mut removed_items = Vec::new();
    let mut unchanged = 0;

    for (id, cur) in &current_by_id {
        match baseline_by_id.get(id) {
            Some(base) => {
                let (worse, better) = diff_metrics(&base.metrics, &cur.metrics, &cfg.metrics);
                if !worse.is_empty() {
                    regressions.push(Regression {
                        id: (*id).clone(),
                        kind: RegressionKind::Worsened,
                        location: cur.location.clone(),
                        deltas: worse,
                    });
                } else if !better.is_empty() {
                    improvements.push(Regression {
                        id: (*id).clone(),
                        kind: RegressionKind::Worsened,
                        location: cur.location.clone(),
                        deltas: better,
                    });
                } else {
                    unchanged += 1;
                }
            }
            None => {
                new_items.push((*cur).clone());
                if let Some(delta) = classify_new_item(cur, cfg) {
                    regressions.push(Regression {
                        id: (*id).clone(),
                        kind: RegressionKind::New,
                        location: cur.location.clone(),
                        deltas: vec![delta],
                    });
                }
            }
        }
    }

    for (id, base) in &baseline_by_id {
        if !current_by_id.contains_key(id) {
            removed_items.push((*base).clone());
        }
    }

    let summary = ReportSummary {
        regressed: regressions.len(),
        new_items: new_items.len(),
        improved: improvements.len(),
        unchanged,
        removed: removed_items.len(),
    };

    RegressionReport {
        analyzer: current.analyzer.clone(),
        baseline_path: baseline_path.to_owned(),
        summary,
        regressions,
        improvements,
        new_items,
        removed_items,
    }
}

/// Detect "new cycle" extras-only regressions for the coupling
/// analyzer. Compares the sorted-member-set of every cycle in
/// `current.extras["cycles"]` against `baseline.extras["cycles"]` and
/// returns one `Regression` per cycle absent from the baseline.
///
/// Lives here next to [`compare`] so the runner can call both and
/// merge the results without duplicating the snapshot loading.
pub fn compare_cycles(baseline: &Snapshot, current: &Snapshot) -> Vec<Regression> {
    let baseline_cycles = extract_cycle_keys(baseline);
    let current_cycles = extract_cycle_keys_with_members(current);
    current_cycles
        .into_iter()
        .filter(|(key, _)| !baseline_cycles.contains(key))
        .map(|(_, members)| Regression {
            id: BTreeMap::from([("cycle".to_owned(), members.join(" → "))]),
            kind: RegressionKind::NewCycle,
            location: BTreeMap::new(),
            deltas: Vec::new(),
        })
        .collect()
}

fn extract_cycle_keys(snap: &Snapshot) -> BTreeSet<Vec<String>> {
    snap.extras
        .get("cycles")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(cycle_sorted_members).collect())
        .unwrap_or_default()
}

fn extract_cycle_keys_with_members(snap: &Snapshot) -> Vec<(Vec<String>, Vec<String>)> {
    snap.extras
        .get("cycles")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let members = cycle_members(c)?;
                    let mut sorted = members.clone();
                    sorted.sort();
                    Some((sorted, members))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cycle_sorted_members(value: &serde_json::Value) -> Option<Vec<String>> {
    let mut members = cycle_members(value)?;
    members.sort();
    Some(members)
}

fn cycle_members(value: &serde_json::Value) -> Option<Vec<String>> {
    value.get("members").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect()
    })
}

/// For each metric in `metrics`, produce a `MetricDelta` if it
/// changed. Returned as `(worsened, improved)` so the caller can
/// route them to the right report buckets without re-walking.
fn diff_metrics(
    baseline: &BTreeMap<String, f64>,
    current: &BTreeMap<String, f64>,
    metrics: &[String],
) -> (Vec<MetricDelta>, Vec<MetricDelta>) {
    let mut worse = Vec::new();
    let mut better = Vec::new();
    for metric in metrics {
        let base = baseline.get(metric).copied();
        let cur = current.get(metric).copied();
        let (Some(b), Some(c)) = (base, cur) else {
            continue;
        };
        if c > b {
            worse.push(MetricDelta {
                metric: metric.clone(),
                baseline: Some(b),
                current: c,
                delta: c - b,
            });
        } else if c < b {
            better.push(MetricDelta {
                metric: metric.clone(),
                baseline: Some(b),
                current: c,
                delta: c - b,
            });
        }
    }
    (worse, better)
}

/// Classify a brand-new item against the configured policy. `Some`
/// means "this is a regression"; `None` means "report as new but
/// don't fail".
fn classify_new_item(item: &Item, cfg: &RatchetConfig) -> Option<MetricDelta> {
    let cur = item
        .metrics
        .get(&cfg.primary_metric)
        .copied()
        .unwrap_or(0.0);
    let trigger = match cfg.policy {
        NewItemPolicy::Strict => cur > 0.0,
        NewItemPolicy::Threshold => match cfg.new_item_threshold {
            Some(t) => cur >= t,
            None => false,
        },
        NewItemPolicy::Ignore => false,
    };
    if !trigger {
        return None;
    }
    Some(MetricDelta {
        metric: cfg.primary_metric.clone(),
        baseline: None,
        current: cur,
        delta: cur,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline::{Item, Snapshot};
    use std::collections::BTreeMap;

    fn item(file: &str, name: &str, metrics: &[(&str, f64)]) -> Item {
        Item {
            id: BTreeMap::from([
                ("file".to_owned(), file.to_owned()),
                ("name".to_owned(), name.to_owned()),
            ]),
            metrics: metrics.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
            location: BTreeMap::new(),
        }
    }

    fn snap(items: Vec<Item>) -> Snapshot {
        Snapshot::new(
            "complexity",
            serde_json::Value::Null,
            items,
            serde_json::Map::new(),
        )
    }

    fn cfg(policy: NewItemPolicy, threshold: Option<f64>) -> RatchetConfig {
        RatchetConfig {
            metrics: vec!["cognitive".to_owned()],
            primary_metric: "cognitive".to_owned(),
            new_item_threshold: threshold,
            policy,
        }
    }

    #[test]
    fn identical_snapshots_have_no_regressions() {
        let s = snap(vec![item("src/a.rs", "f", &[("cognitive", 5.0)])]);
        let report = compare(&s, &s, "baseline.json", &cfg(NewItemPolicy::Strict, None));
        assert!(report.regressions.is_empty());
        assert_eq!(report.summary.unchanged, 1);
    }

    #[test]
    fn worsened_metric_is_a_regression() {
        let base = snap(vec![item("src/a.rs", "f", &[("cognitive", 5.0)])]);
        let cur = snap(vec![item("src/a.rs", "f", &[("cognitive", 9.0)])]);
        let report = compare(
            &base,
            &cur,
            "baseline.json",
            &cfg(NewItemPolicy::Strict, None),
        );
        assert_eq!(report.regressions.len(), 1);
        let reg = &report.regressions[0];
        assert!(matches!(reg.kind, RegressionKind::Worsened));
        assert_eq!(reg.deltas[0].metric, "cognitive");
        assert_eq!(reg.deltas[0].baseline, Some(5.0));
        assert_eq!(reg.deltas[0].current, 9.0);
        assert_eq!(reg.deltas[0].delta, 4.0);
    }

    #[test]
    fn improved_metric_is_listed_under_improvements_only() {
        let base = snap(vec![item("src/a.rs", "f", &[("cognitive", 9.0)])]);
        let cur = snap(vec![item("src/a.rs", "f", &[("cognitive", 5.0)])]);
        let report = compare(
            &base,
            &cur,
            "baseline.json",
            &cfg(NewItemPolicy::Strict, None),
        );
        assert!(report.regressions.is_empty());
        assert_eq!(report.improvements.len(), 1);
        assert_eq!(report.improvements[0].deltas[0].delta, -4.0);
    }

    #[test]
    fn existing_debt_unchanged_is_silent() {
        // Pre-existing high-cognitive function in both snapshots; a
        // separate item improves. Regressions list stays empty.
        let base = snap(vec![
            item("src/a.rs", "debt", &[("cognitive", 30.0)]),
            item("src/a.rs", "tweak", &[("cognitive", 4.0)]),
        ]);
        let cur = snap(vec![
            item("src/a.rs", "debt", &[("cognitive", 30.0)]),
            item("src/a.rs", "tweak", &[("cognitive", 2.0)]),
        ]);
        let report = compare(&base, &cur, "b.json", &cfg(NewItemPolicy::Strict, None));
        assert!(
            report.regressions.is_empty(),
            "got {:?}",
            report.regressions
        );
    }

    #[test]
    fn strict_policy_treats_new_item_as_regression() {
        let base = snap(vec![]);
        let cur = snap(vec![item("src/a.rs", "newf", &[("cognitive", 7.0)])]);
        let report = compare(&base, &cur, "b.json", &cfg(NewItemPolicy::Strict, None));
        assert_eq!(report.regressions.len(), 1);
        assert!(matches!(report.regressions[0].kind, RegressionKind::New));
        assert_eq!(report.summary.new_items, 1);
    }

    #[test]
    fn ignore_policy_keeps_new_item_off_regressions() {
        let base = snap(vec![]);
        let cur = snap(vec![item("src/a.rs", "newf", &[("cognitive", 7.0)])]);
        let report = compare(&base, &cur, "b.json", &cfg(NewItemPolicy::Ignore, None));
        assert!(report.regressions.is_empty());
        assert_eq!(report.new_items.len(), 1);
    }

    #[test]
    fn threshold_policy_uses_threshold_as_lower_bound() {
        let base = snap(vec![]);
        let cur = snap(vec![
            item("src/a.rs", "small", &[("cognitive", 4.0)]),
            item("src/a.rs", "big", &[("cognitive", 12.0)]),
        ]);
        let report = compare(
            &base,
            &cur,
            "b.json",
            &cfg(NewItemPolicy::Threshold, Some(10.0)),
        );
        assert_eq!(report.regressions.len(), 1);
        assert_eq!(report.regressions[0].id["name"], "big");
    }

    #[test]
    fn strict_policy_skips_zero_metric_new_items() {
        // A new function with cognitive=0 (e.g. an empty body) is not
        // new debt — strict means "non-trivial primary metric".
        let base = snap(vec![]);
        let cur = snap(vec![item("src/a.rs", "empty", &[("cognitive", 0.0)])]);
        let report = compare(&base, &cur, "b.json", &cfg(NewItemPolicy::Strict, None));
        assert!(report.regressions.is_empty());
        assert_eq!(report.new_items.len(), 1);
    }

    #[test]
    fn removed_items_are_listed_but_not_a_regression() {
        let base = snap(vec![item("src/a.rs", "gone", &[("cognitive", 5.0)])]);
        let cur = snap(vec![]);
        let report = compare(&base, &cur, "b.json", &cfg(NewItemPolicy::Strict, None));
        assert!(report.regressions.is_empty());
        assert_eq!(report.removed_items.len(), 1);
    }

    #[test]
    fn fail_on_regression_ignores_new_items() {
        let base = snap(vec![]);
        let cur = snap(vec![item("src/a.rs", "newf", &[("cognitive", 7.0)])]);
        let report = compare(&base, &cur, "b.json", &cfg(NewItemPolicy::Strict, None));
        assert!(report.has_failures(FailOn::Any));
        assert!(!report.has_failures(FailOn::Regression));
        assert!(report.has_failures(FailOn::NewItem));
    }

    #[test]
    fn fail_on_new_item_ignores_worsened_existing_items() {
        let base = snap(vec![item("src/a.rs", "f", &[("cognitive", 5.0)])]);
        let cur = snap(vec![item("src/a.rs", "f", &[("cognitive", 9.0)])]);
        let report = compare(&base, &cur, "b.json", &cfg(NewItemPolicy::Strict, None));
        assert!(report.has_failures(FailOn::Any));
        assert!(report.has_failures(FailOn::Regression));
        assert!(!report.has_failures(FailOn::NewItem));
    }

    #[test]
    fn unchanged_count_excludes_changed_items() {
        let base = snap(vec![
            item("src/a.rs", "stable", &[("cognitive", 3.0)]),
            item("src/a.rs", "shifty", &[("cognitive", 4.0)]),
        ]);
        let cur = snap(vec![
            item("src/a.rs", "stable", &[("cognitive", 3.0)]),
            item("src/a.rs", "shifty", &[("cognitive", 6.0)]),
        ]);
        let report = compare(&base, &cur, "b.json", &cfg(NewItemPolicy::Strict, None));
        assert_eq!(report.summary.unchanged, 1);
        assert_eq!(report.summary.regressed, 1);
    }

    #[test]
    fn metrics_filter_only_compares_listed_metrics() {
        // Cognitive went up — but we only track cyclomatic. No regression.
        let base = snap(vec![item(
            "src/a.rs",
            "f",
            &[("cognitive", 5.0), ("cyclomatic", 2.0)],
        )]);
        let cur = snap(vec![item(
            "src/a.rs",
            "f",
            &[("cognitive", 9.0), ("cyclomatic", 2.0)],
        )]);
        let mut c = cfg(NewItemPolicy::Strict, None);
        c.metrics = vec!["cyclomatic".to_owned()];
        let report = compare(&base, &cur, "b.json", &c);
        assert!(
            report.regressions.is_empty(),
            "got {:?}",
            report.regressions
        );
        assert_eq!(report.summary.unchanged, 1);
    }

    fn cycle_extras(cycles: &[Vec<&str>]) -> serde_json::Map<String, serde_json::Value> {
        let cycles_json: Vec<serde_json::Value> = cycles
            .iter()
            .map(|members| {
                serde_json::json!({
                    "size": members.len(),
                    "members": members,
                })
            })
            .collect();
        let mut m = serde_json::Map::new();
        m.insert("cycles".to_owned(), serde_json::Value::Array(cycles_json));
        m
    }

    #[test]
    fn compare_cycles_flags_only_new_cycles() {
        let mut base = snap(vec![]);
        base.extras = cycle_extras(&[vec!["a", "b"]]);
        let mut cur = snap(vec![]);
        cur.extras = cycle_extras(&[vec!["a", "b"], vec!["c", "d"]]);
        let regs = compare_cycles(&base, &cur);
        assert_eq!(regs.len(), 1);
        let id = &regs[0].id["cycle"];
        assert!(id.contains("c") && id.contains("d"), "got: {id}");
    }

    #[test]
    fn compare_cycles_treats_member_order_as_irrelevant() {
        // baseline lists [b, a], current lists [a, b] — same cycle.
        let mut base = snap(vec![]);
        base.extras = cycle_extras(&[vec!["b", "a"]]);
        let mut cur = snap(vec![]);
        cur.extras = cycle_extras(&[vec!["a", "b"]]);
        let regs = compare_cycles(&base, &cur);
        assert!(regs.is_empty(), "got: {regs:?}");
    }
}
