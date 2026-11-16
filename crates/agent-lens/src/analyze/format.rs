//! Small rendering helpers shared by more than one analyzer.

use std::fmt::Write as _;

use super::call_graph::model::ModuleResolutionSummary;

/// How many modules the resolution-confidence section lists.
const CONFIDENCE_TOP_MODULES: usize = 5;

/// Heading of the section [`render_module_confidence`] emits, shared with
/// [`ConfidenceDeduper`] so the two cannot drift apart.
const CONFIDENCE_HEADING: &str = "## Resolution confidence (worst modules)";

/// How much of a report a caller wants: `--detail`.
///
/// The same switch means the same thing for every analyzer. JSON is the
/// full report at either level; the levels differ in what is rendered
/// from it:
///
/// * markdown: `concise` leaves out the standing explanation a report
///   opens with ([`render_note`]) and caps each listing at
///   [`CONCISE_TOP`] rows unless `--top` says otherwise
///   ([`top_limit`]);
/// * the row formats: `concise` keeps a row's scalar fields only
///   ([`super::rows`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Detail {
    /// Rows and headline numbers only.
    Concise,
    /// Everything the format carries.
    #[default]
    Detailed,
}

/// Rows a markdown listing shows at [`Detail::Concise`] when `--top` is
/// not given.
pub(crate) const CONCISE_TOP: usize = 5;

thread_local! {
    static DETAIL: std::cell::Cell<Detail> = const { std::cell::Cell::new(Detail::Detailed) };
}

impl Detail {
    /// The level of the innermost live [`DetailScope`] on this thread,
    /// [`Detail::Detailed`] outside any.
    pub(crate) fn current() -> Self {
        DETAIL.get()
    }
}

/// RAII selection of the [`Detail`] level reports rendered on this
/// thread use, restoring the previous level on drop.
///
/// Ambient rather than a parameter for the reason
/// [`super::index::AnalysisIndexScope`] is: it is one decision about one
/// invocation, and threading it through every analyzer's options would
/// put the same field on thirty builders that never read it themselves.
#[derive(Debug)]
pub struct DetailScope {
    previous: Detail,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl DetailScope {
    pub fn enter(detail: Detail) -> Self {
        Self {
            previous: DETAIL.replace(detail),
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for DetailScope {
    fn drop(&mut self) {
        DETAIL.set(self.previous);
    }
}

/// Write a report's standing explanation — what its rows mean and how
/// far to trust them — as a paragraph of its own. It is the same text on
/// every run, so [`Detail::Concise`] leaves it out: a caller who asked
/// for the concise report has read it before, and JSON still carries it.
///
/// For a note the next paragraph follows without opening a blank line
/// of its own: the blank line that separates that paragraph from what
/// came before is written at either level, the note and the blank line
/// after it only at [`Detail::Detailed`]. [`render_note_tight`] is the
/// one for a note followed by a block that brings its own.
pub(crate) fn render_note(out: &mut String, note: &str) {
    out.push('\n');
    if Detail::current() == Detail::Detailed {
        let _ = writeln!(out, "{note}\n");
    }
}

/// [`render_note`] for a report whose next write opens with its own
/// blank line: the same paragraph without one after it, and nothing at
/// all at [`Detail::Concise`].
pub(crate) fn render_note_tight(out: &mut String, note: &str) {
    if Detail::current() == Detail::Detailed {
        let _ = writeln!(out, "\n{note}");
    }
}

/// The standing explanation as inline text, empty at [`Detail::Concise`]:
/// for reports that weave the note into a longer line.
pub(crate) fn note_text(note: &str) -> &str {
    match Detail::current() {
        Detail::Detailed => note,
        Detail::Concise => "",
    }
}

/// How many rows a markdown listing shows: `--top` when given, else the
/// analyzer's `default`, lowered to [`CONCISE_TOP`] at
/// [`Detail::Concise`].
pub(crate) fn top_limit(top: Option<usize>, default: usize) -> usize {
    top.unwrap_or(match Detail::current() {
        Detail::Detailed => default,
        Detail::Concise => default.min(CONCISE_TOP),
    })
}

/// [`top_limit`] for a listing with no default cap: `--top` when given,
/// [`CONCISE_TOP`] at [`Detail::Concise`], otherwise every row.
pub(crate) fn top_cap(top: Option<usize>) -> Option<usize> {
    top.or(match Detail::current() {
        Detail::Detailed => None,
        Detail::Concise => Some(CONCISE_TOP),
    })
}

/// The `--since` hint for a history listing's overflow line. Without a
/// window the suggestion is a concrete one; inside a window there is no
/// value to derive, so the flag is named and the choice left open.
pub(crate) fn since_hint(since: Option<&str>) -> &'static str {
    if since.is_some() {
        "a shorter `--since` window"
    } else {
        "`--since 90.days.ago`"
    }
}

/// The `--threshold` hint for a similarity listing's overflow line: the
/// score of the last row shown, or `None` when that is not above the
/// `current` threshold at the two decimals the flag is written to — a
/// hint that returns the same listing is worse than none.
pub(crate) fn threshold_hint(last_shown: f64, current: f64) -> Option<String> {
    let cents = |score: f64| (score * 100.0).round();
    (cents(last_shown) > cents(current)).then(|| format!("`--threshold {last_shown:.2}`"))
}

pub(crate) fn format_optional_f64(v: Option<f64>, precision: usize) -> String {
    match v {
        Some(x) => format!("{x:.precision$}"),
        None => "n/a".to_owned(),
    }
}

/// Close a report row: ` — caveat; caveat` when there are any, then
/// the newline.
pub(crate) fn end_row_with_caveats<'c>(out: &mut String, caveats: impl Iterator<Item = &'c str>) {
    let caveats: Vec<&str> = caveats.collect();
    if !caveats.is_empty() {
        let _ = write!(out, " — {}", caveats.join("; "));
    }
    out.push('\n');
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
    let _ = writeln!(out, "\n{CONFIDENCE_HEADING}\n");
    // What the undercount does to this analyzer's numbers is the same
    // sentence on every run; the rows are the finding.
    if Detail::current() == Detail::Detailed {
        let _ = writeln!(out, "{note}\n");
    }
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

/// Collapses repeated resolution-confidence listings across the sections
/// of one `agent-lens run` markdown report.
///
/// Every call-graph analyzer in a profile cites the same worst-resolved
/// modules — they all read one graph over one corpus — so from the second
/// section on the rows are context spent on nothing new. The dedupe keeps
/// each analyzer's note (it says what the shared uncertainty does to
/// *that* report) and replaces the repeated rows with a pointer at the
/// section that still carries them. Rows that differ are left alone:
/// a disagreement between two analyzers' confidence lists is signal.
#[derive(Debug, Default)]
pub struct ConfidenceDeduper {
    /// Section label and confidence rows of the first section seen — the
    /// copy every later duplicate points back at.
    first: Option<(String, Vec<String>)>,
}

impl ConfidenceDeduper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rewrite `body`'s confidence rows into a pointer at the earlier
    /// section when they repeat it row for row. Returns `None` when the
    /// body stands as-is: no confidence section, the first section seen
    /// (recorded as the one later duplicates cite), or rows that differ.
    pub fn dedupe(&mut self, label: &str, body: &str) -> Option<String> {
        let marker = format!("\n{CONFIDENCE_HEADING}\n");
        let start = body.find(&marker)?;
        let section_start = start + marker.len();
        // The section runs to the next heading or the end of the body.
        let section_end = body[section_start..]
            .find("\n#")
            .map_or(body.len(), |i| section_start + i);
        let section = &body[section_start..section_end];
        let rows: Vec<&str> = section
            .lines()
            .filter(|line| line.starts_with("- "))
            .collect();
        if rows.is_empty() {
            return None;
        }
        let Some((first_label, first_rows)) = &self.first else {
            self.first = Some((
                label.to_owned(),
                rows.iter().map(|&r| r.to_owned()).collect(),
            ));
            return None;
        };
        let same_rows = first_rows.len() == rows.len()
            && first_rows.iter().zip(&rows).all(|(a, b)| a.as_str() == *b);
        if !same_rows {
            return None;
        }
        // Keep everything up to the first row — the note — and swap the
        // rows for the pointer.
        let rows_offset = section.find("\n- ").map_or(section.len(), |i| i + 1);
        let mut out = String::with_capacity(body.len());
        out.push_str(&body[..section_start]);
        out.push_str(&section[..rows_offset]);
        let _ = writeln!(out, "Same worst modules as under `## {first_label}`.");
        out.push_str(&body[section_end..]);
        Some(out)
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
            let _ = writeln!(
                out,
                "- +{overflow} more (`--format jsonl` carries every row)"
            );
        }
    }
    render_overflow(out, groups.len(), shown, "module(s)");
}

/// Close a capped table, naming what was left out and how to see it.
/// Nothing is written when nothing was capped: "+0 more" reads as
/// truncation that never happened.
pub(crate) fn render_overflow(out: &mut String, total: usize, shown: usize, unit: &str) {
    render_overflow_narrowed(out, total, shown, unit, "");
}

/// [`render_overflow`] for a listing its analyzer can narrow: `narrow`
/// names the concrete flag that does it (`` `--min-score 8` ``), and is
/// put ahead of the generic ways out, because a narrower query is what
/// keeps the next report small where a higher `--top` only moves the
/// cut.
pub(crate) fn render_overflow_narrowed(
    out: &mut String,
    total: usize,
    shown: usize,
    unit: &str,
    narrow: &str,
) {
    let overflow = total.saturating_sub(shown);
    if overflow > 0 {
        let narrow = if narrow.is_empty() {
            String::new()
        } else {
            format!("{narrow}, ")
        };
        let _ = writeln!(
            out,
            "\n+{overflow} more {unit} not shown (--top {shown}). Narrow it: {narrow}pass a \
             sub-path, or raise `--top`; `--format jsonl` carries every row."
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

    /// What a writer puts into an empty buffer at `detail`.
    fn written_at(detail: Detail, write: impl FnOnce(&mut String)) -> String {
        let _scope = DetailScope::enter(detail);
        let mut out = String::new();
        write(&mut out);
        out
    }

    #[rstest]
    #[case::detailed(Detail::Detailed, "\nwhat the rows mean\n\n")]
    // The blank line the next paragraph leans on survives the note.
    #[case::concise(Detail::Concise, "\n")]
    fn render_note_keeps_the_paragraph_break_and_gates_the_note(
        #[case] detail: Detail,
        #[case] expected: &str,
    ) {
        let out = written_at(detail, |out| render_note(out, "what the rows mean"));
        assert_eq!(out, expected);
    }

    #[rstest]
    #[case::detailed(Detail::Detailed, "\nwhat the rows mean\n")]
    #[case::concise(Detail::Concise, "")]
    fn render_note_tight_writes_nothing_at_concise(#[case] detail: Detail, #[case] expected: &str) {
        let out = written_at(detail, |out| render_note_tight(out, "what the rows mean"));
        assert_eq!(out, expected);
    }

    #[rstest]
    #[case::detailed(Detail::Detailed, "inline note")]
    #[case::concise(Detail::Concise, "")]
    fn note_text_is_empty_at_concise(#[case] detail: Detail, #[case] expected: &str) {
        let _scope = DetailScope::enter(detail);
        assert_eq!(note_text("inline note"), expected);
    }

    #[rstest]
    #[case::detailed_default(Detail::Detailed, None, 20, 20)]
    #[case::detailed_explicit(Detail::Detailed, Some(3), 20, 3)]
    #[case::concise_lowers_the_default(Detail::Concise, None, 20, CONCISE_TOP)]
    // A default already under the concise cap is not raised to it.
    #[case::concise_keeps_a_smaller_default(Detail::Concise, None, 2, 2)]
    #[case::concise_explicit_above_the_cap(Detail::Concise, Some(50), 20, 50)]
    #[case::concise_explicit_zero(Detail::Concise, Some(0), 20, 0)]
    fn top_limit_prefers_the_flag_then_the_level(
        #[case] detail: Detail,
        #[case] top: Option<usize>,
        #[case] default: usize,
        #[case] expected: usize,
    ) {
        let _scope = DetailScope::enter(detail);
        assert_eq!(top_limit(top, default), expected);
    }

    #[rstest]
    #[case::detailed_is_uncapped(Detail::Detailed, None, None)]
    #[case::detailed_explicit(Detail::Detailed, Some(3), Some(3))]
    #[case::concise_caps(Detail::Concise, None, Some(CONCISE_TOP))]
    #[case::concise_explicit(Detail::Concise, Some(50), Some(50))]
    fn top_cap_is_uncapped_only_at_detailed_without_the_flag(
        #[case] detail: Detail,
        #[case] top: Option<usize>,
        #[case] expected: Option<usize>,
    ) {
        let _scope = DetailScope::enter(detail);
        assert_eq!(top_cap(top), expected);
    }

    #[test]
    fn the_level_is_detailed_outside_any_scope() {
        assert_eq!(Detail::current(), Detail::Detailed);
    }

    /// Scopes nest: each restores the level it found, not the default.
    #[test]
    fn a_scope_restores_the_level_it_replaced() {
        let outer = DetailScope::enter(Detail::Concise);
        assert_eq!(Detail::current(), Detail::Concise);
        {
            let _inner = DetailScope::enter(Detail::Detailed);
            assert_eq!(Detail::current(), Detail::Detailed);
            {
                let _innermost = DetailScope::enter(Detail::Concise);
                assert_eq!(Detail::current(), Detail::Concise);
            }
            assert_eq!(Detail::current(), Detail::Detailed);
        }
        assert_eq!(Detail::current(), Detail::Concise);
        drop(outer);
        assert_eq!(Detail::current(), Detail::Detailed);
    }

    #[rstest]
    #[case::narrowed(
        12,
        5,
        "`--min-score 8`",
        "\n+7 more row(s) not shown (--top 5). Narrow it: `--min-score 8`, pass a sub-path, \
         or raise `--top`; `--format jsonl` carries every row.\n"
    )]
    #[case::no_flag(
        12,
        5,
        "",
        "\n+7 more row(s) not shown (--top 5). Narrow it: pass a sub-path, or raise `--top`; \
         `--format jsonl` carries every row.\n"
    )]
    #[case::exactly_fits(5, 5, "`--min-score 8`", "")]
    #[case::under_the_cap(3, 5, "`--min-score 8`", "")]
    #[case::nothing_to_show(0, 5, "", "")]
    fn overflow_line_names_the_cut_and_the_narrowing_flag(
        #[case] total: usize,
        #[case] shown: usize,
        #[case] narrow: &str,
        #[case] expected: &str,
    ) {
        let mut out = String::new();
        render_overflow_narrowed(&mut out, total, shown, "row(s)", narrow);
        assert_eq!(out, expected);
    }

    /// The overflow line is a fact about the listing, not explanation:
    /// it is written at either level.
    #[test]
    fn overflow_line_is_written_at_concise() {
        let out = written_at(Detail::Concise, |out| render_overflow(out, 9, 5, "row(s)"));
        assert!(
            out.contains("+4 more row(s) not shown (--top 5)"),
            "got: {out}"
        );
    }

    #[rstest]
    #[case::unwindowed(None, "`--since 90.days.ago`")]
    #[case::windowed(Some("30.days.ago"), "a shorter `--since` window")]
    fn since_hint_is_concrete_only_without_a_window(
        #[case] since: Option<&str>,
        #[case] expected: &str,
    ) {
        assert_eq!(since_hint(since), expected);
    }

    #[rstest]
    #[case::above(0.93, 0.85, Some("`--threshold 0.93`"))]
    #[case::equal(0.85, 0.85, None)]
    // Equal at the two decimals the flag is written to: no narrower.
    #[case::equal_after_rounding(0.853, 0.85, None)]
    #[case::below(0.80, 0.85, None)]
    fn threshold_hint_needs_a_score_above_the_current_cut(
        #[case] last_shown: f64,
        #[case] current: f64,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(threshold_hint(last_shown, current).as_deref(), expected);
    }

    #[test]
    fn concise_keeps_the_confidence_rows_and_drops_their_note() {
        let modules = [summary("murky", 1, 3)];
        let out = written_at(Detail::Concise, |out| {
            render_module_confidence(out, &modules, "note text");
        });
        assert_eq!(
            out,
            "\n## Resolution confidence (worst modules)\n\n\
             - `murky`: 3/4 call sites not resolved (75%)\n",
        );
    }

    /// The fold reads rows from wherever they start, so it holds when
    /// the note above them is gone.
    #[test]
    fn deduper_folds_repeated_rows_at_concise() {
        let _scope = DetailScope::enter(Detail::Concise);
        let modules = [summary("murky", 1, 3)];
        let mut deduper = ConfidenceDeduper::new();
        assert_eq!(
            deduper.dedupe("delegation", &body_with_confidence(&modules, "n")),
            None
        );
        let folded = deduper
            .dedupe("layers", &body_with_confidence(&modules, "n"))
            .unwrap();
        assert!(
            folded.contains("Same worst modules as under `## delegation`."),
            "got: {folded}",
        );
        assert!(!folded.contains("- `murky`"), "got: {folded}");
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

    /// A report body ending in a confidence section, the shape every
    /// call-graph analyzer produces.
    fn body_with_confidence(modules: &[ModuleResolutionSummary], note: &str) -> String {
        let mut body = String::from("# Some report\n\n## Findings\n\n- a finding\n");
        render_module_confidence(&mut body, modules, note);
        body
    }

    #[test]
    fn deduper_folds_repeated_rows_and_keeps_each_note() {
        let modules = [summary("murky", 1, 3), summary("dim", 2, 2)];
        let mut deduper = ConfidenceDeduper::new();

        let first = body_with_confidence(&modules, "delegation note");
        assert_eq!(deduper.dedupe("delegation", &first), None);

        let second = body_with_confidence(&modules, "layers note");
        let folded = deduper.dedupe("layers", &second).unwrap();
        assert!(folded.contains("## Resolution confidence"), "got: {folded}");
        assert!(folded.contains("layers note"), "got: {folded}");
        assert!(
            folded.contains("Same worst modules as under `## delegation`."),
            "got: {folded}",
        );
        assert!(!folded.contains("- `murky`"), "got: {folded}");
        // Everything before the confidence section is untouched.
        assert!(folded.contains("- a finding"), "got: {folded}");
    }

    #[test]
    fn deduper_leaves_sections_whose_rows_differ() {
        let mut deduper = ConfidenceDeduper::new();
        let first = body_with_confidence(&[summary("murky", 1, 3)], "note");
        assert_eq!(deduper.dedupe("delegation", &first), None);
        let second = body_with_confidence(&[summary("other", 0, 2)], "note");
        assert_eq!(deduper.dedupe("layers", &second), None);
    }

    /// A section-less body between two matching ones must not disturb the
    /// recorded rows: analyzers without a call graph sit between the ones
    /// with one in every real profile.
    #[test]
    fn deduper_skips_bodies_without_a_confidence_section() {
        let modules = [summary("murky", 1, 3)];
        let mut deduper = ConfidenceDeduper::new();
        assert_eq!(
            deduper.dedupe("delegation", &body_with_confidence(&modules, "n")),
            None
        );
        assert_eq!(
            deduper.dedupe("complexity", "# Complexity report\n\n- rows\n"),
            None
        );
        assert!(
            deduper
                .dedupe("layers", &body_with_confidence(&modules, "n"))
                .is_some(),
        );
    }

    /// The parser stops at the next heading, so a section that is not the
    /// body's tail keeps whatever follows it.
    #[test]
    fn deduper_preserves_content_after_the_section() {
        let modules = [summary("murky", 1, 3)];
        let mut deduper = ConfidenceDeduper::new();
        assert_eq!(
            deduper.dedupe("delegation", &body_with_confidence(&modules, "n")),
            None
        );
        let mut second = body_with_confidence(&modules, "n");
        second.push_str("\n## Trailing section\n\n- kept\n");
        let folded = deduper.dedupe("layers", &second).unwrap();
        assert!(folded.contains("## Trailing section"), "got: {folded}");
        assert!(folded.contains("- kept"), "got: {folded}");
        assert!(!folded.contains("- `murky`"), "got: {folded}");
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
            out.contains("+1 more module(s) not shown (--top 2). Narrow it: pass a sub-path"),
            "got: {out}",
        );
    }

    #[test]
    fn the_per_module_cap_truncates_and_points_at_the_json() {
        let out = sections(&[group("a", 5)], 10, 2);
        assert_eq!(out.matches("- item ").count(), 2, "got: {out}");
        assert!(
            out.contains("- +3 more (`--format jsonl` carries every row)"),
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
            out.contains("- +5 more (`--format jsonl` carries every row)"),
            "got: {out}",
        );
    }

    #[test]
    fn an_empty_group_list_still_states_the_zero() {
        let out = sections(&[], 10, 10);
        assert_eq!(out, "\n## Heading (note; 0 of 0 module(s))\n");
    }
}
