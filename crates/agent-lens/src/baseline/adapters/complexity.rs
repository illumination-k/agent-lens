//! Complexity adapter: turn the typed per-file output of
//! [`ComplexityAnalyzer::collect`](crate::analyze::ComplexityAnalyzer::collect)
//! into the `(file, name)`-keyed [`Item`] list the baseline subsystem
//! stores.
//!
//! Method shadowing inside one file (two `impl` blocks of the same
//! type, two free functions in different `mod tests`) is real, so a
//! pure `(file, name)` key would collide. We resolve it by appending
//! an emission-order index (`name#1`, `name#2`) when the file holds
//! more than one function with the same name. The id round-trips
//! identically across save/check runs as long as emission order is
//! stable, which `lens_*::extract_complexity_units` guarantees.

use std::collections::{BTreeMap, HashMap};

use lens_domain::FunctionComplexity;

use crate::analyze::ComplexityFileReport;
use crate::baseline::{Baselinable, Item};

pub struct ComplexityBaseline;

impl Baselinable for ComplexityBaseline {
    const ANALYZER_NAME: &'static str = "complexity";

    fn ratchet_metrics() -> &'static [&'static str] {
        // `maintainability_index` is intentionally omitted — it's a
        // function of LOC and Halstead and rocks around under
        // refactors that don't change the underlying logic. Opt in
        // via `--metric maintainability_index`.
        &["cognitive", "cyclomatic", "max_nesting"]
    }

    fn primary_metric() -> &'static str {
        "cognitive"
    }

    fn new_item_threshold(metric: &str) -> Option<f64> {
        // Mirrors the cutoffs the existing PreToolUse hook uses, so
        // `--new-item-policy threshold` and the hook agree on what
        // counts as "non-trivial new function".
        match metric {
            "cognitive" => Some(10.0),
            "cyclomatic" => Some(8.0),
            "max_nesting" => Some(3.0),
            _ => None,
        }
    }
}

/// Project the per-file complexity reports into snapshot items. One
/// item per function; ids include a `#N` suffix when needed to
/// disambiguate same-name functions in one file.
pub fn to_items(reports: &[ComplexityFileReport]) -> Vec<Item> {
    let mut items = Vec::new();
    for file in reports {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        // Two-pass: first count, then emit. We only suffix names that
        // actually collide so the common case keeps clean ids.
        let mut totals: HashMap<&str, usize> = HashMap::new();
        for f in &file.functions {
            *totals.entry(f.name.as_str()).or_insert(0) += 1;
        }
        for f in &file.functions {
            let total = totals[f.name.as_str()];
            let display_name = if total > 1 {
                let counter = seen.entry(f.name.as_str()).or_insert(0);
                *counter += 1;
                format!("{}#{}", f.name, counter)
            } else {
                f.name.clone()
            };
            items.push(item_for(&file.file, &display_name, f));
        }
    }
    items
}

fn item_for(file: &str, name: &str, f: &FunctionComplexity) -> Item {
    let mut metrics: BTreeMap<String, f64> = BTreeMap::new();
    metrics.insert("cognitive".to_owned(), f64::from(f.cognitive));
    metrics.insert("cyclomatic".to_owned(), f64::from(f.cyclomatic));
    metrics.insert("max_nesting".to_owned(), f64::from(f.max_nesting));
    metrics.insert("loc".to_owned(), f.loc() as f64);
    if let Some(mi) = f.maintainability_index() {
        metrics.insert("maintainability_index".to_owned(), mi);
    }
    Item {
        id: BTreeMap::from([
            ("file".to_owned(), file.to_owned()),
            ("name".to_owned(), name.to_owned()),
        ]),
        metrics,
        location: BTreeMap::from([
            ("start_line".to_owned(), serde_json::json!(f.start_line)),
            ("end_line".to_owned(), serde_json::json!(f.end_line)),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::ComplexityAnalyzer;
    use crate::test_support::write_file;

    #[test]
    fn to_items_emits_one_item_per_function_with_complexity_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            "fn simple() {}\nfn branchy(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\n",
        );
        let reports = ComplexityAnalyzer::new().collect(&file).unwrap();
        let items = to_items(&reports);
        assert_eq!(items.len(), 2);
        let names: Vec<&str> = items.iter().map(|i| i.id["name"].as_str()).collect();
        assert!(names.contains(&"simple"));
        assert!(names.contains(&"branchy"));
        let branchy = items.iter().find(|i| i.id["name"] == "branchy").unwrap();
        assert!(branchy.metrics.contains_key("cognitive"));
        assert!(branchy.metrics.contains_key("cyclomatic"));
        assert!(branchy.metrics.contains_key("max_nesting"));
        assert!(branchy.metrics["cyclomatic"] >= 2.0);
    }

    #[test]
    fn rust_methods_get_owner_qualified_names_without_suffix() {
        // Rust extractor pre-qualifies methods (`A::run`, `B::run`),
        // so even two `impl` blocks with the same method name produce
        // distinct ids without needing the emission-order suffix.
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
struct A;
struct B;
impl A { fn run(&self) -> i32 { 1 } }
impl B { fn run(&self) -> i32 { 2 } }
"#,
        );
        let reports = ComplexityAnalyzer::new().collect(&file).unwrap();
        let items = to_items(&reports);
        let names: Vec<&str> = items.iter().map(|i| i.id["name"].as_str()).collect();
        assert!(names.contains(&"A::run"), "got: {names:?}");
        assert!(names.contains(&"B::run"), "got: {names:?}");
        // No `#N` suffix needed because the names already differ.
        assert!(names.iter().all(|n| !n.contains('#')), "got: {names:?}",);
    }

    #[test]
    fn synthetic_duplicate_names_get_emission_order_suffix() {
        // The owner-qualifier prevents collisions in Rust, but other
        // languages (or a future extractor change) could still emit
        // two functions with identical names in one file. Pin the
        // suffix logic directly with a synthetic report so the
        // disambiguation contract is exercised.
        use lens_domain::{FunctionComplexity, HalsteadCounts};
        let dup = |name: &str, line: usize| FunctionComplexity {
            name: name.to_owned(),
            start_line: line,
            end_line: line,
            cyclomatic: 1,
            cognitive: 0,
            max_nesting: 0,
            halstead: HalsteadCounts::default(),
        };
        let report = ComplexityFileReport {
            file: "src/lib.rs".to_owned(),
            functions: vec![dup("run", 1), dup("run", 5), dup("solo", 9)],
        };
        let items = to_items(&[report]);
        let names: Vec<&str> = items.iter().map(|i| i.id["name"].as_str()).collect();
        assert!(names.contains(&"run#1"), "got: {names:?}");
        assert!(names.contains(&"run#2"), "got: {names:?}");
        assert!(names.contains(&"solo"), "got: {names:?}");
    }

    #[test]
    fn singletons_keep_unsuffixed_names() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", "fn solo() -> i32 { 0 }\n");
        let reports = ComplexityAnalyzer::new().collect(&file).unwrap();
        let items = to_items(&reports);
        assert_eq!(items[0].id["name"], "solo");
    }

    #[test]
    fn new_item_threshold_matches_hook_cutoff_for_cognitive() {
        // The PreToolUse complexity hook flags cognitive >= 10; the
        // baseline threshold policy must agree, otherwise a new debt
        // function would be silent under the baseline but loud under
        // the hook.
        assert_eq!(
            ComplexityBaseline::new_item_threshold("cognitive"),
            Some(10.0),
        );
    }
}
