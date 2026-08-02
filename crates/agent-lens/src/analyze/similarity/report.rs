use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use lens_domain::SimilarCluster;
use serde::Serialize;

use super::{OwnedUnit, SimilarityComponents, SimilarityTarget};

/// Longest representative snippet rendered for a block cluster. Long
/// enough to show the repeated shape, short enough that a 40-cluster
/// report still fits an agent's context window.
const MAX_SNIPPET_LINES: usize = 12;

/// How many files a block cluster's occurrence breakdown names before it
/// collapses the tail into a count.
const MAX_BREAKDOWN_FILES: usize = 6;

#[derive(Debug, Serialize)]
pub(super) struct Report<'a> {
    /// Input path: a single source file, or the root directory walked.
    root: String,
    /// Body-scoring algorithm used: `tsed` or `token`. Surfaced because
    /// the two methods are not on the same score scale.
    method: &'static str,
    /// Comparison unit: `functions`, `types`, or `blocks`.
    target: &'static str,
    unit_count: usize,
    /// Clustering cut applied. In sweep mode this is the ladder floor;
    /// otherwise the plain `--threshold`.
    threshold: f64,
    /// Multi-threshold sweep ladder (ascending), when `--sweep` is active.
    /// Present so a consumer can see which rungs the per-cluster
    /// `survives_at_threshold` annotations were drawn from.
    #[serde(skip_serializing_if = "Option::is_none")]
    sweep: Option<Vec<f64>>,
    min_lines: usize,
    cluster_count: usize,
    clusters: &'a [ClusterView<'a>],
}

impl<'a> Report<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        path: &Path,
        method: &'static str,
        target: &'static str,
        threshold: f64,
        min_lines: usize,
        unit_count: usize,
        sweep: Option<&[f64]>,
        clusters: &'a [ClusterView<'a>],
    ) -> Self {
        Self {
            root: path.display().to_string(),
            method,
            target,
            unit_count,
            threshold,
            sweep: sweep.map(<[f64]>::to_vec),
            min_lines,
            cluster_count: clusters.len(),
            clusters,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct ClusterView<'a> {
    size: usize,
    min_similarity: f64,
    max_similarity: f64,
    /// Highest sweep rung at which this cluster survives intact, set only
    /// in `--sweep` mode. `None` (and omitted from JSON) otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    survives_at_threshold: Option<f64>,
    units: Vec<UnitRef<'a>>,
    pairs: Vec<PairView<'a>>,
    /// Source lines of the cluster's representative occurrence, for
    /// `--target blocks`. A block has no name to look up, so without the
    /// text a reader has to open every file to see what repeated.
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<Vec<String>>,
    /// Corpus indices of the members, kept for snippet resolution.
    /// Not serialized: the `units` list is the public shape, and raw
    /// indices into a corpus the consumer never sees mean nothing.
    #[serde(skip)]
    members: Vec<usize>,
}

impl<'a> ClusterView<'a> {
    pub(super) fn from_domain(
        corpus: &'a [OwnedUnit],
        cluster: SimilarCluster,
        pair_scores: &HashMap<(usize, usize), SimilarityComponents>,
        target: SimilarityTarget,
    ) -> Self {
        // Members and units are built in one pass so they stay index
        // aligned: `representative` reads the corpus index that goes
        // with a chosen unit, and a `filter_map` that dropped one side
        // only would silently quote the wrong occurrence.
        let (members, units): (Vec<usize>, Vec<UnitRef<'a>>) = cluster
            .members
            .iter()
            .filter_map(|i| corpus.get(*i).map(|unit| (*i, UnitRef::from(unit))))
            .unzip();
        // Block clusters routinely run to dozens of members, and a
        // pairwise view list is quadratic in that: 55 occurrences would
        // emit 1,485 pair objects carrying nothing the min/max band does
        // not already say — blocks have no signature, so every optional
        // component on them is null. The band plus the unit list is the
        // whole of the information.
        let pairs = match target {
            SimilarityTarget::Blocks => Vec::new(),
            _ => cluster_pair_views(corpus, &cluster.members, pair_scores),
        };
        let size = units.len();
        Self {
            size,
            min_similarity: cluster.min_similarity,
            max_similarity: cluster.max_similarity,
            survives_at_threshold: None,
            units,
            pairs,
            snippet: None,
            members,
        }
    }

    /// Corpus index of the occurrence the report quotes. Must agree
    /// with [`Self::quoted_unit`] — the markdown prints that unit's
    /// location above the snippet, so reading the text from a different
    /// member would caption the wrong file and line.
    pub(super) fn representative(&self) -> Option<usize> {
        let quoted = self.quoted_unit()?;
        self.members
            .iter()
            .zip(&self.units)
            .find(|(_, unit)| *unit == quoted)
            .map(|(index, _)| *index)
    }

    pub(super) fn set_snippet(&mut self, snippet: Vec<String>) {
        self.snippet = Some(snippet);
    }

    /// Longest span any member covers, used to rank a cluster against
    /// one whose windows are shorter.
    fn max_line_count(&self) -> usize {
        self.units
            .iter()
            .map(UnitRef::line_count)
            .max()
            .unwrap_or(0)
    }

    /// The occurrence the markdown report quotes: earliest by file then
    /// line, so the snippet and the location line always agree and stay
    /// stable across runs.
    fn quoted_unit(&self) -> Option<&UnitRef<'_>> {
        self.units.iter().min_by_key(|u| (u.file, u.start_line))
    }
}

/// Rank block clusters by how much duplication they represent, then drop
/// the ones that are only a sub-window of a cluster already reported.
///
/// Sliding windows mint a unit per statement run, so one repeated
/// five-statement fragment surfaces as up to five overlapping clusters —
/// the whole run, plus every shorter run inside it. Reporting all of
/// them would bury the finding in its own echoes. Ordering by occurrence
/// count first and span second means the most-repeated, longest form is
/// kept, and a shorter window is dropped only when *every* one of its
/// occurrences already sits inside a kept one: a fragment that also
/// repeats somewhere the longer run does not still earns its own entry.
pub(super) fn prune_block_clusters(clusters: &mut Vec<ClusterView<'_>>) {
    clusters.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then(b.max_line_count().cmp(&a.max_line_count()))
            .then(
                b.min_similarity
                    .partial_cmp(&a.min_similarity)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    let mut kept: Vec<Vec<UnitRef<'_>>> = Vec::new();
    clusters.retain(|cluster| {
        let covered = cluster
            .units
            .iter()
            .all(|unit| kept.iter().flatten().any(|k| k.contains(unit)));
        if !covered {
            kept.push(cluster.units.clone());
        }
        !covered
    });
}

/// Tag each cluster with the highest sweep rung at which its complete-link
/// structure survives intact, then re-rank so the tightest (most clone-like)
/// survival bands sort first. A complete-link cluster keeps every internal
/// pair `>= min_similarity`, so it stays whole at any rung `<= min_similarity`
/// and breaks above it; the survival rung is therefore the largest ladder
/// value not exceeding `min_similarity`. The ladder floor is `<=` every
/// reported cluster's `min_similarity` (clustering ran at that floor), so
/// `survives_at_threshold` is always populated in sweep mode.
pub(super) fn annotate_sweep_survival(clusters: &mut [ClusterView<'_>], ladder: &[f64]) {
    for cluster in clusters.iter_mut() {
        cluster.survives_at_threshold = highest_surviving_rung(cluster.min_similarity, ladder);
    }
    clusters.sort_by(|a, b| {
        b.survives_at_threshold
            .partial_cmp(&a.survives_at_threshold)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                b.max_similarity
                    .partial_cmp(&a.max_similarity)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(b.size.cmp(&a.size))
    });
}

/// Highest `ladder` rung not exceeding `min_similarity` (the tightest pair a
/// complete-link cluster holds). A small epsilon absorbs float drift so a
/// cluster whose `min_similarity` lands exactly on a rung still counts as
/// surviving it. `ladder` is ascending; returns `None` only when every rung
/// is above `min_similarity`, which the sweep floor rules out in practice.
fn highest_surviving_rung(min_similarity: f64, ladder: &[f64]) -> Option<f64> {
    ladder
        .iter()
        .rev()
        .find(|&&rung| min_similarity + 1e-9 >= rung)
        .copied()
}

fn cluster_pair_views<'a>(
    corpus: &'a [OwnedUnit],
    members: &[usize],
    pair_scores: &HashMap<(usize, usize), SimilarityComponents>,
) -> Vec<PairView<'a>> {
    let mut pairs = Vec::new();
    for (pos, &i) in members.iter().enumerate() {
        for &j in &members[pos + 1..] {
            let Some(components) = pair_scores.get(&sorted_pair_key(i, j)).copied() else {
                continue;
            };
            let Some(a) = corpus.get(i).map(UnitRef::from) else {
                continue;
            };
            let Some(b) = corpus.get(j).map(UnitRef::from) else {
                continue;
            };
            pairs.push(PairView::new(a, b, components));
        }
    }
    pairs.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    pairs
}

fn sorted_pair_key(i: usize, j: usize) -> (usize, usize) {
    if i <= j { (i, j) } else { (j, i) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(super) struct UnitRef<'a> {
    file: &'a str,
    name: &'a str,
    /// Language-facing kind for a type unit (`struct`, `interface`,
    /// `dataclass`, …); absent for functions.
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    start_line: usize,
    end_line: usize,
    is_test: bool,
}

impl UnitRef<'_> {
    fn line_count(&self) -> usize {
        self.end_line.saturating_sub(self.start_line) + 1
    }

    /// True when `other` occupies the same file and its line span sits
    /// inside this one's.
    fn contains(&self, other: &UnitRef<'_>) -> bool {
        self.file == other.file
            && self.start_line <= other.start_line
            && other.end_line <= self.end_line
    }
}

impl<'a> From<&'a OwnedUnit> for UnitRef<'a> {
    fn from(f: &'a OwnedUnit) -> Self {
        Self {
            file: f.rel_path.as_str(),
            name: f.name(),
            kind: f.kind,
            start_line: f.start_line(),
            end_line: f.end_line(),
            is_test: f.is_test,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct PairView<'a> {
    a: UnitRef<'a>,
    b: UnitRef<'a>,
    similarity: f64,
    body_similarity: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature_similarity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    type_overlap: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identifier_overlap: Option<f64>,
    /// Doc-comment word overlap; diagnostic only, absent unless both
    /// functions carry doc text. High values flag "same stated intent"
    /// pairs; low values on high-similarity pairs flag structural
    /// coincidences that likely should not be merged.
    #[serde(skip_serializing_if = "Option::is_none")]
    doc_overlap: Option<f64>,
}

impl<'a> PairView<'a> {
    pub(super) fn new(a: UnitRef<'a>, b: UnitRef<'a>, components: SimilarityComponents) -> Self {
        Self {
            a,
            b,
            similarity: components.similarity,
            body_similarity: components.body_similarity,
            signature_similarity: components.signature_similarity,
            type_overlap: components.type_overlap,
            identifier_overlap: components.identifier_overlap,
            doc_overlap: components.doc_overlap,
        }
    }
}

/// Cluster-level rollup of the per-pair doc overlap, for the markdown
/// report. JSON always carries the raw per-pair values; markdown gets a
/// range plus how many of the cluster's pairs actually had doc text on
/// both sides, since a range drawn from one documented pair out of six
/// means something quite different from one drawn from all six.
fn doc_overlap_summary(cluster: &ClusterView<'_>) -> String {
    let scored: Vec<f64> = cluster.pairs.iter().filter_map(|p| p.doc_overlap).collect();
    let total = cluster.pairs.len();
    let Some((min, max)) = min_max(&scored) else {
        return format!("doc overlap n/a (0/{total} pairs documented)");
    };
    format!(
        "doc overlap {:.0}–{:.0}% ({}/{total} pairs documented)",
        min * 100.0,
        max * 100.0,
        scored.len(),
    )
}

/// Cluster-level rollup of the per-pair identifier overlap, or `None`
/// when no pair carries one (both sides need a signature shape).
///
/// Reported unconditionally, unlike doc overlap: it is already computed
/// for scoring, so it costs nothing, and it is what tells the reader
/// which refactor a cluster calls for. Identifiers matching as well as
/// bodies means a verbatim clone, deletable today; identifiers diverging
/// under an identical body means same-shape / different-entity
/// boilerplate, where the answer is a generic helper or nothing.
fn identifier_overlap_summary(cluster: &ClusterView<'_>) -> Option<String> {
    let scored: Vec<f64> = cluster
        .pairs
        .iter()
        .filter_map(|p| p.identifier_overlap)
        .collect();
    let (min, max) = min_max(&scored)?;
    Some(format!(
        "identifier overlap {:.0}–{:.0}%",
        min * 100.0,
        max * 100.0,
    ))
}

/// Span of a slice of scores, or `None` when there are none to span.
fn min_max(values: &[f64]) -> Option<(f64, f64)> {
    let first = *values.first()?;
    Some(
        values
            .iter()
            .fold((first, first), |(lo, hi), &v| (lo.min(v), hi.max(v))),
    )
}

pub(super) fn format_markdown(
    report: &Report<'_>,
    top: Option<usize>,
    doc_overlap: bool,
    target: SimilarityTarget,
) -> String {
    let noun = target.noun();
    let cut = match &report.sweep {
        Some(ladder) => format!("sweep {}", format_ladder(ladder)),
        None => format!("threshold {:.2}", report.threshold),
    };
    let mut out = format!(
        "# Similarity report: {} ({} method, {} {noun}(s), {}, min lines {})\n",
        report.root, report.method, report.unit_count, cut, report.min_lines,
    );
    if report.clusters.is_empty() {
        let _ = writeln!(out, "\n_No similar {noun} clusters at or above threshold._");
        return out;
    }
    let clusters = top.map_or(report.clusters, |limit| {
        &report.clusters[..report.clusters.len().min(limit)]
    });
    if let Some(limit) = top {
        let _ = writeln!(
            out,
            "\n## Top {} similar cluster(s) of {} total",
            limit.min(report.cluster_count),
            report.cluster_count
        );
    } else {
        let _ = writeln!(out, "\n## {} similar cluster(s)", report.cluster_count);
    }
    for cluster in clusters {
        // writeln! into a String cannot fail; the result is swallowed
        // deliberately rather than unwrapped to satisfy the workspace's
        // `unwrap_used` lint.
        let _ = writeln!(out, "\n- {}", cluster_headline(cluster, doc_overlap, noun));
        if target == SimilarityTarget::Blocks {
            write_block_body(&mut out, cluster);
            continue;
        }
        for f in &cluster.units {
            let _ = writeln!(
                out,
                "  - {}:`{}` (L{}-{})",
                f.file, f.name, f.start_line, f.end_line,
            );
        }
    }
    out
}

/// Body of a block cluster: where it lives, one quoted occurrence, and
/// the per-file occurrence counts.
///
/// Deliberately *not* the flat member list the other targets print. A
/// block cluster's whole point is that it has many occurrences — the
/// 55-site case from the issue would otherwise emit 55 near-identical
/// lines and swamp the rest of the report — and a window has no name to
/// look up, so the quoted text is what makes the finding actionable.
fn write_block_body(out: &mut String, cluster: &ClusterView<'_>) {
    if let Some(rep) = cluster.quoted_unit() {
        let _ = writeln!(
            out,
            "  - at {}:`{}` (L{}-{})",
            rep.file, rep.name, rep.start_line, rep.end_line,
        );
    }
    if let Some(snippet) = &cluster.snippet {
        let _ = writeln!(out, "    ```");
        for line in snippet.iter().take(MAX_SNIPPET_LINES) {
            let _ = writeln!(out, "    {line}");
        }
        if snippet.len() > MAX_SNIPPET_LINES {
            let _ = writeln!(
                out,
                "    … ({} more lines)",
                snippet.len() - MAX_SNIPPET_LINES
            );
        }
        let _ = writeln!(out, "    ```");
    }
    let _ = writeln!(out, "  - occurrences: {}", file_breakdown(cluster));
}

/// `path ×6, other ×5 (+3 more files)` — where the occurrences actually
/// concentrate, which is what decides whether the fix is one helper or a
/// dozen local edits.
fn file_breakdown(cluster: &ClusterView<'_>) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for unit in &cluster.units {
        match counts.iter_mut().find(|(file, _)| *file == unit.file) {
            Some((_, count)) => *count += 1,
            None => counts.push((unit.file, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let shown: Vec<String> = counts
        .iter()
        .take(MAX_BREAKDOWN_FILES)
        .map(|(file, count)| format!("{file} ×{count}"))
        .collect();
    let mut line = shown.join(", ");
    if counts.len() > MAX_BREAKDOWN_FILES {
        let _ = write!(
            line,
            " (+{} more file(s))",
            counts.len() - MAX_BREAKDOWN_FILES
        );
    }
    line
}

/// Distinct enclosing functions and files a block cluster spans, for the
/// headline. "40 occurrences in 40 functions" and "40 occurrences in 2
/// functions" call for completely different fixes.
fn block_spread(cluster: &ClusterView<'_>) -> (usize, usize) {
    let mut functions: Vec<(&str, &str)> = cluster
        .units
        .iter()
        .map(|unit| (unit.file, unit.name))
        .collect();
    functions.sort_unstable();
    functions.dedup();
    let mut files: Vec<&str> = cluster.units.iter().map(|unit| unit.file).collect();
    files.sort_unstable();
    files.dedup();
    (functions.len(), files.len())
}

/// The one-line summary heading a cluster's member list: size, the
/// similarity band, the identifier-overlap band, then the optional
/// annotations — doc overlap when asked for, and the sweep survival
/// rung when sweeping.
fn cluster_headline(cluster: &ClusterView<'_>, doc_overlap: bool, noun: &str) -> String {
    let identifier_tag = identifier_overlap_summary(cluster)
        .map(|summary| format!(", {summary}"))
        .unwrap_or_default();
    let doc_tag = if doc_overlap {
        format!(", {}", doc_overlap_summary(cluster))
    } else {
        String::new()
    };
    let survival_tag = cluster
        .survives_at_threshold
        .map(|rung| format!(" [survives ≥{rung:.2}]"))
        .unwrap_or_default();
    let spread_tag = if noun == "block" {
        let (function_count, file_count) = block_spread(cluster);
        let lines = cluster.quoted_unit().map_or(0, UnitRef::line_count);
        format!(", {lines} line(s), in {function_count} function(s) across {file_count} file(s)")
    } else {
        String::new()
    };
    format!(
        "{} {noun}s, similarity {:.0}–{:.0}%{spread_tag}{identifier_tag}{doc_tag}{survival_tag}",
        cluster.size,
        cluster.min_similarity * 100.0,
        cluster.max_similarity * 100.0,
    )
}

/// Render a sweep ladder as `[0.60, 0.75, 0.85]` for the markdown header.
fn format_ladder(ladder: &[f64]) -> String {
    let rungs: Vec<String> = ladder.iter().map(|r| format!("{r:.2}")).collect();
    format!("[{}]", rungs.join(", "))
}

/// Report shape for `--paired-by`: name keys and the functions that
/// share them, instead of threshold clusters. Pairing happens before
/// scoring, so every match is reported whether or not it clears the
/// threshold — the point of the mode is the pairs clustering cannot
/// reach.
#[derive(Debug, Serialize)]
pub(super) struct PairedReport<'a> {
    root: String,
    method: &'static str,
    /// Comparison unit: `functions` or `types`.
    target: &'static str,
    /// Key that decided which units are siblings: `qualified` or
    /// `method`.
    paired_by: &'static str,
    /// Score below which a matched pair is called drifted. Unlike the
    /// clustering threshold it filters nothing — it only labels.
    threshold: f64,
    /// Score below which a matched pair was dropped as a namesake rather
    /// than reported as drift.
    drift_floor: f64,
    min_lines: usize,
    unit_count: usize,
    /// Distinct name keys that produced at least one reported pair.
    key_count: usize,
    pair_count: usize,
    drifted_pair_count: usize,
    /// Same-key functions skipped because both sides sit in one file.
    /// Present so an empty report distinguishes "no siblings anywhere"
    /// from "siblings, but all in-file".
    same_file_pair_count: usize,
    /// Name matches dropped by `drift_floor`. Reported rather than
    /// silently swallowed: a large count means the key is matching a lot
    /// of unrelated namesakes, which is worth knowing before trusting the
    /// groups that did survive.
    below_floor_count: usize,
    groups: Vec<DriftGroupView<'a>>,
}

/// Everything [`PairedReport::new`] needs beyond the groups themselves.
/// Grouped into one struct because the run configuration is threaded
/// through verbatim and a nine-argument constructor is easy to
/// mis-order at the call site.
pub(super) struct PairedReportInputs<'p> {
    pub path: &'p Path,
    pub method: &'static str,
    pub target: &'static str,
    pub paired_by: &'static str,
    pub threshold: f64,
    pub drift_floor: f64,
    pub min_lines: usize,
    pub unit_count: usize,
    pub same_file_pair_count: usize,
    pub below_floor_count: usize,
}

impl<'a> PairedReport<'a> {
    pub(super) fn new(inputs: PairedReportInputs<'_>, mut groups: Vec<DriftGroupView<'a>>) -> Self {
        sort_by_drift(&mut groups);
        Self {
            root: inputs.path.display().to_string(),
            method: inputs.method,
            target: inputs.target,
            paired_by: inputs.paired_by,
            threshold: inputs.threshold,
            drift_floor: inputs.drift_floor,
            min_lines: inputs.min_lines,
            unit_count: inputs.unit_count,
            key_count: groups.len(),
            pair_count: groups.iter().map(|g| g.pairs.len()).sum(),
            drifted_pair_count: groups.iter().map(|g| g.drifted_pair_count).sum(),
            same_file_pair_count: inputs.same_file_pair_count,
            below_floor_count: inputs.below_floor_count,
            groups,
        }
    }
}

/// Every function sharing one name key, with the pairwise scores between
/// them. Deliberately shaped like [`ClusterView`] — same member list,
/// same similarity band, same per-pair components — so a consumer that
/// already reads clusters needs no second parser. The difference is how
/// membership was decided: a cluster is functions that scored alike, a
/// group is functions that were *named* alike.
#[derive(Debug, Serialize)]
pub(super) struct DriftGroupView<'a> {
    key: &'a str,
    size: usize,
    min_similarity: f64,
    max_similarity: f64,
    /// How many of this group's pairs fell below the threshold: same
    /// name, no longer the same implementation. Either a deliberate
    /// divergence or a missed sync — the report labels rather than
    /// judges, because it cannot tell which.
    drifted_pair_count: usize,
    units: Vec<UnitRef<'a>>,
    pairs: Vec<PairView<'a>>,
}

/// One name-matched pair after scoring, before it is folded into its
/// group. `key` indexes the key list the pairing pass produced.
#[derive(Debug, Clone, Copy)]
pub(super) struct ScoredMatch {
    pub key: usize,
    pub i: usize,
    pub j: usize,
    pub components: SimilarityComponents,
}

/// Fold scored name matches into one [`DriftGroupView`] per key.
///
/// Members are the distinct functions that appear in the key's surviving
/// pairs, in corpus order: a pair dropped by the drift floor takes its
/// endpoints with it unless another pair keeps them, so the member list
/// always matches the pairs actually shown.
pub(super) fn build_drift_groups<'a>(
    corpus: &'a [OwnedUnit],
    keys: &'a [String],
    matches: &[ScoredMatch],
    threshold: f64,
) -> Vec<DriftGroupView<'a>> {
    let mut by_key: HashMap<usize, Vec<ScoredMatch>> = HashMap::new();
    for scored in matches {
        by_key.entry(scored.key).or_default().push(*scored);
    }
    let mut groups: Vec<DriftGroupView<'a>> = by_key
        .into_iter()
        .filter_map(|(key_index, mut scored)| {
            scored.sort_by_key(|m| (m.i, m.j));
            let mut members: Vec<usize> = scored.iter().flat_map(|m| [m.i, m.j]).collect();
            members.sort_unstable();
            members.dedup();
            let units: Vec<UnitRef<'a>> = members
                .iter()
                .filter_map(|i| corpus.get(*i).map(UnitRef::from))
                .collect();
            let pairs: Vec<PairView<'a>> = scored
                .iter()
                .filter_map(|m| {
                    Some(PairView::new(
                        corpus.get(m.i).map(UnitRef::from)?,
                        corpus.get(m.j).map(UnitRef::from)?,
                        m.components,
                    ))
                })
                .collect();
            let scores: Vec<f64> = pairs.iter().map(|p| p.similarity).collect();
            let (min_similarity, max_similarity) = min_max(&scores)?;
            Some(DriftGroupView {
                key: keys.get(key_index)?.as_str(),
                size: units.len(),
                min_similarity,
                max_similarity,
                drifted_pair_count: scores.iter().filter(|s| **s < threshold).count(),
                units,
                pairs,
            })
        })
        .collect();
    for group in &mut groups {
        group.pairs.sort_by(|a, b| {
            a.similarity
                .partial_cmp(&b.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    groups
}

/// Ascending by the group's worst pair — the whole point of the mode is
/// that the most-drifted siblings come first. Ties break on the band's
/// upper end and then the key, so the order is total and reproducible.
fn sort_by_drift(groups: &mut [DriftGroupView<'_>]) {
    groups.sort_by(|a, b| {
        a.min_similarity
            .partial_cmp(&b.min_similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.max_similarity
                    .partial_cmp(&b.max_similarity)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then_with(|| a.key.cmp(b.key))
    });
}

pub(super) fn format_paired_markdown(
    report: &PairedReport<'_>,
    top: Option<usize>,
    noun: &str,
) -> String {
    let mut out = format!(
        "# Similarity report: {} (paired by {}, {} method, {} {noun}(s), drift threshold {:.2}, floor {:.2}, min lines {})\n",
        report.root,
        report.paired_by,
        report.method,
        report.unit_count,
        report.threshold,
        report.drift_floor,
        report.min_lines,
    );
    if report.groups.is_empty() {
        let _ = writeln!(
            out,
            "\n_No name-matched {noun}s above the floor ({} match(es) below floor, {} same-file namesake pair(s) skipped)._",
            report.below_floor_count, report.same_file_pair_count,
        );
        return out;
    }
    let groups = top.map_or(report.groups.as_slice(), |limit| {
        &report.groups[..report.groups.len().min(limit)]
    });
    let heading = match top {
        Some(limit) => format!(
            "## Most drifted {} of {} name key(s)",
            limit.min(report.key_count),
            report.key_count,
        ),
        None => format!(
            "## {} name key(s) with cross-file siblings",
            report.key_count
        ),
    };
    let _ = writeln!(out, "\n{heading}");
    let _ = writeln!(
        out,
        "\n{} of {} pair(s) scored below {:.2}: matched by name, no longer matching in body. Keys are ordered by their worst pair, so the widest drift comes first. {} further match(es) fell below the {:.2} floor and are treated as unrelated namesakes rather than drift.",
        report.drifted_pair_count,
        report.pair_count,
        report.threshold,
        report.below_floor_count,
        report.drift_floor,
    );
    for group in groups {
        // writeln! into a String cannot fail; the result is swallowed
        // deliberately rather than unwrapped to satisfy the workspace's
        // `unwrap_used` lint.
        let _ = writeln!(out, "\n- {}", drift_headline(group, noun));
        for f in &group.units {
            let _ = writeln!(
                out,
                "  - {}:`{}` (L{}-{})",
                f.file, f.name, f.start_line, f.end_line,
            );
        }
    }
    out
}

/// The one-line summary heading a key's member list: the key, how many
/// units share it, their similarity band, and how many of the pairs
/// between them count as drifted.
fn drift_headline(group: &DriftGroupView<'_>, noun: &str) -> String {
    format!(
        "`{}` — {} {noun}s, similarity {:.0}–{:.0}%, {}/{} pair(s) drifted",
        group.key,
        group.size,
        group.min_similarity * 100.0,
        group.max_similarity * 100.0,
        group.drifted_pair_count,
        group.pairs.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::path::PathBuf;

    fn owned_function(name: &str) -> OwnedUnit {
        OwnedUnit {
            file: PathBuf::from("lib.rs"),
            rel_path: "lib.rs".to_owned(),
            is_test: false,
            kind: None,
            shape: lens_domain::FunctionShape::from(lens_domain::FunctionDef {
                name: name.to_owned(),
                start_line: 1,
                end_line: 5,
                is_test: false,
                signature: None,
                doc: None,
                tree: lens_domain::TreeNode::leaf("Block"),
            }),
        }
    }

    fn components(similarity: f64) -> SimilarityComponents {
        SimilarityComponents {
            similarity,
            body_similarity: similarity,
            signature_similarity: Some(similarity),
            type_overlap: Some(similarity),
            identifier_overlap: Some(similarity),
            doc_overlap: None,
        }
    }

    /// Bare cluster carrying only the stats the sweep ranking reads. The
    /// `units`/`pairs` vectors stay empty so the value is `'static` and
    /// the sort/annotation logic can be exercised in isolation.
    fn cluster(min_similarity: f64, max_similarity: f64, size: usize) -> ClusterView<'static> {
        ClusterView {
            size,
            min_similarity,
            max_similarity,
            survives_at_threshold: None,
            units: Vec::new(),
            pairs: Vec::new(),
            snippet: None,
            members: Vec::new(),
        }
    }

    #[test]
    fn highest_surviving_rung_picks_largest_rung_not_above_min_similarity() {
        let ladder = [0.6, 0.75, 0.85];
        // Above the top rung → survives the top.
        assert_eq!(highest_surviving_rung(0.97, &ladder), Some(0.85));
        // Between rungs → survives the rung just below.
        assert_eq!(highest_surviving_rung(0.79, &ladder), Some(0.75));
        // Exactly on a rung → epsilon keeps it surviving that rung.
        assert_eq!(highest_surviving_rung(0.85, &ladder), Some(0.85));
        // At the floor → survives the floor.
        assert_eq!(highest_surviving_rung(0.60, &ladder), Some(0.6));
        // Below every rung → no rung survives.
        assert_eq!(highest_surviving_rung(0.50, &ladder), None);
    }

    #[test]
    fn annotate_sweep_survival_tags_each_cluster_from_its_min_similarity() {
        let mut clusters = vec![cluster(0.97, 0.97, 2), cluster(0.79, 0.79, 2)];
        annotate_sweep_survival(&mut clusters, &[0.6, 0.75, 0.85]);
        let tags: Vec<Option<f64>> = clusters.iter().map(|c| c.survives_at_threshold).collect();
        assert_eq!(tags, vec![Some(0.85), Some(0.75)]);
    }

    #[test]
    fn annotate_sweep_survival_ranks_by_survival_rung_before_max_similarity() {
        // The looser cluster has the higher `max_similarity`; survival rung
        // must still win the sort, so it lands second despite the bigger max.
        let mut clusters = vec![
            cluster(0.70, 0.99, 2), // survives 0.6
            cluster(0.95, 0.95, 2), // survives 0.85
        ];
        annotate_sweep_survival(&mut clusters, &[0.6, 0.85]);
        assert_eq!(
            clusters
                .iter()
                .map(|c| c.survives_at_threshold)
                .collect::<Vec<_>>(),
            vec![Some(0.85), Some(0.6)],
        );
    }

    #[test]
    fn annotate_sweep_survival_breaks_survival_ties_by_max_then_size() {
        // All three survive the same rung (0.8). Input order is deliberately
        // scrambled so a passing assertion proves the max- then size-keyed
        // tiebreak actually ran.
        let mut clusters = vec![
            cluster(0.82, 0.86, 2), // tie on rung, lowest max
            cluster(0.81, 0.90, 2), // tie on rung, highest max
            cluster(0.83, 0.86, 3), // ties on rung+max with the first, larger
        ];
        annotate_sweep_survival(&mut clusters, &[0.6, 0.8]);
        let order: Vec<(f64, usize)> = clusters
            .iter()
            .map(|c| (c.max_similarity, c.size))
            .collect();
        assert_eq!(order, vec![(0.90, 2), (0.86, 3), (0.86, 2)]);
    }

    /// Cluster of block windows at fixed spans, for the pruning and
    /// block-markdown checks.
    fn block_cluster(units: &[(&'static str, &'static str, usize, usize)]) -> ClusterView<'static> {
        ClusterView {
            size: units.len(),
            min_similarity: 0.9,
            max_similarity: 1.0,
            survives_at_threshold: None,
            units: units
                .iter()
                .map(|&(file, name, start_line, end_line)| UnitRef {
                    file,
                    name,
                    kind: None,
                    start_line,
                    end_line,
                    is_test: false,
                })
                .collect(),
            pairs: Vec::new(),
            snippet: None,
            members: (0..units.len()).collect(),
        }
    }

    /// The snippet is read from the member `representative` names while
    /// the caption comes from `quoted_unit`; if they can disagree, the
    /// report quotes one occurrence under another's file and line. The
    /// corpus index deliberately does not follow source order here.
    #[test]
    fn representative_is_the_corpus_index_of_the_quoted_unit() {
        let mut cluster = block_cluster(&[
            ("b.rs", "second", 30, 32),
            ("a.rs", "first", 20, 22),
            ("a.rs", "first", 10, 12),
        ]);
        cluster.members = vec![7, 4, 9];

        assert_eq!(
            cluster.quoted_unit().map(|u| (u.file, u.start_line)),
            Some(("a.rs", 10)),
        );
        assert_eq!(cluster.representative(), Some(9));
    }

    fn cluster_spans(clusters: &[ClusterView<'_>]) -> Vec<Vec<(usize, usize)>> {
        clusters
            .iter()
            .map(|c| c.units.iter().map(|u| (u.start_line, u.end_line)).collect())
            .collect()
    }

    #[test]
    fn prune_block_clusters_drops_sub_windows_of_a_kept_cluster() {
        // The 5-line cluster and the 3-line one inside it describe the
        // same duplication; only the longer form should survive.
        let mut clusters = vec![
            block_cluster(&[("a.rs", "f", 11, 13), ("b.rs", "g", 31, 33)]),
            block_cluster(&[("a.rs", "f", 10, 14), ("b.rs", "g", 30, 34)]),
        ];

        prune_block_clusters(&mut clusters);

        assert_eq!(cluster_spans(&clusters), vec![vec![(10, 14), (30, 34)]]);
    }

    /// A shorter fragment that also repeats where the longer run does
    /// not is a finding of its own, not an echo of the longer one.
    #[test]
    fn prune_block_clusters_keeps_a_sub_window_with_an_uncovered_occurrence() {
        let mut clusters = vec![
            block_cluster(&[("a.rs", "f", 10, 14), ("b.rs", "g", 30, 34)]),
            block_cluster(&[
                ("a.rs", "f", 11, 13),
                ("b.rs", "g", 31, 33),
                ("c.rs", "h", 50, 52),
            ]),
        ];

        prune_block_clusters(&mut clusters);

        // Ranked by occurrence count first, so the 3-member cluster
        // leads and nothing it covers is dropped.
        assert_eq!(
            cluster_spans(&clusters),
            vec![vec![(11, 13), (31, 33), (50, 52)], vec![(10, 14), (30, 34)],],
        );
    }

    /// Containment is per-file: the same line range in a different file
    /// is a different piece of code.
    #[test]
    fn prune_block_clusters_does_not_cover_across_files() {
        let mut clusters = vec![
            block_cluster(&[("a.rs", "f", 10, 14), ("b.rs", "g", 30, 34)]),
            block_cluster(&[("c.rs", "h", 11, 13), ("d.rs", "i", 31, 33)]),
        ];

        prune_block_clusters(&mut clusters);

        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn block_markdown_quotes_one_occurrence_and_rolls_the_rest_up_by_file() {
        let mut cluster = block_cluster(&[
            ("b.rs", "second", 30, 32),
            ("a.rs", "first", 10, 12),
            ("a.rs", "first", 20, 22),
        ]);
        cluster.set_snippet(vec!["let a = 1;".to_owned(), "let b = 2;".to_owned()]);
        let clusters = [cluster];
        let report = Report::new(
            Path::new("src"),
            "tsed",
            "blocks",
            0.85,
            3,
            9,
            None,
            &clusters,
        );

        let md = format_markdown(&report, None, false, SimilarityTarget::Blocks);

        assert!(md.contains("3 blocks, similarity 90–100%"), "got: {md}");
        assert!(
            md.contains("3 line(s), in 2 function(s) across 2 file(s)"),
            "got: {md}",
        );
        // Earliest by file then line is the quoted occurrence.
        assert!(md.contains("at a.rs:`first` (L10-12)"), "got: {md}");
        assert!(md.contains("    let a = 1;"), "got: {md}");
        assert!(md.contains("occurrences: a.rs ×2, b.rs ×1"), "got: {md}",);
        // The flat member list of the other targets must not appear.
        assert!(!md.contains("  - b.rs:`second`"), "got: {md}");
    }

    /// A quoted occurrence is capped so one long window cannot swallow
    /// the report, and the cut has to be announced — a silently
    /// truncated snippet reads as the whole fragment.
    #[rstest]
    #[case::under_cap(MAX_SNIPPET_LINES - 1, MAX_SNIPPET_LINES - 1, None)]
    #[case::exactly_at_cap(MAX_SNIPPET_LINES, MAX_SNIPPET_LINES, None)]
    #[case::over_cap(MAX_SNIPPET_LINES + 3, MAX_SNIPPET_LINES, Some("… (3 more lines)"))]
    fn block_snippet_is_capped_and_says_when_it_truncated(
        #[case] snippet_len: usize,
        #[case] expected_shown: usize,
        #[case] expected_marker: Option<&str>,
    ) {
        let mut cluster = block_cluster(&[("a.rs", "f", 1, 3), ("b.rs", "g", 1, 3)]);
        cluster.set_snippet((0..snippet_len).map(|i| format!("line{i};")).collect());
        let mut out = String::new();

        write_block_body(&mut out, &cluster);

        let shown = (0..snippet_len)
            .filter(|i| out.contains(&format!("    line{i};\n")))
            .count();
        assert_eq!(shown, expected_shown, "got: {out}");
        match expected_marker {
            Some(marker) => assert!(out.contains(marker), "got: {out}"),
            None => assert!(!out.contains("more lines)"), "got: {out}"),
        }
    }

    /// The occurrence breakdown names files up to a cap and then says
    /// how many it left out. A cap that fires one file early would
    /// report "+0 more file(s)" on a cluster that fits exactly.
    #[rstest]
    #[case::under_cap(MAX_BREAKDOWN_FILES - 1, None)]
    #[case::exactly_at_cap(MAX_BREAKDOWN_FILES, None)]
    #[case::over_cap(MAX_BREAKDOWN_FILES + 2, Some("(+2 more file(s))"))]
    fn block_file_breakdown_collapses_the_tail_past_the_display_cap(
        #[case] file_count: usize,
        #[case] expected_marker: Option<&str>,
    ) {
        const FILES: [&str; 9] = [
            "a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs", "g.rs", "h.rs", "i.rs",
        ];
        let units: Vec<(&'static str, &'static str, usize, usize)> = FILES[..file_count]
            .iter()
            .map(|file| (*file, "fn", 1usize, 3usize))
            .collect();

        let line = file_breakdown(&block_cluster(&units));

        assert!(line.starts_with("a.rs ×1, b.rs ×1"), "got: {line}");
        assert_eq!(
            line.split(", ").count(),
            file_count.min(MAX_BREAKDOWN_FILES),
            "got: {line}",
        );
        match expected_marker {
            Some(marker) => assert!(line.ends_with(marker), "got: {line}"),
            None => assert!(!line.contains("more file(s)"), "got: {line}"),
        }
    }

    #[test]
    fn cluster_pair_views_uses_all_unique_member_pairs_and_their_scores() {
        let corpus = vec![
            owned_function("alpha"),
            owned_function("beta"),
            owned_function("gamma"),
        ];
        let pair_scores = HashMap::from([
            ((0, 0), components(1.0)),
            ((0, 1), components(0.91)),
            ((0, 2), components(0.92)),
            ((1, 1), components(1.0)),
            ((1, 2), components(0.93)),
            ((2, 2), components(1.0)),
        ]);

        let pairs = cluster_pair_views(&corpus, &[0, 1, 2], &pair_scores);

        assert_eq!(pairs.len(), 3);
        assert!(pairs.iter().all(|pair| pair.a.name != pair.b.name));
        assert_eq!(
            pairs
                .iter()
                .map(|pair| (pair.a.name, pair.b.name, pair.similarity))
                .collect::<Vec<_>>(),
            vec![
                ("beta", "gamma", 0.93),
                ("alpha", "gamma", 0.92),
                ("alpha", "beta", 0.91),
            ],
        );
        assert_eq!(sorted_pair_key(2, 0), (0, 2));
    }

    /// Cluster carrying only the per-pair identifier scores the rollup
    /// reads, with a fixed similarity band so the headline is stable.
    fn cluster_with_identifier_scores(scores: &[Option<f64>]) -> ClusterView<'static> {
        let f = UnitRef {
            file: "lib.rs",
            name: "alpha",
            kind: None,
            start_line: 1,
            end_line: 5,
            is_test: false,
        };
        ClusterView {
            size: 2,
            min_similarity: 0.9,
            max_similarity: 0.9,
            survives_at_threshold: None,
            units: Vec::new(),
            pairs: scores
                .iter()
                .map(|&identifier_overlap| PairView {
                    a: f,
                    b: f,
                    similarity: 0.9,
                    body_similarity: 0.9,
                    signature_similarity: None,
                    type_overlap: None,
                    identifier_overlap,
                    doc_overlap: None,
                })
                .collect(),
            snippet: None,
            members: Vec::new(),
        }
    }

    #[rstest]
    #[case::verbatim_clone(&[Some(1.0)], Some("identifier overlap 100–100%"))]
    #[case::parameterised_repetition(
        &[Some(0.2), Some(0.5), Some(0.33)],
        Some("identifier overlap 20–50%")
    )]
    // Both sides need a signature shape; without one there is nothing
    // to report and the headline drops the tag rather than showing 0%.
    #[case::unscored(&[None, None], None)]
    #[case::no_pairs(&[], None)]
    fn identifier_overlap_summary_spans_the_scored_pairs(
        #[case] scores: &[Option<f64>],
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(
            identifier_overlap_summary(&cluster_with_identifier_scores(scores)).as_deref(),
            expected,
        );
    }

    /// The regression the tag exists for: two clusters that are
    /// indistinguishable by the similarity band alone, and that call
    /// for different refactors.
    #[test]
    fn headline_separates_verbatim_clones_from_parameterised_repetition() {
        let clone = cluster_with_identifier_scores(&[Some(1.0)]);
        let boilerplate = cluster_with_identifier_scores(&[Some(0.2), Some(0.5)]);

        assert_eq!(
            cluster_headline(&clone, false, "function"),
            "2 functions, similarity 90–90%, identifier overlap 100–100%",
        );
        assert_eq!(
            cluster_headline(&boilerplate, false, "function"),
            "2 functions, similarity 90–90%, identifier overlap 20–50%",
        );
    }

    /// Cluster carrying only the per-pair doc scores the rollup reads.
    fn cluster_with_doc_scores(docs: &[Option<f64>]) -> ClusterView<'static> {
        let f = UnitRef {
            file: "lib.rs",
            name: "alpha",
            kind: None,
            start_line: 1,
            end_line: 5,
            is_test: false,
        };
        ClusterView {
            size: 2,
            min_similarity: 0.9,
            max_similarity: 0.9,
            survives_at_threshold: None,
            units: Vec::new(),
            pairs: docs
                .iter()
                .map(|&doc_overlap| PairView {
                    a: f,
                    b: f,
                    similarity: 0.9,
                    body_similarity: 0.9,
                    signature_similarity: None,
                    type_overlap: None,
                    identifier_overlap: None,
                    doc_overlap,
                })
                .collect(),
            snippet: None,
            members: Vec::new(),
        }
    }

    #[rstest]
    #[case::single(&[Some(0.5)], "doc overlap 50–50% (1/1 pairs documented)")]
    #[case::spread(
        &[Some(0.2), Some(0.8), Some(0.5)],
        "doc overlap 20–80% (3/3 pairs documented)"
    )]
    // A range drawn from one pair out of three is a much weaker signal
    // than the same range drawn from all three, so the count is reported.
    #[case::partial(
        &[Some(0.4), None, None],
        "doc overlap 40–40% (1/3 pairs documented)"
    )]
    #[case::none_documented(&[None, None], "doc overlap n/a (0/2 pairs documented)")]
    #[case::no_pairs(&[], "doc overlap n/a (0/0 pairs documented)")]
    fn doc_overlap_summary_reports_range_and_documented_pair_count(
        #[case] docs: &[Option<f64>],
        #[case] expected: &str,
    ) {
        assert_eq!(
            doc_overlap_summary(&cluster_with_doc_scores(docs)),
            expected
        );
    }

    #[rstest]
    #[case::empty(&[], None)]
    #[case::single(&[0.3], Some((0.3, 0.3)))]
    #[case::unordered(&[0.5, 0.1, 0.9, 0.4], Some((0.1, 0.9)))]
    fn min_max_spans_the_values(#[case] values: &[f64], #[case] expected: Option<(f64, f64)>) {
        assert_eq!(min_max(values), expected);
    }

    fn sibling(name: &str, rel_path: &str) -> OwnedUnit {
        OwnedUnit {
            rel_path: rel_path.to_owned(),
            file: PathBuf::from(rel_path),
            ..owned_function(name)
        }
    }

    fn scored_match(key: usize, i: usize, j: usize, similarity: f64) -> ScoredMatch {
        ScoredMatch {
            key,
            i,
            j,
            components: components(similarity),
        }
    }

    /// Three siblings of one key, one pair of which is missing (it fell
    /// below the drift floor before grouping). The member list must
    /// follow the surviving pairs rather than the key's original group,
    /// so the functions shown are exactly the ones the scores describe.
    #[test]
    fn build_drift_groups_collects_members_from_surviving_pairs() {
        let corpus = vec![
            sibling("Summary::from", "napi.rs"),
            sibling("JsSummary::from", "wasm.rs"),
            sibling("PySummary::from", "py.rs"),
            sibling("Article::parse", "napi.rs"),
        ];
        let keys = vec!["summary::from".to_owned()];
        let matches = [scored_match(0, 0, 1, 0.4), scored_match(0, 0, 2, 0.9)];

        let groups = build_drift_groups(&corpus, &keys, &matches, 0.85);

        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.key, "summary::from");
        assert_eq!(group.size, 3);
        assert_eq!(group.min_similarity, 0.4);
        assert_eq!(group.max_similarity, 0.9);
        assert_eq!(group.drifted_pair_count, 1);
        assert_eq!(
            group.units.iter().map(|f| f.file).collect::<Vec<_>>(),
            vec!["napi.rs", "wasm.rs", "py.rs"],
        );
        // Pairs ascend so the worst sibling reads first.
        assert_eq!(
            group.pairs.iter().map(|p| p.similarity).collect::<Vec<_>>(),
            vec![0.4, 0.9],
        );
    }

    /// A pair sitting exactly on the threshold still matches — the
    /// clustering path keeps `similarity == threshold`, and the drift
    /// label has to agree with it or the same score reads two ways.
    #[test]
    fn build_drift_groups_excludes_pairs_exactly_on_the_threshold() {
        let corpus = vec![
            sibling("Summary::from", "a.rs"),
            sibling("JsSummary::from", "b.rs"),
            sibling("PySummary::from", "c.rs"),
        ];
        let keys = vec!["summary::from".to_owned()];
        let matches = [
            scored_match(0, 0, 1, 0.85),
            scored_match(0, 0, 2, 0.8499999),
        ];

        let groups = build_drift_groups(&corpus, &keys, &matches, 0.85);

        assert_eq!(groups[0].drifted_pair_count, 1);
    }

    #[test]
    fn build_drift_groups_splits_by_key_and_drops_unknown_keys() {
        let corpus = vec![
            sibling("Summary::from", "a.rs"),
            sibling("Summary::from", "b.rs"),
            sibling("Article::parse", "a.rs"),
            sibling("Article::parse", "b.rs"),
        ];
        let keys = vec!["article::parse".to_owned(), "summary::from".to_owned()];
        let matches = [
            scored_match(0, 2, 3, 0.5),
            scored_match(1, 0, 1, 0.7),
            // A key index past the end of `keys` cannot be rendered and
            // must be dropped rather than panic the report.
            scored_match(9, 0, 3, 0.6),
        ];

        let groups = build_drift_groups(&corpus, &keys, &matches, 0.85);

        let mut rendered: Vec<(&str, f64)> =
            groups.iter().map(|g| (g.key, g.min_similarity)).collect();
        rendered.sort_by(|a, b| a.0.cmp(b.0));
        assert_eq!(
            rendered,
            vec![("article::parse", 0.5), ("summary::from", 0.7)],
        );
    }

    fn group(key: &'static str, min: f64, max: f64) -> DriftGroupView<'static> {
        DriftGroupView {
            key,
            size: 2,
            min_similarity: min,
            max_similarity: max,
            drifted_pair_count: 1,
            units: Vec::new(),
            pairs: Vec::new(),
        }
    }

    #[test]
    fn sort_by_drift_puts_the_worst_pair_first() {
        // Input order is scrambled and the tightest key carries the
        // highest max, so a passing assertion proves the min-keyed sort
        // ran rather than the order surviving by accident.
        let mut groups = vec![
            group("beta", 0.80, 0.99),
            group("alpha", 0.35, 0.40),
            group("gamma", 0.35, 0.99),
        ];

        sort_by_drift(&mut groups);

        assert_eq!(
            groups.iter().map(|g| g.key).collect::<Vec<_>>(),
            vec!["alpha", "gamma", "beta"],
        );
    }

    #[test]
    fn drift_headline_reports_band_and_drifted_share() {
        let mut wide = group("render_modules", 0.34, 0.98);
        wide.size = 4;
        wide.drifted_pair_count = 3;
        wide.pairs = vec![PairView::new(
            UnitRef {
                file: "a.rs",
                name: "render_modules",
                kind: None,
                start_line: 1,
                end_line: 5,
                is_test: false,
            },
            UnitRef {
                file: "b.rs",
                name: "render_modules",
                kind: None,
                start_line: 1,
                end_line: 5,
                is_test: false,
            },
            components(0.34),
        )];

        assert_eq!(
            drift_headline(&wide, "function"),
            "`render_modules` — 4 functions, similarity 34–98%, 3/1 pair(s) drifted",
        );
    }

    fn paired_report(groups: Vec<DriftGroupView<'static>>) -> PairedReport<'static> {
        PairedReport::new(
            PairedReportInputs {
                path: Path::new("src"),
                method: "tsed",
                target: "functions",
                paired_by: "qualified",
                threshold: 0.85,
                drift_floor: 0.3,
                min_lines: 5,
                unit_count: 40,
                same_file_pair_count: 2,
                below_floor_count: 7,
            },
            groups,
        )
    }

    /// An empty paired report has to say *why* it is empty: "nothing
    /// shares a name" and "everything that did was a namesake or in-file"
    /// call for different next steps.
    #[test]
    fn empty_paired_markdown_reports_what_was_excluded() {
        let out = format_paired_markdown(&paired_report(Vec::new()), None, "function");
        assert!(out.contains("paired by qualified"));
        assert!(out.contains("7 match(es) below floor"));
        assert!(out.contains("2 same-file namesake pair(s) skipped"));
    }

    #[test]
    fn paired_markdown_caps_at_top_and_says_so() {
        let report = paired_report(vec![group("alpha", 0.35, 0.40), group("beta", 0.80, 0.99)]);

        let capped = format_paired_markdown(&report, Some(1), "function");
        assert!(capped.contains("## Most drifted 1 of 2 name key(s)"));
        assert!(capped.contains("`alpha`"));
        assert!(!capped.contains("`beta`"));

        let full = format_paired_markdown(&report, None, "function");
        assert!(full.contains("## 2 name key(s) with cross-file siblings"));
        assert!(full.contains("`beta`"));
    }

    #[test]
    fn paired_report_totals_roll_up_from_the_groups() {
        let mut first = group("alpha", 0.35, 0.40);
        first.pairs = vec![PairView::new(
            UnitRef {
                file: "a.rs",
                name: "alpha",
                kind: None,
                start_line: 1,
                end_line: 5,
                is_test: false,
            },
            UnitRef {
                file: "b.rs",
                name: "alpha",
                kind: None,
                start_line: 1,
                end_line: 5,
                is_test: false,
            },
            components(0.35),
        )];
        let second = group("beta", 0.80, 0.99);

        let report = paired_report(vec![first, second]);

        assert_eq!(report.key_count, 2);
        assert_eq!(report.pair_count, 1);
        assert_eq!(report.drifted_pair_count, 2);
    }
}
