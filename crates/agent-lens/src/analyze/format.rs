//! Small rendering helpers shared by more than one analyzer.

use std::fmt::Write as _;

use super::call_graph::model::ModuleResolutionSummary;

/// How many modules the resolution-confidence section lists.
const CONFIDENCE_TOP_MODULES: usize = 5;

pub(crate) fn format_optional_f64(v: Option<f64>, precision: usize) -> String {
    match v {
        Some(x) => format!("{x:.precision$}"),
        None => "n/a".to_owned(),
    }
}

/// Cite the graph-confidence calibration: the modules whose call sites
/// resolved worst, i.e. where this analyzer's numbers are most
/// undercounted. `note` says what specifically is undercounted, since
/// every call-graph analyzer measures something different.
///
/// Emits nothing when every module resolved completely — a section full
/// of zeroes is noise in an agent's context.
pub(crate) fn render_module_confidence(
    out: &mut String,
    modules: &[ModuleResolutionSummary],
    note: &str,
) {
    // A module with anything left unresolved necessarily has at least
    // one call site, so this one comparison also guards the division
    // below.
    let mut worst: Vec<&ModuleResolutionSummary> = modules
        .iter()
        .filter(|m| m.calls.resolved_call_count < m.total_call_count)
        .collect();
    if worst.is_empty() {
        return;
    }
    let unresolved_share = |m: &ModuleResolutionSummary| {
        (m.total_call_count - m.calls.resolved_call_count) as f64 / m.total_call_count as f64
    };
    worst.sort_by(|a, b| {
        unresolved_share(b)
            .total_cmp(&unresolved_share(a))
            .then_with(|| b.total_call_count.cmp(&a.total_call_count))
            .then_with(|| a.module.cmp(&b.module))
    });
    let _ = writeln!(
        out,
        "\n## Resolution confidence (worst modules)\n\n{note}\n"
    );
    for m in worst.iter().take(CONFIDENCE_TOP_MODULES) {
        let unresolved = m.total_call_count - m.calls.resolved_call_count;
        let _ = writeln!(
            out,
            "- `{}`: {}/{} call sites not resolved ({:.0}%)",
            m.module,
            unresolved,
            m.total_call_count,
            unresolved_share(m) * 100.0,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::call_graph::model::ResolutionCallCounts;
    use rstest::rstest;

    fn summary(module: &str, resolved: usize, unresolved: usize) -> ModuleResolutionSummary {
        ModuleResolutionSummary {
            module: module.to_owned(),
            calls: ResolutionCallCounts {
                resolved_call_count: resolved,
                unresolved_call_count: unresolved,
                ..ResolutionCallCounts::default()
            },
            total_call_count: resolved + unresolved,
        }
    }

    fn render(modules: &[ModuleResolutionSummary]) -> String {
        let mut out = String::new();
        render_module_confidence(&mut out, modules, "note text");
        out
    }

    #[rstest]
    #[case::no_modules(vec![])]
    #[case::every_call_resolved(vec![summary("a", 4, 0), summary("b", 1, 0)])]
    #[case::no_call_sites_at_all(vec![summary("a", 0, 0)])]
    fn nothing_is_rendered_without_unresolved_calls(#[case] modules: Vec<ModuleResolutionSummary>) {
        assert_eq!(render(&modules), "");
    }

    #[test]
    fn fully_resolved_modules_are_left_out_of_the_listing() {
        let out = render(&[summary("clean", 9, 0), summary("murky", 1, 3)]);
        assert!(out.contains("## Resolution confidence"), "got: {out}");
        assert!(out.contains("note text"), "got: {out}");
        assert!(
            out.contains("- `murky`: 3/4 call sites not resolved (75%)"),
            "got: {out}",
        );
        assert!(!out.contains("clean"), "got: {out}");
    }

    #[test]
    fn modules_rank_by_unresolved_share_then_volume_then_name() {
        let out = render(&[
            summary("small_but_bad", 0, 2),
            summary("big_and_bad", 0, 20),
            summary("half", 5, 5),
            summary("also_half", 5, 5),
        ]);
        let listed: Vec<&str> = out
            .lines()
            .filter_map(|l| l.strip_prefix("- `"))
            .filter_map(|l| l.split('`').next())
            .collect();
        assert_eq!(
            listed,
            ["big_and_bad", "small_but_bad", "also_half", "half"],
        );
    }

    #[test]
    fn the_listing_is_capped_at_five_modules() {
        let modules: Vec<ModuleResolutionSummary> =
            (0..8).map(|i| summary(&format!("m{i}"), 0, 1)).collect();
        assert_eq!(
            render(&modules)
                .lines()
                .filter(|l| l.starts_with("- `"))
                .count(),
            CONFIDENCE_TOP_MODULES,
        );
    }

    #[rstest]
    #[case(Some(1.5), 2, "1.50")]
    #[case(Some(1.5), 0, "2")]
    #[case(None, 2, "n/a")]
    fn optional_f64_formats_or_says_not_available(
        #[case] value: Option<f64>,
        #[case] precision: usize,
        #[case] expected: &str,
    ) {
        assert_eq!(format_optional_f64(value, precision), expected);
    }
}
