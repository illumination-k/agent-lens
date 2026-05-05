//! Baseline & ratchet — save a snapshot of analyzer metrics, then on
//! later runs fail only on regressions (or new debt) rather than on
//! existing debt.
//!
//! The submodules split responsibility:
//!
//! - [`storage`] — `Snapshot::load` / `Snapshot::save`, default path
//!   resolution (`.agent-lens/baseline/<analyzer>.json`).
//! - [`compare`] — pure regression-detection logic on two `Snapshot`s.
//! - [`adapters`] — per-analyzer `Baselinable` impls that turn the
//!   typed `collect()` output of each analyzer into a `Vec<Item>`.
//! - [`runner`] — glue between the CLI and the adapters: dispatches
//!   `save` / `check` for the supported analyzers.
//!
//! The on-disk shape lives here in [`Snapshot`] / [`Item`] and is
//! deliberately decoupled from the per-analyzer `Report` structs in
//! [`crate::analyze`] so analyzer code can evolve without invalidating
//! every saved baseline. [`SNAPSHOT_FORMAT_VERSION`] guards the schema.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub mod adapters;
pub mod compare;
pub mod runner;
pub mod storage;

pub use compare::{
    MetricDelta, Regression, RegressionKind, RegressionReport, ReportSummary, compare,
    compare_cycles,
};
pub use runner::{
    AnalyzerKind, AnalyzerOptions, BaselineCheck, BaselineCommand, BaselineExitCode, BaselineSave,
    CheckOutcome, RunOutcome, SaveOutcome, run_baseline,
};
pub use storage::{StorageError, default_baseline_path, load_snapshot, save_snapshot};

/// Schema version of the on-disk snapshot. Bumped on a breaking change
/// to [`Snapshot`] / [`Item`]. `Snapshot::load` rejects mismatched
/// versions with a clear error rather than silently mis-comparing.
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// Stable identifier for an item across save/check runs. Encoded as a
/// `BTreeMap` so JSON output is a sorted object (deterministic) and
/// equality comparison is structural — line numbers are deliberately
/// **not** part of the id (they live in [`Item::location`] as a hint
/// only) so editing a function doesn't break matching.
pub type ItemId = BTreeMap<String, String>;

/// One ratcheted thing: a function, a cohesion unit, a module, a file.
/// `metrics` is the lower-is-better numeric snapshot consulted by
/// [`compare`]; `location` is informational and not used for matching.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub id: ItemId,
    pub metrics: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub location: BTreeMap<String, serde_json::Value>,
}

/// Self-describing snapshot of one analyzer's metrics at a point in
/// time. Written by `agent-lens baseline save` and read by
/// `agent-lens baseline check`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub format_version: u32,
    pub analyzer: String,
    pub agent_lens_version: String,
    pub generated_at: String,
    /// Free-form record of the args this snapshot was produced with, so
    /// `check` can warn on mismatched inputs. Not used for matching.
    pub args: serde_json::Value,
    pub items: Vec<Item>,
    /// Per-analyzer non-per-item data (e.g. coupling's dependency
    /// cycles). Compared via the analyzer's adapter, not by
    /// [`compare`].
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

impl Snapshot {
    /// Build a fresh snapshot with the current `agent-lens` version,
    /// the current UTC instant, and `format_version` pinned to
    /// [`SNAPSHOT_FORMAT_VERSION`]. Adapters call this to assemble a
    /// snapshot from their `to_items` / `to_extras` output.
    pub fn new(
        analyzer: impl Into<String>,
        args: serde_json::Value,
        items: Vec<Item>,
        extras: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        Self {
            format_version: SNAPSHOT_FORMAT_VERSION,
            analyzer: analyzer.into(),
            agent_lens_version: env!("CARGO_PKG_VERSION").to_owned(),
            generated_at: now_rfc3339(),
            args,
            items,
            extras,
        }
    }
}

/// New-item handling for `baseline check`. Existing items are always
/// compared by metric delta; this only governs items present in the
/// current run that aren't in the baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum NewItemPolicy {
    /// Any new item with a non-zero primary metric is a regression.
    /// Honours "fail on regressions, not on existing debt": existing
    /// debt is grandfathered, but new debt is not.
    #[default]
    Strict,
    /// New item is a regression only when its primary metric exceeds
    /// the analyzer's `new_item_threshold` (the same threshold used by
    /// the existing PreToolUse hooks). Useful when you want to allow
    /// trivial new functions without flagging them.
    Threshold,
    /// New items never flip the exit code. They are still listed under
    /// [`RegressionReport::new_items`] for visibility.
    Ignore,
}

/// Which kinds of findings should flip the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum FailOn {
    /// Either a worsened existing item or a new item per
    /// [`NewItemPolicy`].
    #[default]
    Any,
    /// Only worsened existing items (and `extras`-based regressions
    /// such as new coupling cycles) flip the exit code; new items are
    /// reported but ignored.
    Regression,
    /// Only new items per [`NewItemPolicy`] flip the exit code;
    /// worsened existing items are reported but ignored. Mostly useful
    /// alongside `--new-item-policy strict` when you want to gate
    /// purely on "no new debt".
    NewItem,
}

/// Compile-time configuration each analyzer adapter exposes for the
/// baseline subsystem.
pub trait Baselinable {
    /// Kebab-case CLI name (`"complexity"`, `"cohesion"`, …).
    const ANALYZER_NAME: &'static str;
    /// Default set of metric names that count toward "regressed". The
    /// CLI's `--metric` flag narrows or extends this.
    fn ratchet_metrics() -> &'static [&'static str];
    /// The metric the new-item policy consults when deciding whether a
    /// brand-new item counts as new debt.
    fn primary_metric() -> &'static str;
    /// Lower-bound threshold under [`NewItemPolicy::Threshold`] for the
    /// given metric, mirroring the values the existing PreToolUse
    /// hooks use. `None` means "no threshold defined" — under
    /// `Threshold` policy such metrics never trigger.
    fn new_item_threshold(metric: &str) -> Option<f64>;
}

/// Errors produced by the baseline subsystem.
#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    #[error("baseline storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("analyzer error: {0}")]
    Analyzer(String),
    #[error(
        "baseline analyzer `{requested}` is not supported by `agent-lens baseline`; supported analyzers: complexity, cohesion, coupling, hotspot"
    )]
    UnsupportedAnalyzer { requested: String },
    #[error(
        "baseline analyzer mismatch: snapshot was saved as `{baseline}` but `check` was invoked for `{requested}`"
    )]
    AnalyzerMismatch { baseline: String, requested: String },
    #[error("unknown metric `{requested}` for analyzer `{analyzer}`; known: {known}")]
    UnknownMetric {
        analyzer: String,
        requested: String,
        known: String,
    },
}

/// Minimal RFC-3339 UTC formatter for the `generated_at` snapshot
/// field. Avoids a `chrono` / `time` dependency by computing the
/// civil date manually from the unix epoch — the value is purely
/// informational (we never parse it back), so this is good enough.
fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_secs_rfc3339(secs)
}

/// Convert a unix-epoch second count into an RFC-3339 UTC string.
/// Pulled out of [`now_rfc3339`] so the formatter is unit-testable
/// without depending on the wall clock.
fn format_unix_secs_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's civil-from-days algorithm: convert a day count
/// since the unix epoch (1970-01-01) into a `(year, month, day)`
/// triple in the proleptic Gregorian calendar.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_through_serde() {
        let snap = Snapshot::new(
            "complexity",
            serde_json::json!({"path": "src"}),
            vec![Item {
                id: BTreeMap::from([
                    ("file".to_owned(), "src/lib.rs".to_owned()),
                    ("name".to_owned(), "foo".to_owned()),
                ]),
                metrics: BTreeMap::from([("cognitive".to_owned(), 7.0)]),
                location: BTreeMap::from([("start_line".to_owned(), serde_json::json!(10))]),
            }],
            serde_json::Map::new(),
        );
        let s = serde_json::to_string(&snap).unwrap();
        let back: Snapshot = serde_json::from_str(&s).unwrap();
        assert_eq!(back.format_version, SNAPSHOT_FORMAT_VERSION);
        assert_eq!(back.analyzer, "complexity");
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].metrics["cognitive"], 7.0);
    }

    #[test]
    fn empty_extras_are_skipped_in_json() {
        let snap = Snapshot::new(
            "cohesion",
            serde_json::Value::Null,
            Vec::new(),
            serde_json::Map::new(),
        );
        let s = serde_json::to_string(&snap).unwrap();
        assert!(!s.contains("extras"), "got: {s}");
    }

    #[test]
    fn empty_location_is_skipped_in_json() {
        let item = Item {
            id: BTreeMap::from([("path".to_owned(), "foo".to_owned())]),
            metrics: BTreeMap::new(),
            location: BTreeMap::new(),
        };
        let s = serde_json::to_string(&item).unwrap();
        assert!(!s.contains("location"), "got: {s}");
    }

    #[test]
    fn civil_from_days_recovers_unix_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn civil_from_days_recovers_leap_day() {
        // 2020-02-29 is day 18_321 since the unix epoch.
        assert_eq!(civil_from_days(18_321), (2020, 2, 29));
    }

    #[test]
    fn format_unix_secs_rfc3339_renders_known_instant() {
        // 2024-01-02T03:04:05Z -> seconds since the epoch (computed
        // ahead of time via `date -u -d ... +%s`):
        // 86_400 * (DAYS) + 3*3600 + 4*60 + 5 = 1_704_164_645
        assert_eq!(
            format_unix_secs_rfc3339(1_704_164_645),
            "2024-01-02T03:04:05Z",
        );
    }
}
