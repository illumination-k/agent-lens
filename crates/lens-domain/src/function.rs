//! Language-agnostic function extraction and pairwise similarity.
//!
//! Language-specific crates implement [`LanguageParser`] to go from source
//! text to a list of [`FunctionDef`] values; this module then takes care of
//! comparing every pair with [`crate::calculate_tsed`] and returning the
//! ones that cross a user-supplied threshold.
//!
//! Pairs can be reported flat ([`find_similar_functions`]) or grouped into
//! complete-link clusters ([`cluster_similar_pairs`]). Clustering is the
//! preferred surface for agent-facing output: it keeps the context size
//! down and makes the "these N functions are all near-duplicates of each
//! other" signal explicit.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::time::Instant;

use rayon::prelude::*;

use crate::lsh::{LshOptions, LshTree, lsh_candidate_pairs};
use crate::tree::TreeNode;
use crate::tsed::{TSEDOptions, calculate_tsed};

const PROFILE_TARGET: &str = "agent_lens::similarity_profile";

/// A single function extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDef {
    pub name: String,
    /// 1-based inclusive start line.
    pub start_line: usize,
    /// 1-based inclusive end line.
    pub end_line: usize,
    /// Whether the language adapter classified this function-shaped item as
    /// test code from source-level syntax or naming conventions.
    pub is_test: bool,
    /// Lightweight syntactic signature metadata. Language adapters can leave
    /// this as `None` until they have a signature projection; body-only
    /// similarity remains available through [`Self::body_tree`].
    pub signature: Option<FunctionSignature>,
    pub tree: TreeNode,
}

impl FunctionDef {
    /// Lines spanned by the function, inclusive of both endpoints.
    ///
    /// # Examples
    ///
    /// ```
    /// use lens_domain::{FunctionDef, TreeNode};
    ///
    /// let f = FunctionDef {
    ///     name: "f".into(),
    ///     start_line: 5,
    ///     end_line: 10,
    ///     is_test: false,
    ///     signature: None,
    ///     tree: TreeNode::leaf("Block"),
    /// };
    /// assert_eq!(f.line_count(), 6);
    /// ```
    pub fn line_count(&self) -> usize {
        self.end_line.saturating_sub(self.start_line) + 1
    }

    /// The structural body tree used for body-focused similarity.
    ///
    /// Rust functions are represented as `Function(FnSignature, Block)` so
    /// this peels off the `Block` child. Other adapters already store the
    /// body as the root tree, so they fall back to `self.tree`.
    pub fn body_tree(&self) -> &TreeNode {
        self.tree
            .children
            .iter()
            .find(|child| child.label == "Block")
            .unwrap_or(&self.tree)
    }
}

/// Lightweight, syntax-only signature facts used by signature-aware scoring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionSignature {
    pub name_tokens: Vec<String>,
    pub parameter_count: usize,
    pub parameter_names: Vec<String>,
    pub parameter_type_paths: Vec<String>,
    pub return_type_paths: Vec<String>,
    pub generics: Vec<String>,
    pub receiver: ReceiverShape,
}

/// Method receiver shape for languages that expose one syntactically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReceiverShape {
    None,
    Value,
    Ref,
    RefMut,
}

impl LshTree for FunctionDef {
    fn tree(&self) -> &TreeNode {
        &self.tree
    }
}

/// Abstraction over a language's source-to-tree/function-extraction pipeline.
///
/// Implementors are expected to be cheap to reuse across files (e.g. storing
/// a tree-sitter or syn parser by value) but they don't need to be thread
/// safe — `agent-lens` drives comparison sequentially per parser instance.
pub trait LanguageParser {
    /// Short identifier for the language (e.g. `"rust"`).
    fn language(&self) -> &'static str;

    /// Parse the whole source into a single tree. Mostly useful for whole-
    /// file comparisons; most callers will reach for [`Self::extract_functions`].
    fn parse(&mut self, source: &str) -> Result<TreeNode, LanguageParseError>;

    /// Extract every function-like definition in `source`.
    fn extract_functions(&mut self, source: &str) -> Result<Vec<FunctionDef>, LanguageParseError>;
}

#[derive(Debug, thiserror::Error)]
#[error("failed to parse {language} source: {source}")]
pub struct LanguageParseError {
    language: &'static str,
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl LanguageParseError {
    pub fn new(
        language: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            language,
            source: Box::new(source),
        }
    }
}

/// A pair of functions that exceed the similarity threshold.
#[derive(Debug, Clone, Copy)]
pub struct SimilarPair<'a> {
    pub a: &'a FunctionDef,
    pub b: &'a FunctionDef,
    pub similarity: f64,
}

/// A complete-link cluster of similar items.
///
/// Every pair of `members` had a recorded similarity `>= threshold` in the
/// input — no chaining through weaker links. `min_similarity` /
/// `max_similarity` summarise the tightest and loosest pair inside the
/// cluster.
///
/// `members` are indices back into the slice that produced the input pairs.
/// Clusters always contain `>= 2` members; singletons are dropped.
#[derive(Debug, Clone)]
pub struct SimilarCluster {
    pub members: Vec<usize>,
    pub min_similarity: f64,
    pub max_similarity: f64,
}

/// Compute pairwise similarity over `functions` and return every pair whose
/// TSED score is `>= threshold`, sorted from most to least similar.
pub fn find_similar_functions<'a>(
    functions: &'a [FunctionDef],
    threshold: f64,
    opts: &TSEDOptions,
) -> Vec<SimilarPair<'a>> {
    let mut pairs: Vec<SimilarPair<'a>> = find_similar_pair_indices(functions, threshold, opts)
        .into_iter()
        .map(|(i, j, similarity)| SimilarPair {
            a: &functions[i],
            b: &functions[j],
            similarity,
        })
        .collect();
    pairs.sort_by(|x, y| {
        y.similarity
            .partial_cmp(&x.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    pairs
}

/// Strategy for generating candidate pairs before TSED scoring.
///
/// The cartesian product is fastest at small N because LSH preprocessing
/// (signature computation, bucketing) costs more than it saves. Past a
/// couple hundred functions the dominant cost flips to per-pair TSED, and
/// the LSH pre-filter cuts the number of pairs that get scored by an
/// order of magnitude or more. `Default` picks based on `functions.len()`.
#[derive(Debug, Clone)]
pub struct CandidateStrategy {
    /// Switch to LSH when `functions.len() >= lsh_min_functions`. `None`
    /// disables LSH entirely (always full cartesian product); useful in
    /// tests that want the deterministic pair set.
    pub lsh_min_functions: Option<usize>,
    /// Tuning for the LSH path. Ignored when `lsh_min_functions` is `None`
    /// or `functions.len()` is below it.
    pub lsh: LshOptions,
}

impl Default for CandidateStrategy {
    fn default() -> Self {
        // ~200 functions ≈ 19,900 pairs at full cartesian. Below this the
        // cartesian path is fastest in practice; above it LSH starts
        // earning its preprocessing cost.
        Self {
            lsh_min_functions: Some(200),
            lsh: LshOptions::default(),
        }
    }
}

/// Compute pairwise similarity for the candidate pairs in `functions` and
/// return the `(i, j, similarity)` triples that exceed `threshold`.
///
/// Lower-level building block shared by [`find_similar_functions`] (which
/// materialises [`SimilarPair`]s with references) and clustering callers
/// (which only need indices). Auto-selects between the cartesian product
/// and an LSH-based candidate filter via [`CandidateStrategy::default`];
/// for explicit control use [`find_similar_pair_indices_with_strategy`].
pub fn find_similar_pair_indices(
    functions: &[FunctionDef],
    threshold: f64,
    opts: &TSEDOptions,
) -> Vec<(usize, usize, f64)> {
    find_similar_pair_indices_with_strategy(
        functions,
        threshold,
        opts,
        &CandidateStrategy::default(),
    )
}

/// [`find_similar_pair_indices`] with an explicit `strategy`. Prefer the
/// auto-dispatching wrapper unless you need to force a specific path
/// (most callers — and every test that snapshots TSED scores — should use
/// the wrapper).
pub fn find_similar_pair_indices_with_strategy(
    functions: &[FunctionDef],
    threshold: f64,
    opts: &TSEDOptions,
    strategy: &CandidateStrategy,
) -> Vec<(usize, usize, f64)> {
    let started = Instant::now();
    let use_lsh = strategy
        .lsh_min_functions
        .is_some_and(|min_n| functions.len() >= min_n);
    let candidates: Vec<(usize, usize)> = if use_lsh {
        lsh_candidate_pairs(functions, &strategy.lsh)
    } else {
        enumerate_candidate_pairs(functions.len()).collect()
    };
    let candidate_count = candidates.len();
    let candidate_elapsed = started.elapsed();
    let score_started = Instant::now();
    let mut pairs: Vec<_> = candidates
        .into_par_iter()
        .filter_map(|(i, j)| {
            let similarity = calculate_tsed(&functions[i].tree, &functions[j].tree, opts);
            (similarity >= threshold).then_some((i, j, similarity))
        })
        .collect();
    pairs.sort_by_key(|(i, j, _)| (*i, *j));
    tracing::debug!(
        target: PROFILE_TARGET,
        functions = functions.len(),
        threshold,
        strategy = if use_lsh { "lsh" } else { "cartesian" },
        candidate_count,
        matched_pair_count = pairs.len(),
        candidate_ms = candidate_elapsed.as_secs_f64() * 1000.0,
        score_ms = score_started.elapsed().as_secs_f64() * 1000.0,
        total_ms = started.elapsed().as_secs_f64() * 1000.0,
        "similarity pair scoring finished"
    );
    pairs
}

/// Group `(index_a, index_b, similarity)` triples into complete-link clusters
/// cut at `threshold`.
///
/// "Complete-link" means every pair of members in an output cluster had a
/// recorded similarity `>= threshold`: no member sneaks in via a weaker
/// transitive chain (`A ~ B ~ C` with `A` and `C` unrelated). Output is a
/// partition of the active items, so an item never appears in two clusters
/// even when it is similar to functions in different families.
///
/// Items appearing in `pairs` but never merged past their first partnership
/// still come back as 2-clusters; items not present in any pair are not
/// returned. Clusters are sorted by `max_similarity` desc, then by size desc.
pub fn cluster_similar_pairs(pairs: &[(usize, usize, f64)], threshold: f64) -> Vec<SimilarCluster> {
    let started = Instant::now();
    if pairs.is_empty() {
        tracing::debug!(
            target: PROFILE_TARGET,
            pair_count = 0usize,
            cluster_count = 0usize,
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "similarity clustering skipped"
        );
        return Vec::new();
    }

    // Densely re-index the items that actually appear in pairs. Working in
    // local index space keeps the similarity hashmap small even when the
    // caller's index space is sparse (e.g. one cluster carved out of a
    // 10k-function corpus).
    let mut active_set: HashSet<usize> = HashSet::new();
    for (a, b, _) in pairs {
        active_set.insert(*a);
        active_set.insert(*b);
    }
    let mut active: Vec<usize> = active_set.into_iter().collect();
    active.sort();
    let local_of: HashMap<usize, usize> =
        active.iter().enumerate().map(|(li, &i)| (i, li)).collect();

    // Sparse similarity matrix keyed by sorted local indices. Duplicate
    // input triples for the same pair keep the highest score.
    let mut sim: HashMap<(usize, usize), f64> = HashMap::with_capacity(pairs.len());
    for (a, b, s) in pairs {
        if *s < threshold {
            continue;
        }
        let (Some(&la), Some(&lb)) = (local_of.get(a), local_of.get(b)) else {
            continue;
        };
        if la == lb {
            continue;
        }
        sim.entry(sorted_key(la, lb))
            .and_modify(|cur| {
                if *s > *cur {
                    *cur = *s;
                }
            })
            .or_insert(*s);
    }

    let full_pair_count = complete_pair_count(active.len());
    if active.len() >= 2 && sim.len() == full_pair_count {
        let (min_similarity, max_similarity) = sim_minmax(sim.values().copied());
        let out = vec![SimilarCluster {
            members: active,
            min_similarity,
            max_similarity,
        }];
        tracing::debug!(
            target: PROFILE_TARGET,
            pair_count = pairs.len(),
            active_count = out[0].members.len(),
            cluster_count = out.len(),
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "similarity clustering finished"
        );
        return out;
    }

    // Each slot holds the current cluster (sorted local indices) or `None`
    // once it has been merged into another.
    let mut slots: Vec<Option<Vec<usize>>> = (0..active.len()).map(|li| Some(vec![li])).collect();
    let mut cluster_sim = sim.clone();
    let mut heap = BinaryHeap::with_capacity(cluster_sim.len());
    for (&(ci, cj), &score) in &cluster_sim {
        if score >= threshold {
            heap.push(MergeCandidate { score, ci, cj });
        }
    }

    while let Some((ci, cj)) = pop_best_merge(&mut heap, &slots, &cluster_sim, threshold) {
        let Some(mut moved) = slots[cj].take() else {
            break;
        };
        if let Some(target) = slots[ci].as_mut() {
            target.append(&mut moved);
            target.sort();
        }
        update_cluster_similarities(ci, cj, &slots, threshold, &mut cluster_sim, &mut heap);
    }

    let mut out: Vec<SimilarCluster> = slots
        .into_iter()
        .flatten()
        .filter(|c| c.len() >= 2)
        .map(|c| {
            let (min_s, max_s) = internal_minmax(&c, &sim);
            let members: Vec<usize> = c.iter().filter_map(|li| active.get(*li).copied()).collect();
            SimilarCluster {
                members,
                min_similarity: min_s,
                max_similarity: max_s,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.max_similarity
            .partial_cmp(&a.max_similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.members.len().cmp(&a.members.len()))
    });
    tracing::debug!(
        target: PROFILE_TARGET,
        pair_count = pairs.len(),
        active_count = active.len(),
        cluster_count = out.len(),
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "similarity clustering finished"
    );
    out
}

/// Enumerate candidate pairs `(i, j)` with `i < j` for an index range of
/// size `n`. Currently the full cartesian product (O(n²)); replacing this
/// with an LSH-based candidate generator is the planned escape hatch when
/// pair counts start dominating runtime.
fn enumerate_candidate_pairs(n: usize) -> impl Iterator<Item = (usize, usize)> {
    (0..n).flat_map(move |i| (i + 1..n).map(move |j| (i, j)))
}

fn sorted_key(a: usize, b: usize) -> (usize, usize) {
    (a.min(b), a.max(b))
}

fn complete_pair_count(n: usize) -> usize {
    n.saturating_mul(n.saturating_sub(1)) / 2
}

fn sim_minmax(values: impl IntoIterator<Item = f64>) -> (f64, f64) {
    values
        .into_iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(min_s, max_s), s| {
            (min_s.min(s), max_s.max(s))
        })
}

#[derive(Debug, Clone, Copy)]
struct MergeCandidate {
    score: f64,
    ci: usize,
    cj: usize,
}

impl PartialEq for MergeCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.score.total_cmp(&other.score) == Ordering::Equal
            && self.ci == other.ci
            && self.cj == other.cj
    }
}

impl Eq for MergeCandidate {}

impl PartialOrd for MergeCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MergeCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            // BinaryHeap is max-first; reverse indices so lower `(ci, cj)`
            // wins ties, matching the previous full-scan behaviour.
            .then_with(|| other.ci.cmp(&self.ci))
            .then_with(|| other.cj.cmp(&self.cj))
    }
}

/// Return the best live merge candidate: highest complete-link similarity
/// (= min cross-cluster pair) above `threshold`. Stale heap entries are
/// discarded lazily after their cluster was merged or their score changed.
fn pop_best_merge(
    heap: &mut BinaryHeap<MergeCandidate>,
    slots: &[Option<Vec<usize>>],
    cluster_sim: &HashMap<(usize, usize), f64>,
    threshold: f64,
) -> Option<(usize, usize)> {
    while let Some(candidate) = heap.pop() {
        if candidate.score < threshold {
            return None;
        }
        if !has_live_slot(slots, candidate.ci) || !has_live_slot(slots, candidate.cj) {
            continue;
        }
        if !candidate_score_matches(candidate, cluster_sim) {
            continue;
        }
        return Some((candidate.ci, candidate.cj));
    }
    None
}

fn has_live_slot(slots: &[Option<Vec<usize>>], index: usize) -> bool {
    slots.get(index).is_some_and(Option::is_some)
}

fn candidate_score_matches(
    candidate: MergeCandidate,
    cluster_sim: &HashMap<(usize, usize), f64>,
) -> bool {
    cluster_sim
        .get(&sorted_key(candidate.ci, candidate.cj))
        .is_some_and(|current| current.total_cmp(&candidate.score) == Ordering::Equal)
}

fn update_cluster_similarities(
    kept: usize,
    removed: usize,
    slots: &[Option<Vec<usize>>],
    threshold: f64,
    cluster_sim: &mut HashMap<(usize, usize), f64>,
    heap: &mut BinaryHeap<MergeCandidate>,
) {
    cluster_sim.remove(&sorted_key(kept, removed));
    for (other, slot) in slots.iter().enumerate() {
        if other == kept || other == removed || slot.is_none() {
            continue;
        }
        let kept_key = sorted_key(kept, other);
        let removed_key = sorted_key(removed, other);
        let Some(next) = next_merged_similarity(cluster_sim, kept_key, removed_key) else {
            cluster_sim.remove(&kept_key);
            continue;
        };
        if next >= threshold {
            cluster_sim.insert(kept_key, next);
            heap.push(MergeCandidate {
                score: next,
                ci: kept_key.0,
                cj: kept_key.1,
            });
        } else {
            cluster_sim.remove(&kept_key);
        }
    }
}

fn next_merged_similarity(
    cluster_sim: &mut HashMap<(usize, usize), f64>,
    kept_key: (usize, usize),
    removed_key: (usize, usize),
) -> Option<f64> {
    let kept_sim = cluster_sim.get(&kept_key).copied()?;
    let removed_sim = cluster_sim.remove(&removed_key)?;
    Some(kept_sim.min(removed_sim))
}

fn pairwise_similarities<'a>(
    members: &'a [usize],
    sim: &'a HashMap<(usize, usize), f64>,
) -> impl Iterator<Item = f64> + 'a {
    members.iter().enumerate().flat_map(move |(i, &x)| {
        members[i + 1..]
            .iter()
            .filter_map(move |&y| sim.get(&sorted_key(x, y)).copied())
    })
}

fn internal_minmax(members: &[usize], sim: &HashMap<(usize, usize), f64>) -> (f64, f64) {
    sim_minmax(pairwise_similarities(members, sim))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::TreeNode;

    fn def(name: &str, tree: TreeNode) -> FunctionDef {
        FunctionDef {
            name: name.to_owned(),
            start_line: 1,
            end_line: 10,
            is_test: false,
            signature: None,
            tree,
        }
    }

    fn fn_tree(kinds: &[&str]) -> TreeNode {
        let children = kinds.iter().map(|k| TreeNode::leaf(*k)).collect();
        TreeNode::with_children("Block", "", children)
    }

    #[test]
    fn identical_functions_are_reported() {
        let funcs = vec![
            def("a", fn_tree(&["Let", "Call", "Return"])),
            def("b", fn_tree(&["Let", "Call", "Return"])),
        ];
        let pairs = find_similar_functions(&funcs, 0.9, &TSEDOptions::default());
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a.name, "a");
        assert_eq!(pairs[0].b.name, "b");
        assert!((pairs[0].similarity - 1.0).abs() < 1e-9);
    }

    #[test]
    fn dissimilar_functions_filtered_out() {
        let funcs = vec![
            def("a", fn_tree(&["Let", "Call", "Return"])),
            def(
                "b",
                fn_tree(&["If", "While", "For", "Match", "Break", "Loop"]),
            ),
        ];
        let pairs = find_similar_functions(&funcs, 0.9, &TSEDOptions::default());
        assert!(pairs.is_empty());
    }

    #[test]
    fn line_count_includes_both_endpoints() {
        let f = FunctionDef {
            name: "f".into(),
            start_line: 5,
            end_line: 10,
            is_test: false,
            signature: None,
            tree: TreeNode::leaf("Block"),
        };
        assert_eq!(f.line_count(), 6);
    }

    #[test]
    fn line_count_is_one_for_single_line() {
        let f = FunctionDef {
            name: "f".into(),
            start_line: 7,
            end_line: 7,
            is_test: false,
            signature: None,
            tree: TreeNode::leaf("Block"),
        };
        assert_eq!(f.line_count(), 1);
    }

    #[test]
    fn line_count_saturates_when_end_before_start() {
        let f = FunctionDef {
            name: "f".into(),
            start_line: 10,
            end_line: 5,
            is_test: false,
            signature: None,
            tree: TreeNode::leaf("Block"),
        };
        assert_eq!(f.line_count(), 1);
    }

    #[test]
    fn body_tree_prefers_function_block_child_and_falls_back_to_root() {
        let with_signature = FunctionDef {
            name: "f".into(),
            start_line: 1,
            end_line: 4,
            is_test: false,
            signature: None,
            tree: TreeNode::with_children(
                "Function",
                "",
                vec![TreeNode::leaf("FnSignature"), TreeNode::leaf("Block")],
            ),
        };
        assert_eq!(with_signature.body_tree().label, "Block");

        let body_only = FunctionDef {
            name: "g".into(),
            start_line: 1,
            end_line: 4,
            is_test: false,
            signature: None,
            tree: TreeNode::leaf("Body"),
        };
        assert_eq!(body_only.body_tree().label, "Body");
    }

    #[test]
    fn pairs_sorted_by_similarity_desc() {
        let funcs = vec![
            def("base", fn_tree(&["Let", "Call", "Return"])),
            def("close", fn_tree(&["Let", "Call", "Return"])),
            def("far", fn_tree(&["Let", "If", "Match"])),
        ];
        let pairs = find_similar_functions(&funcs, 0.0, &TSEDOptions::default());
        assert!(pairs.len() >= 2);
        for w in pairs.windows(2) {
            assert!(w[0].similarity >= w[1].similarity);
        }
    }

    #[test]
    fn cluster_empty_input_returns_empty_output() {
        let clusters = cluster_similar_pairs(&[], 0.85);
        assert!(clusters.is_empty());
    }

    #[test]
    fn cluster_single_pair_yields_one_two_member_cluster() {
        let clusters = cluster_similar_pairs(&[(0, 1, 0.92)], 0.85);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members, vec![0, 1]);
        assert!((clusters[0].min_similarity - 0.92).abs() < 1e-9);
        assert!((clusters[0].max_similarity - 0.92).abs() < 1e-9);
    }

    #[test]
    fn cluster_full_clique_merges_into_single_group() {
        // Three items pairwise similar: should form one cluster of size 3.
        let pairs = [(0, 1, 0.95), (0, 2, 0.91), (1, 2, 0.93)];
        let clusters = cluster_similar_pairs(&pairs, 0.85);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members, vec![0, 1, 2]);
        assert!((clusters[0].min_similarity - 0.91).abs() < 1e-9);
        assert!((clusters[0].max_similarity - 0.95).abs() < 1e-9);
    }

    #[test]
    fn cluster_chain_does_not_merge_through_missing_edge() {
        // A~B and B~C are above threshold but A~C is missing (below
        // threshold and so absent from input). Complete-link must NOT
        // pull A and C into the same cluster: B can only land in one.
        let pairs = [(0, 1, 0.90), (1, 2, 0.90)];
        let clusters = cluster_similar_pairs(&pairs, 0.85);
        assert_eq!(
            clusters.len(),
            1,
            "expected one of the two pairs to win the merge",
        );
        assert_eq!(clusters[0].members.len(), 2);
        // The other pair's items don't both appear; B is in exactly one cluster.
        let appears = |x: usize| clusters[0].members.contains(&x);
        assert!(appears(1), "shared vertex must be in the surviving cluster");
        assert!(appears(0) ^ appears(2), "exactly one of A/C, not both");
    }

    #[test]
    fn cluster_two_disjoint_cliques_stay_separate() {
        // Two independent triangles, no cross edges.
        let pairs = [
            (0, 1, 0.95),
            (0, 2, 0.93),
            (1, 2, 0.94),
            (10, 11, 0.92),
            (10, 12, 0.91),
            (11, 12, 0.90),
        ];
        let mut clusters = cluster_similar_pairs(&pairs, 0.85);
        clusters.sort_by_key(|c| c.members[0]);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].members, vec![0, 1, 2]);
        assert_eq!(clusters[1].members, vec![10, 11, 12]);
    }

    #[test]
    fn cluster_drops_pairs_below_threshold_defensively() {
        // The clusterer is normally fed pre-filtered pairs, but defensively
        // drop any triple whose score has slipped under the threshold so
        // upstream bugs don't relax cohesion silently.
        let pairs = [(0, 1, 0.95), (1, 2, 0.5)];
        let clusters = cluster_similar_pairs(&pairs, 0.85);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members, vec![0, 1]);
    }

    #[test]
    fn cluster_output_sorted_by_max_similarity_desc() {
        let pairs = [(0, 1, 0.86), (10, 11, 0.99)];
        let clusters = cluster_similar_pairs(&pairs, 0.85);
        assert_eq!(clusters.len(), 2);
        assert!(clusters[0].max_similarity >= clusters[1].max_similarity);
        assert_eq!(clusters[0].members, vec![10, 11]);
    }

    #[test]
    fn strategy_default_uses_cartesian_below_threshold() {
        // Below the auto-LSH threshold the strategy's behaviour should
        // match the deterministic cartesian path: every pair is scored,
        // and an identical-pair has score 1.0.
        let funcs = vec![
            def("a", fn_tree(&["Let", "Call", "Return"])),
            def("b", fn_tree(&["Let", "Call", "Return"])),
        ];
        let pairs = find_similar_pair_indices(&funcs, 0.5, &TSEDOptions::default());
        assert_eq!(pairs.len(), 1);
        assert_eq!((pairs[0].0, pairs[0].1), (0, 1));
        assert!((pairs[0].2 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn strategy_with_lsh_disabled_keeps_full_cartesian() {
        // Forcing the LSH gate off must not change which pairs are
        // returned (full O(N²) sweep).
        let funcs = vec![
            def("a", fn_tree(&["Let", "Call", "Return"])),
            def("b", fn_tree(&["Let", "Call", "Return"])),
            def("c", fn_tree(&["If", "While", "Match"])),
        ];
        let strategy = CandidateStrategy {
            lsh_min_functions: None,
            ..Default::default()
        };
        let pairs = find_similar_pair_indices_with_strategy(
            &funcs,
            0.0,
            &TSEDOptions::default(),
            &strategy,
        );
        // Cartesian sees all 3 pairs; LSH might prune them.
        assert_eq!(pairs.len(), 3);
    }

    #[test]
    fn strategy_with_lsh_forced_on_still_recovers_identicals() {
        // Setting `lsh_min_functions: Some(0)` forces the LSH path even
        // for tiny inputs. Identical functions must still emerge as a
        // candidate pair after TSED scoring; otherwise a corpus past the
        // auto-trip threshold would silently lose true near-duplicates.
        let funcs = vec![
            def("a", fn_tree(&["Let", "Call", "Add", "Mul", "Return"])),
            def("b", fn_tree(&["Let", "Call", "Add", "Mul", "Return"])),
            def("c", fn_tree(&["If", "While", "Match", "Break", "Loop"])),
        ];
        let strategy = CandidateStrategy {
            lsh_min_functions: Some(0),
            ..Default::default()
        };
        let pairs = find_similar_pair_indices_with_strategy(
            &funcs,
            0.85,
            &TSEDOptions::default(),
            &strategy,
        );
        // The (a, b) pair is identical so its TSED is 1.0; whatever LSH
        // candidates other pairs contribute, this one must survive.
        assert!(
            pairs.iter().any(|(i, j, _)| (*i, *j) == (0, 1)),
            "LSH path must keep recall on identicals: {pairs:?}",
        );
    }

    #[test]
    fn cluster_partial_clique_only_merges_full_complete_link() {
        // 4-node graph: {0,1,2} is a triangle, plus edge (2,3). 3 cannot
        // join the triangle because edges (0,3) and (1,3) are missing.
        let pairs = [(0, 1, 0.95), (0, 2, 0.93), (1, 2, 0.94), (2, 3, 0.91)];
        let clusters = cluster_similar_pairs(&pairs, 0.85);
        // Exactly one of the two competing clusterings wins:
        //   A) {0,1,2} (triangle) + dropped (2,3)
        //   B) {2,3} (two-pair) + dropped triangle pairs
        // The greedy picks the merge with the highest min-cross first,
        // which is the triangle (min 0.93), so we expect option A.
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members, vec![0, 1, 2]);
    }
}
