//! Cohesion adapter: turn the typed per-file output of
//! [`CohesionAnalyzer::collect`](crate::analyze::CohesionAnalyzer::collect)
//! into ratchet items keyed on `(file, kind, trait_name?, type_name)`.
//!
//! Two `impl` blocks for the same type still differ on `trait_name`
//! (`Inherent` vs `Trait { trait_name }`), so the four-key id is
//! sufficient to disambiguate; we don't need the emission-order
//! suffix that the complexity adapter uses.

use std::collections::BTreeMap;

use lens_domain::{CohesionUnit, CohesionUnitKind};

use crate::analyze::CohesionFileReport;
use crate::baseline::{Baselinable, Item};

pub struct CohesionBaseline;

impl Baselinable for CohesionBaseline {
    const ANALYZER_NAME: &'static str = "cohesion";

    fn ratchet_metrics() -> &'static [&'static str] {
        // LCOM4 is the comparable scalar. LCOM96 is a float in
        // `[0, 1]` whose floor is "fully cohesive" — opt in via
        // `--metric lcom96` if desired.
        &["lcom4"]
    }

    fn primary_metric() -> &'static str {
        "lcom4"
    }

    fn new_item_threshold(metric: &str) -> Option<f64> {
        // The PreToolUse cohesion hook flags lcom4 >= 2; mirror it
        // here so the threshold policy and the hook agree on what
        // counts as a non-trivial new unit.
        match metric {
            "lcom4" => Some(2.0),
            _ => None,
        }
    }
}

pub fn to_items(reports: &[CohesionFileReport]) -> Vec<Item> {
    let mut items = Vec::new();
    for file in reports {
        for unit in &file.units {
            items.push(item_for(&file.file, unit));
        }
    }
    items
}

fn item_for(file: &str, unit: &CohesionUnit) -> Item {
    let mut id: BTreeMap<String, String> = BTreeMap::new();
    id.insert("file".to_owned(), file.to_owned());
    let (kind, trait_name) = match &unit.kind {
        CohesionUnitKind::Inherent => ("inherent", None),
        CohesionUnitKind::Trait { trait_name } => ("trait", Some(trait_name.as_str())),
        CohesionUnitKind::Module => ("module", None),
    };
    id.insert("kind".to_owned(), kind.to_owned());
    if let Some(t) = trait_name {
        id.insert("trait_name".to_owned(), t.to_owned());
    }
    id.insert("type_name".to_owned(), unit.type_name.clone());

    let mut metrics: BTreeMap<String, f64> = BTreeMap::new();
    metrics.insert("lcom4".to_owned(), unit.lcom4 as f64);
    metrics.insert("method_count".to_owned(), unit.methods.len() as f64);
    if let Some(lcom96) = unit.lcom96 {
        metrics.insert("lcom96".to_owned(), lcom96);
    }

    Item {
        id,
        metrics,
        location: BTreeMap::from([
            ("start_line".to_owned(), serde_json::json!(unit.start_line)),
            ("end_line".to_owned(), serde_json::json!(unit.end_line)),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::CohesionAnalyzer;
    use crate::test_support::write_file;

    #[test]
    fn to_items_keys_inherent_impls_on_kind_and_type_name() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
struct Thing { a: i32, b: i32 }
impl Thing {
    fn ga(&self) -> i32 { self.a }
    fn gb(&self) -> i32 { self.b }
}
"#,
        );
        // Walk from the directory so display paths come out relative
        // to it (single-file mode keeps the absolute path).
        let reports = CohesionAnalyzer::new().collect(dir.path()).unwrap();
        let items = to_items(&reports);
        // Filter to just our fixture, ignoring anything WalkBuilder
        // might pick up under the tempdir from sibling tests.
        let items: Vec<_> = items.iter().filter(|i| i.id["file"] == "lib.rs").collect();
        assert_eq!(items.len(), 1);
        let id = &items[0].id;
        assert_eq!(id["file"], "lib.rs");
        assert_eq!(id["kind"], "inherent");
        assert_eq!(id["type_name"], "Thing");
        assert!(!id.contains_key("trait_name"));
        assert!(items[0].metrics.contains_key("lcom4"));
        let _ = file;
    }
}
