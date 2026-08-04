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

/// Backticked items up to `cap`, the remainder rolled into a count.
///
/// Every analyzer that inlines a name list into a report row needs the
/// same shape — a row that grows without bound costs more context than
/// the finding is worth — so the cap belongs to the caller and the
/// rendering belongs here.
pub(crate) fn render_backticked_list(items: &[String], cap: usize) -> String {
    let listed = items
        .iter()
        .take(cap)
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let overflow = items.len().saturating_sub(cap);
    if overflow > 0 {
        format!("{listed} +{overflow} more")
    } else {
        listed
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

/// One module's worth of findings in a per-module markdown listing.
/// Implemented by the analyzer's own group type so
/// [`render_module_sections`] owns the truncation bookkeeping while the
/// analyzer keeps its own wording and row shape.
pub(crate) trait ModuleSection {
    /// Module path, rendered as the section heading.
    fn module(&self) -> &str;

    /// Total findings in this module, including any not rendered — the
    /// count the overflow line is computed against.
    fn item_count(&self) -> usize;

    /// Prose after the module name in the heading, e.g.
    /// `"7 function(s), 210 LOC"`.
    fn heading_detail(&self) -> String;

    /// Write up to `limit` findings, one markdown bullet each.
    fn render_items(&self, out: &mut String, limit: usize);
}

/// Render a "findings grouped by module" listing: an `## ` heading
/// stating how much of the corpus is shown, one `### ` section per
/// module, and the two overflow lines that tell an agent where the rest
/// went. Both caps truncate rather than drop silently, and both say
/// where the untruncated data lives.
///
/// `heading` is the section title including its ordering note, e.g.
/// `"Untested by module (largest body first"` — the counts and closing
/// parenthesis are appended.
pub(crate) fn render_module_sections<G: ModuleSection>(
    out: &mut String,
    heading: &str,
    groups: &[G],
    module_limit: usize,
    items_per_module: usize,
) {
    let shown = groups.len().min(module_limit);
    let _ = writeln!(
        out,
        "\n## {heading}; {shown} of {} module(s))",
        groups.len(),
    );
    for group in groups.iter().take(module_limit) {
        let _ = writeln!(
            out,
            "\n### `{}` — {}",
            group.module(),
            group.heading_detail(),
        );
        group.render_items(out, items_per_module);
        let overflow = group.item_count().saturating_sub(items_per_module);
        if overflow > 0 {
            let _ = writeln!(out, "- +{overflow} more (JSON output carries every row)");
        }
    }
    let module_overflow = groups.len() - shown;
    if module_overflow > 0 {
        let _ = writeln!(
            out,
            "\n+{module_overflow} more module(s) not shown (raise `--top`; JSON carries every \
             row)."
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

    #[rstest]
    #[case::empty(&[], 3, "")]
    #[case::under_cap(&["a", "b"], 3, "`a`, `b`")]
    #[case::exactly_at_cap(&["a", "b"], 2, "`a`, `b`")]
    #[case::over_cap(&["a", "b", "c", "d"], 2, "`a`, `b` +2 more")]
    // A cap of zero rolls everything into the count rather than
    // panicking on the empty join.
    #[case::zero_cap(&["a"], 0, " +1 more")]
    fn backticked_list_caps_and_counts_the_remainder(
        #[case] items: &[&str],
        #[case] cap: usize,
        #[case] expected: &str,
    ) {
        let items: Vec<String> = items.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(render_backticked_list(&items, cap), expected);
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

    /// A group carrying more findings than it renders — the case both
    /// overflow lines exist for.
    struct Group {
        module: &'static str,
        item_count: usize,
        rendered: usize,
    }

    impl ModuleSection for Group {
        fn module(&self) -> &str {
            self.module
        }

        fn item_count(&self) -> usize {
            self.item_count
        }

        fn heading_detail(&self) -> String {
            format!("{} item(s)", self.item_count)
        }

        fn render_items(&self, out: &mut String, limit: usize) {
            for i in 0..self.rendered.min(limit) {
                let _ = writeln!(out, "- item {i}");
            }
        }
    }

    fn group(module: &'static str, item_count: usize) -> Group {
        Group {
            module,
            item_count,
            rendered: item_count,
        }
    }

    fn sections(groups: &[Group], module_limit: usize, items_per_module: usize) -> String {
        let mut out = String::new();
        render_module_sections(
            &mut out,
            "Heading (note",
            groups,
            module_limit,
            items_per_module,
        );
        out
    }

    #[test]
    fn every_group_and_item_renders_when_nothing_is_capped() {
        let out = sections(&[group("a", 2), group("b", 1)], 10, 10);
        assert!(
            out.contains("## Heading (note; 2 of 2 module(s))"),
            "got: {out}"
        );
        assert!(out.contains("### `a` — 2 item(s)"), "got: {out}");
        assert!(out.contains("### `b` — 1 item(s)"), "got: {out}");
        assert_eq!(out.matches("- item ").count(), 3, "got: {out}");
        assert!(!out.contains("more"), "no overflow line is due, got: {out}");
    }

    #[test]
    fn the_module_cap_truncates_and_says_how_many_are_left() {
        let out = sections(&[group("a", 1), group("b", 1), group("c", 1)], 2, 10);
        assert!(
            out.contains("## Heading (note; 2 of 3 module(s))"),
            "got: {out}"
        );
        assert!(!out.contains("`c`"), "got: {out}");
        assert!(
            out.contains("+1 more module(s) not shown (raise `--top`"),
            "got: {out}",
        );
    }

    #[test]
    fn the_per_module_cap_truncates_and_points_at_the_json() {
        let out = sections(&[group("a", 5)], 10, 2);
        assert_eq!(out.matches("- item ").count(), 2, "got: {out}");
        assert!(
            out.contains("- +3 more (JSON output carries every row)"),
            "got: {out}",
        );
    }

    /// The overflow count is computed from `item_count`, not from how
    /// many rows the group chose to write, so a group holding rows it
    /// declines to render still accounts for them.
    #[test]
    fn the_overflow_count_follows_the_declared_item_count() {
        let out = sections(
            &[Group {
                module: "a",
                item_count: 9,
                rendered: 1,
            }],
            10,
            4,
        );
        assert_eq!(out.matches("- item ").count(), 1, "got: {out}");
        assert!(
            out.contains("- +5 more (JSON output carries every row)"),
            "got: {out}",
        );
    }

    #[test]
    fn an_empty_group_list_still_states_the_zero() {
        let out = sections(&[], 10, 10);
        assert_eq!(out, "\n## Heading (note; 0 of 0 module(s))\n");
    }
}
