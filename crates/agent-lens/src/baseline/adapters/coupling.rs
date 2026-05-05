//! Coupling adapter: turn the typed [`CouplingCollection`] from
//! [`CouplingAnalyzer::collect`](crate::analyze::CouplingAnalyzer::collect)
//! into per-module ratchet items, plus an `extras.cycles` block that
//! [`compare_cycles`](crate::baseline::compare::compare_cycles) reads
//! to flag newly-introduced dependency cycles.
//!
//! `fan_in` is intentionally **not** in `ratchet_metrics`. A module
//! gaining callers isn't that module's regression; it's the new
//! callers' churn. We still record `fan_in` in `metrics` for
//! transparency, but it never trips `compare()`.

use std::collections::BTreeMap;

use lens_domain::ModuleMetrics;

use crate::analyze::CouplingCollection;
use crate::baseline::{Baselinable, Item};

pub struct CouplingBaseline;

impl Baselinable for CouplingBaseline {
    const ANALYZER_NAME: &'static str = "coupling";

    fn ratchet_metrics() -> &'static [&'static str] {
        // `fan_out` and `instability` move when a module grows new
        // outgoing dependencies; both are honest signals of "this
        // module just got more entangled with the rest of the
        // codebase". `fan_in` is informational only — opting it in
        // would amount to flagging a module for being *useful*.
        &["fan_out", "instability"]
    }

    fn primary_metric() -> &'static str {
        "fan_out"
    }

    fn new_item_threshold(metric: &str) -> Option<f64> {
        match metric {
            "fan_out" => Some(1.0),
            _ => None,
        }
    }
}

pub fn to_items(collection: &CouplingCollection) -> Vec<Item> {
    collection.report.modules.iter().map(item_for).collect()
}

fn item_for(m: &ModuleMetrics) -> Item {
    let mut metrics: BTreeMap<String, f64> = BTreeMap::new();
    metrics.insert("fan_in".to_owned(), m.fan_in as f64);
    metrics.insert("fan_out".to_owned(), m.fan_out as f64);
    metrics.insert("ifc".to_owned(), m.ifc as f64);
    if let Some(i) = m.instability {
        metrics.insert("instability".to_owned(), i);
    }
    Item {
        id: BTreeMap::from([("module".to_owned(), m.path.as_str().to_owned())]),
        metrics,
        location: BTreeMap::new(),
    }
}

/// Encode the report's dependency cycles into the snapshot's `extras`
/// block. Members are stored in their natural order; the comparator
/// in [`crate::baseline::compare::compare_cycles`] sorts before
/// matching so member-order changes are not regressions.
pub fn to_extras(collection: &CouplingCollection) -> serde_json::Map<String, serde_json::Value> {
    let cycles: Vec<serde_json::Value> = collection
        .report
        .cycles
        .iter()
        .map(|c| {
            let members: Vec<&str> = c.members.iter().map(|m| m.as_str()).collect();
            serde_json::json!({
                "size": c.members.len(),
                "members": members,
            })
        })
        .collect();
    let mut extras = serde_json::Map::new();
    if !cycles.is_empty() {
        extras.insert("cycles".to_owned(), serde_json::Value::Array(cycles));
    }
    extras
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::CouplingAnalyzer;
    use crate::test_support::write_file;

    #[test]
    fn to_items_keys_modules_on_path_with_directional_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "pub mod a;\npub mod b;\n");
        write_file(dir.path(), "a.rs", "pub fn helper() {}\npub struct Foo;\n");
        write_file(
            dir.path(),
            "b.rs",
            "use crate::a::Foo;\nfn _x(_f: Foo) { crate::a::helper(); }\n",
        );
        let collection = CouplingAnalyzer::new().collect(&lib).unwrap();
        let items = to_items(&collection);
        let b = items
            .iter()
            .find(|i| i.id["module"] == "crate::b")
            .expect("crate::b present");
        // b depends on a → fan_out >= 1, instability = 1.0.
        assert!(b.metrics["fan_out"] >= 1.0);
        assert_eq!(b.metrics["instability"], 1.0);
    }

    #[test]
    fn to_extras_emits_cycles_only_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "pub mod a;\npub mod b;\n");
        write_file(
            dir.path(),
            "a.rs",
            "use crate::b::Bar;\npub struct Foo;\nfn _x(_b: Bar) {}\n",
        );
        write_file(
            dir.path(),
            "b.rs",
            "use crate::a::Foo;\npub struct Bar;\nfn _y(_f: Foo) {}\n",
        );
        let collection = CouplingAnalyzer::new().collect(&lib).unwrap();
        let extras = to_extras(&collection);
        assert!(extras.contains_key("cycles"));
        let cycles = extras["cycles"].as_array().unwrap();
        assert_eq!(cycles.len(), 1);
    }

    #[test]
    fn to_extras_is_empty_when_no_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "pub mod a;\n");
        write_file(dir.path(), "a.rs", "pub fn solo() {}\n");
        let collection = CouplingAnalyzer::new().collect(&lib).unwrap();
        let extras = to_extras(&collection);
        assert!(extras.is_empty(), "got: {extras:?}");
    }
}
