//! Engine-agnostic per-`impl` cohesion report for PreToolUse hooks.
//!
//! Mirrors [`super::complexity`]: the hook adapters call
//! [`CohesionCore::run`] with the files about to be touched and get back
//! a fully-formatted report — or `None` when every `impl` block is
//! trivially cohesive. Each agent then wraps that string in the
//! engine-specific output envelope.

use std::fmt::Write as _;

use lens_domain::{CohesionUnit, CohesionUnitKind};

use crate::analyze::SourceLang;
use crate::hooks::core::{EditedSource, HookError};

/// LCOM4 value at or above which an `impl` block is reported. `1` means
/// "every method touches the same shared state" — no signal. `2` is the
/// first level that hints at a split-personality `impl`.
const LCOM4_FLOOR: usize = 2;

/// Runner for the pre-edit cohesion hook.
#[derive(Debug, Clone, Default)]
pub struct CohesionCore;

impl CohesionCore {
    pub fn new() -> Self {
        Self
    }

    /// Analyse every source and produce a single report.
    ///
    /// Returns `Ok(None)` when no file produced any incohesive unit, so
    /// callers can treat "no signal" as "no message" without inspecting
    /// the report string.
    pub fn run(&self, sources: &[EditedSource]) -> Result<Option<String>, HookError> {
        let mut body = String::new();
        let mut total = 0usize;

        for src in sources {
            let units = extract_units(src.lang, &src.source)?;
            let flagged: Vec<&CohesionUnit> =
                units.iter().filter(|u| u.lcom4() >= LCOM4_FLOOR).collect();
            if flagged.is_empty() {
                continue;
            }
            total += flagged.len();
            append_section(&mut body, &src.rel_path, &flagged);
        }

        if total == 0 {
            return Ok(None);
        }

        let header = format!("agent-lens cohesion: {total} incohesive impl block(s) before edit\n");
        Ok(Some(format!("{header}{body}")))
    }
}

fn extract_units(lang: SourceLang, source: &str) -> Result<Vec<CohesionUnit>, HookError> {
    match lang {
        SourceLang::Rust => {
            lens_rust::extract_cohesion_units(source).map_err(|e| HookError::Parse(Box::new(e)))
        }
        SourceLang::TypeScript => {
            lens_ts::extract_cohesion_units(source).map_err(|e| HookError::Parse(Box::new(e)))
        }
        SourceLang::Python => {
            lens_py::extract_cohesion_units(source).map_err(|e| HookError::Parse(Box::new(e)))
        }
    }
}

fn append_section(out: &mut String, file_path: &str, units: &[&CohesionUnit]) {
    // writeln! into a String cannot fail; the result is swallowed
    // deliberately rather than unwrapped to satisfy the workspace's
    // `unwrap_used` lint.
    let _ = writeln!(out, "{file_path}:");
    for unit in units {
        let header = match &unit.kind {
            CohesionUnitKind::Inherent => format!("impl {}", unit.type_name),
            CohesionUnitKind::Trait { trait_name } => {
                format!("impl {trait_name} for {}", unit.type_name)
            }
        };
        let _ = writeln!(
            out,
            "- {header} (L{}-{}): LCOM4={}, {} method(s)",
            unit.start_line,
            unit.end_line,
            unit.lcom4(),
            unit.methods.len(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_src(rel: &str, source: &str) -> EditedSource {
        EditedSource {
            rel_path: rel.to_owned(),
            lang: SourceLang::Rust,
            source: source.to_owned(),
        }
    }

    #[test]
    fn no_report_when_every_impl_is_cohesive() {
        // Two methods that share field `n` → LCOM4 = 1.
        let src = rust_src(
            "lib.rs",
            r#"
struct Counter { n: i32 }
impl Counter {
    fn inc(&mut self) { self.n += 1; }
    fn get(&self) -> i32 { self.n }
}
"#,
        );
        let out = CohesionCore::new().run(&[src]).unwrap();
        assert!(out.is_none(), "expected no report, got {out:?}");
    }

    #[test]
    fn reports_split_impl_with_lcom4_above_one() {
        // Two disjoint field clusters → LCOM4 = 2.
        let src = rust_src(
            "lib.rs",
            r#"
struct Thing { a: i32, b: i32 }
impl Thing {
    fn ga(&self) -> i32 { self.a }
    fn gb(&self) -> i32 { self.b }
}
"#,
        );
        let out = CohesionCore::new()
            .run(&[src])
            .unwrap()
            .expect("expected a report");
        assert!(out.contains("lib.rs"), "should mention file: {out}");
        assert!(out.contains("impl Thing"), "should label impl: {out}");
        assert!(out.contains("LCOM4=2"), "should report lcom4: {out}");
        assert!(
            out.starts_with("agent-lens cohesion:"),
            "should have header: {out}",
        );
    }

    #[test]
    fn trait_impl_uses_trait_for_type_label() {
        // Two disjoint field clusters again so LCOM4 ≥ 2 and the unit
        // is surfaced — the test is about labelling, not about the
        // metric.
        let src = rust_src(
            "lib.rs",
            r#"
struct Thing { a: i32, b: i32 }
trait Bag {
    fn ga(&self) -> i32;
    fn gb(&self) -> i32;
}
impl Bag for Thing {
    fn ga(&self) -> i32 { self.a }
    fn gb(&self) -> i32 { self.b }
}
"#,
        );
        let out = CohesionCore::new()
            .run(&[src])
            .unwrap()
            .expect("expected a report");
        assert!(
            out.contains("impl Bag for Thing"),
            "should mention trait impl: {out}",
        );
    }

    #[test]
    fn aggregates_across_multiple_sources() {
        let split = r#"
struct Thing { a: i32, b: i32 }
impl Thing {
    fn ga(&self) -> i32 { self.a }
    fn gb(&self) -> i32 { self.b }
}
"#;
        let a = rust_src("a.rs", split);
        let b = rust_src("b.rs", split);
        let out = CohesionCore::new()
            .run(&[a, b])
            .unwrap()
            .expect("expected a report");
        assert!(out.contains("a.rs"));
        assert!(out.contains("b.rs"));
    }

    #[test]
    fn invalid_rust_surfaces_parse_error() {
        let src = rust_src("lib.rs", "fn ??? {");
        let err = CohesionCore::new().run(&[src]).unwrap_err();
        assert!(matches!(err, HookError::Parse(_)));
    }
}
