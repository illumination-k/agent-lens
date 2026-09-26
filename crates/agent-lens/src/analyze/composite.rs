//! Shared rendering for analyzers that bundle several sections.
//!
//! `reach` and `narrowable` each answer one question from several angles,
//! and each angle is a full analyzer report in its own right. Rather than
//! flatten those reports into a new shape, a bundle runs the section
//! analyzers under one [`AnalysisIndexScope`] — so the call graph and the
//! per-file facts they share are built once — and stacks their reports:
//! JSON nests each report under its section key, markdown demotes each
//! report's headings one level under a bundle heading.

use std::fmt::Write as _;

use serde_json::{Map, Value};

use super::format::ConfidenceDeduper;
use super::index::AnalysisIndex;
use super::{AnalysisIndexScope, AnalyzerError, OutputFormat};

const SCHEMA_VERSION: u32 = 1;

/// One section of a bundle: its JSON key, its CLI spelling, and the
/// section analyzer's report in the bundle's output format.
struct Section {
    key: &'static str,
    name: &'static str,
    body: String,
}

/// A section selector of a bundled analyzer. Declaration order is the
/// report order.
pub(crate) trait BundleSection: Copy + Ord + 'static {
    /// Every section, in report order.
    const ALL: &'static [Self];

    /// The JSON key the section's report is nested under, and the
    /// section's CLI and config spelling.
    fn labels(self) -> (&'static str, &'static str);
}

/// Run the `chosen` sections (all of them when none is chosen) through
/// `run` under one analysis index, and stack the reports.
pub(super) fn run_bundle<S: BundleSection>(
    title: &str,
    note: &str,
    chosen: &[S],
    format: OutputFormat,
    run: impl Fn(S) -> Result<String, AnalyzerError>,
) -> Result<String, AnalyzerError> {
    with_index(|| {
        let sections = selected(chosen, S::ALL)
            .into_iter()
            .map(|section| {
                let (key, name) = section.labels();
                Ok(Section {
                    key,
                    name,
                    body: run(section)?,
                })
            })
            .collect::<Result<Vec<_>, AnalyzerError>>()?;
        render_sections(title, note, sections, format)
    })
}

/// Run `f` under an analysis index scope, activating one only when the
/// caller has not: the CLI and the profile runner already hold one whose
/// cache should outlive this bundle, while a library caller gets the
/// sharing without having to know about it.
fn with_index<R>(run: impl FnOnce() -> R) -> R {
    let _scope = AnalysisIndex::active()
        .is_none()
        .then(AnalysisIndexScope::activate);
    run()
}

/// The sections a bundle reports: `chosen` in declaration order, each
/// once, or every one of `all` when nothing was chosen.
fn selected<T: Copy + Ord>(chosen: &[T], all: &[T]) -> Vec<T> {
    if chosen.is_empty() {
        return all.to_vec();
    }
    let mut sections = chosen.to_vec();
    sections.sort_unstable();
    sections.dedup();
    sections
}

/// Stack section reports into one bundle report.
///
/// `title` heads the markdown; `note` is the one paragraph that says how
/// the sections relate, since each section's own note only speaks for
/// itself.
fn render_sections(
    title: &str,
    note: &str,
    sections: Vec<Section>,
    format: OutputFormat,
) -> Result<String, AnalyzerError> {
    match format {
        OutputFormat::Json => render_json(sections),
        OutputFormat::Md => Ok(render_markdown(title, note, &sections)),
    }
}

fn render_json(sections: Vec<Section>) -> Result<String, AnalyzerError> {
    let mut out = Map::new();
    out.insert("schema_version".to_owned(), Value::from(SCHEMA_VERSION));
    out.insert(
        "sections".to_owned(),
        Value::from(sections.iter().map(|s| s.key).collect::<Vec<_>>()),
    );
    for section in sections {
        let report: Value = serde_json::from_str(&section.body)?;
        out.insert(section.key.to_owned(), report);
    }
    serde_json::to_string_pretty(&Value::Object(out)).map_err(AnalyzerError::Serialize)
}

fn render_markdown(title: &str, note: &str, sections: &[Section]) -> String {
    let names: Vec<&str> = sections.iter().map(|s| s.name).collect();
    let mut out = format!("# {title} ({})\n\n{note}\n", names.join(", "));
    // Every section reads the same call graph, so from the second one on
    // the resolution-confidence rows repeat verbatim; fold them into a
    // pointer the way a profile run does.
    let mut deduper = ConfidenceDeduper::new();
    for section in sections {
        let deduped = deduper.dedupe(section.name, &section.body);
        let body = deduped.as_deref().unwrap_or(&section.body);
        out.push('\n');
        push_demoted(&mut out, body.trim_end_matches('\n'));
        out.push('\n');
    }
    out
}

/// Append `body` with every ATX heading pushed one level down, so a
/// section report's `#` title nests under the bundle's.
fn push_demoted(out: &mut String, body: &str) {
    for line in body.lines() {
        if line.starts_with('#') {
            out.push('#');
        }
        let _ = writeln!(out, "{line}");
    }
    // `writeln!` leaves a trailing newline the caller adds itself.
    out.pop();
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn section(key: &'static str, body: &str) -> Section {
        Section {
            key,
            name: key,
            body: body.to_owned(),
        }
    }

    #[rstest]
    #[case(&[], &[1, 2, 3])]
    #[case(&[3, 1, 3], &[1, 3])]
    fn selected_orders_dedups_and_defaults_to_all(#[case] chosen: &[u8], #[case] expected: &[u8]) {
        assert_eq!(selected(chosen, &[1, 2, 3]), expected);
    }

    #[test]
    fn json_nests_each_report_under_its_key_and_lists_the_order() {
        let out = render_sections(
            "Bundle",
            "note",
            vec![section("b", r#"{"x":1}"#), section("a", r#"{"y":2}"#)],
            OutputFormat::Json,
        )
        .unwrap();
        let json: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["sections"], serde_json::json!(["b", "a"]));
        assert_eq!(json["b"]["x"], 1);
        assert_eq!(json["a"]["y"], 2);
    }

    #[test]
    fn markdown_demotes_section_headings_under_the_bundle_title() {
        let out = render_sections(
            "Bundle",
            "How the sections relate.",
            vec![
                section("one", "# One\n\nbody\n\n## Sub\n- row\n"),
                section("two", "# Two\n"),
            ],
            OutputFormat::Md,
        )
        .unwrap();
        assert_eq!(
            out,
            "# Bundle (one, two)\n\nHow the sections relate.\n\n\
             ## One\n\nbody\n\n### Sub\n- row\n\n## Two\n",
        );
    }

    #[rstest]
    #[case("plain", "plain")]
    #[case("# h", "## h")]
    #[case("a\n## b\nc", "a\n### b\nc")]
    fn push_demoted_only_touches_heading_lines(#[case] body: &str, #[case] expected: &str) {
        let mut out = String::new();
        push_demoted(&mut out, body);
        assert_eq!(out, expected);
    }

    #[test]
    fn markdown_folds_repeated_confidence_rows() {
        let body = |title: &str| {
            format!(
                "# {title}\n\n## Resolution confidence (worst modules)\n\nnote\n\n- `m`: 1/2 call sites not resolved (50%)\n"
            )
        };
        let out = render_sections(
            "Bundle",
            "n",
            vec![section("one", &body("One")), section("two", &body("Two"))],
            OutputFormat::Md,
        )
        .unwrap();
        assert_eq!(out.matches("- `m`: 1/2").count(), 1, "{out}");
    }

    #[test]
    fn with_index_activates_a_scope_only_when_none_is_active() {
        assert!(AnalysisIndex::active().is_none());
        with_index(|| assert!(AnalysisIndex::active().is_some()));
        assert!(AnalysisIndex::active().is_none());
        let outer = AnalysisIndexScope::activate();
        with_index(|| {
            let active = AnalysisIndex::active().unwrap();
            assert_eq!(
                std::sync::Arc::as_ptr(&active),
                std::ptr::from_ref(outer.index())
            );
        });
    }
}
