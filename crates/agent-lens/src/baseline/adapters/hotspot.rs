//! Hotspot adapter: turn the typed [`HotspotCollection`] from
//! [`HotspotAnalyzer::collect`](crate::analyze::HotspotAnalyzer::collect)
//! into one ratchet item per file.
//!
//! The hotspot score is `commits × cognitive_max`, so it depends on
//! the git churn window. Re-running `baseline check` from a different
//! point in history (or with a different `--since`) will move every
//! score even if no source has changed. Document this in the CLI
//! help; pin `--since` for stable comparisons.

use std::collections::BTreeMap;

use lens_domain::HotspotEntry;

use crate::analyze::HotspotCollection;
use crate::baseline::{Baselinable, Item};

pub struct HotspotBaseline;

impl Baselinable for HotspotBaseline {
    const ANALYZER_NAME: &'static str = "hotspot";

    fn ratchet_metrics() -> &'static [&'static str] {
        // `hotspot_score` rolls churn and complexity into a single
        // signal, which is what the analyzer was built around.
        // `cognitive_max` and `commits` are exposed as diagnostic
        // metrics in `metrics` but stay off the default ratchet.
        &["hotspot_score"]
    }

    fn primary_metric() -> &'static str {
        "hotspot_score"
    }

    fn new_item_threshold(metric: &str) -> Option<f64> {
        match metric {
            "hotspot_score" => Some(1.0),
            _ => None,
        }
    }
}

pub fn to_items(collection: &HotspotCollection) -> Vec<Item> {
    collection.entries.iter().map(item_for).collect()
}

fn item_for(entry: &HotspotEntry) -> Item {
    let mut metrics: BTreeMap<String, f64> = BTreeMap::new();
    metrics.insert("hotspot_score".to_owned(), entry.score as f64);
    metrics.insert("commits".to_owned(), f64::from(entry.commits));
    metrics.insert("cognitive_max".to_owned(), f64::from(entry.cognitive_max));
    metrics.insert("cyclomatic_max".to_owned(), f64::from(entry.cyclomatic_max));
    metrics.insert("loc".to_owned(), entry.loc as f64);
    Item {
        id: BTreeMap::from([("file".to_owned(), entry.path.clone())]),
        metrics,
        location: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::HotspotAnalyzer;
    use crate::test_support::{run_git, write_file};

    fn init_repo(dir: &std::path::Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        write_file(dir, "src/lib.rs", "pub mod a;\n");
        write_file(
            dir,
            "src/a.rs",
            "pub fn nest(n: i32) -> i32 {\n    if n > 0 { if n > 10 { return 1; } }\n    0\n}\n",
        );
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", "initial"]);
    }

    #[test]
    fn to_items_emits_one_item_per_file_with_score_metric() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let collection = HotspotAnalyzer::new().collect(dir.path()).unwrap();
        let items = to_items(&collection);
        assert!(!items.is_empty());
        assert!(
            items
                .iter()
                .all(|i| i.metrics.contains_key("hotspot_score"))
        );
        let a = items.iter().find(|i| i.id["file"] == "src/a.rs").unwrap();
        assert!(a.metrics["commits"] >= 1.0);
    }
}
