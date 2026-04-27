//! Engine-agnostic per-function complexity report for PreToolUse hooks.
//!
//! Mirrors [`super::similarity`] and [`super::wrapper`]: the hook adapters
//! call [`ComplexityCore::run`] with the files about to be touched and get
//! back a fully-formatted report — or `None` when nothing crossed the
//! "non-trivial" thresholds. Each agent then wraps that string in the
//! engine-specific output envelope.

use std::fmt::Write as _;

use lens_domain::FunctionComplexity;

use crate::analyze::SourceLang;
use crate::hooks::core::{EditedSource, HookError};

/// Cognitive complexity at or above which a function is reported.
const COGNITIVE_FLOOR: u32 = 8;
/// Cyclomatic complexity at or above which a function is reported.
const CYCLOMATIC_FLOOR: u32 = 10;
/// Max nesting depth at or above which a function is reported.
const NESTING_FLOOR: u32 = 4;
/// Cap on functions surfaced per file. The hook is meant to inject a
/// short signal, not the full report — `analyze complexity` is the
/// right tool when an agent wants the full picture.
const TOP_PER_FILE: usize = 5;

/// Runner for the pre-edit complexity hook.
#[derive(Debug, Clone, Default)]
pub(crate) struct ComplexityCore;

impl ComplexityCore {
    pub fn new() -> Self {
        Self
    }

    /// Analyse every source and produce a single report.
    ///
    /// Returns `Ok(None)` when no file produced any non-trivial finding,
    /// so callers can treat "no signal" as "no message" without
    /// inspecting the report string.
    pub fn run(&self, sources: &[EditedSource]) -> Result<Option<String>, HookError> {
        let mut body = String::new();
        let mut total = 0usize;

        for src in sources {
            let funcs = extract_functions(src.lang, &src.source)?;
            let flagged = pick_flagged(&funcs);
            if flagged.is_empty() {
                continue;
            }
            total += flagged.len();
            append_section(&mut body, &src.rel_path, &flagged);
        }

        if total == 0 {
            return Ok(None);
        }

        let header =
            format!("agent-lens complexity: {total} non-trivial function(s) before edit\n");
        Ok(Some(format!("{header}{body}")))
    }
}

fn extract_functions(lang: SourceLang, source: &str) -> Result<Vec<FunctionComplexity>, HookError> {
    match lang {
        SourceLang::Rust => {
            lens_rust::extract_complexity_units(source).map_err(|e| HookError::Parse(Box::new(e)))
        }
        SourceLang::TypeScript => {
            lens_ts::extract_complexity_units(source).map_err(|e| HookError::Parse(Box::new(e)))
        }
        SourceLang::Python => {
            lens_py::extract_complexity_units(source).map_err(|e| HookError::Parse(Box::new(e)))
        }
    }
}

fn is_flagged(f: &FunctionComplexity) -> bool {
    f.cognitive >= COGNITIVE_FLOOR
        || f.cyclomatic >= CYCLOMATIC_FLOOR
        || f.max_nesting >= NESTING_FLOOR
}

/// Pick functions worth surfacing, ranked the same way `analyze
/// complexity --format md` ranks: cognitive first, then cyclomatic, then
/// earliest line. Cognitive penalises nesting and short-circuit chains
/// the way a human reader does, so it is the closer signal to "this
/// function is risky to touch" than raw cyclomatic.
fn pick_flagged(funcs: &[FunctionComplexity]) -> Vec<&FunctionComplexity> {
    let mut flagged: Vec<&FunctionComplexity> = funcs.iter().filter(|f| is_flagged(f)).collect();
    flagged.sort_by(|a, b| {
        b.cognitive
            .cmp(&a.cognitive)
            .then_with(|| b.cyclomatic.cmp(&a.cyclomatic))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    flagged.truncate(TOP_PER_FILE);
    flagged
}

fn append_section(out: &mut String, file_path: &str, funcs: &[&FunctionComplexity]) {
    // writeln! into a String cannot fail; the result is swallowed
    // deliberately rather than unwrapped to satisfy the workspace's
    // `unwrap_used` lint.
    let _ = writeln!(out, "{file_path}:");
    for f in funcs {
        let _ = writeln!(
            out,
            "- {} (L{}-{}): cc={}, cog={}, nest={}",
            f.name, f.start_line, f.end_line, f.cyclomatic, f.cognitive, f.max_nesting,
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
    fn no_report_when_nothing_is_non_trivial() {
        // Trivial getters: cog=0, cc=1, nest=0.
        let src = rust_src(
            "lib.rs",
            "fn alpha() -> i32 { 1 }\nfn beta() -> i32 { 2 }\n",
        );
        let out = ComplexityCore::new().run(&[src]).unwrap();
        assert!(out.is_none(), "expected no report, got {out:?}");
    }

    #[test]
    fn reports_function_above_cognitive_floor() {
        // A nested-if pyramid: cognitive grows with nesting, so this
        // crosses COGNITIVE_FLOOR even though the cyclomatic count is
        // modest.
        let src = rust_src(
            "lib.rs",
            r#"
fn nested(n: i32) -> i32 {
    if n > 0 {
        if n > 1 {
            if n > 2 {
                if n > 3 {
                    return n;
                }
            }
        }
    }
    0
}
"#,
        );
        let out = ComplexityCore::new()
            .run(&[src])
            .unwrap()
            .expect("expected a report");
        assert!(out.contains("lib.rs"), "should mention file: {out}");
        assert!(out.contains("nested"), "should mention function: {out}");
        assert!(out.contains("cog="), "should include cognitive: {out}");
        assert!(
            out.starts_with("agent-lens complexity:"),
            "should have header: {out}",
        );
    }

    #[test]
    fn caps_per_file_at_top_n() {
        // Six functions, each with nesting depth 4 (above NESTING_FLOOR).
        // Only TOP_PER_FILE = 5 should appear in the report.
        let body = r#"if n > 0 { if n > 1 { if n > 2 { if n > 3 { return n; } } } } 0"#;
        let mut src = String::new();
        for i in 0..6 {
            let _ = writeln!(src, "fn f{i}(n: i32) -> i32 {{ {body} }}\n");
        }
        let edited = rust_src("lib.rs", &src);
        let out = ComplexityCore::new()
            .run(&[edited])
            .unwrap()
            .expect("expected a report");
        let body_lines = out.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(body_lines, TOP_PER_FILE, "got {out}");
    }

    #[test]
    fn aggregates_across_multiple_sources() {
        let nested = r#"
fn nested(n: i32) -> i32 {
    if n > 0 { if n > 1 { if n > 2 { if n > 3 { return n; } } } }
    0
}
"#;
        let a = rust_src("a.rs", nested);
        let b = rust_src("b.rs", nested);
        let out = ComplexityCore::new()
            .run(&[a, b])
            .unwrap()
            .expect("expected a report");
        assert!(out.contains("a.rs"));
        assert!(out.contains("b.rs"));
    }

    #[test]
    fn invalid_rust_surfaces_parse_error() {
        let src = rust_src("lib.rs", "fn ??? {");
        let err = ComplexityCore::new().run(&[src]).unwrap_err();
        assert!(matches!(err, HookError::Parse(_)));
    }
}
