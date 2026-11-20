//! Community detection over a weighted module graph, compared against
//! the grouping the repository *declares*.
//!
//! `coupling` and `layers` answer whether the dependency **direction** is
//! sane. This answers the orthogonal question: is the **grouping** sane?
//! Every repository declares a partition of its files — the directory or
//! module each one is filed under — and the edge graph forms a partition
//! of its own. Where the two disagree, one of them is wrong.
//!
//! Two numbers carry the report:
//!
//! * **`detected_modularity`** — the modularity `Q_γ` of the partition
//!   this module finds, i.e. the best grouping the edges support.
//! * **`declared_modularity`** — the same `Q_γ` computed for the declared
//!   partition, with no re-grouping at all.
//!
//! A declared score close to the detected one means the architecture
//! matches reality. The gap between them is what the misfiled rows are
//! made of.
//!
//! What an edge *is* is the caller's business: a static reference, a
//! co-change in the repository's history, or a blend of the two. Weights
//! are `f64` so a caller can normalise and combine sources; nothing here
//! assumes integer weights.
//!
//! # Algorithms
//!
//! Two partitioners, both scoring the same Reichardt–Bornholdt
//! modularity `Q_γ = Σ_c [ L_c/m − γ·(D_c/2m)² ]` (Newman's `Q` at
//! `γ = 1`):
//!
//! * [`CommunityAlgorithm::Greedy`] — Clauset–Newman–Moore agglomeration:
//!   repeatedly take the single merge with the best `ΔQ`. Simple, and
//!   slow — every merge scans every inter-community pair.
//! * [`CommunityAlgorithm::Leiden`] — Traag, Waltman & van Eck (2019):
//!   fast local moving, a refinement pass that only merges well-connected
//!   subsets inside each community, then aggregation, repeated until
//!   stable. Finds partitions at least as good on real graphs, guarantees
//!   every community is internally connected, and runs in near-linear
//!   time per pass.
//!
//! # Determinism
//!
//! Community detection is usually order-dependent: Louvain visits nodes
//! in input order and Leiden's refinement is randomised, so the same
//! graph gives different answers on different runs. An analyzer that does
//! that is useless — a report an agent cannot diff against the last one
//! is not evidence. Both partitioners here are made total:
//!
//! * nodes are sorted by id before anything else, so the node indices
//!   every later step keys on are a function of the *set* of nodes, not
//!   of the order they arrived in;
//! * parallel edges are summed in sorted-weight order, so floating-point
//!   addition cannot carry arrival order into a weight;
//! * a community is labelled by its smallest member index, so labels are
//!   canonical rather than allocation-ordered;
//! * every choice — the greedy merge, a local move, a refinement merge —
//!   scans candidates in ascending label order and improves on a strict
//!   `>`, so an exact tie resolves to the smallest label. Leiden's
//!   randomised refinement is replaced by its `θ → 0` limit: the
//!   best-gain merge.
//!
//! The result is invariant under permutation of the input node and edge
//! lists, which [`detect_communities`] is property-tested for under both
//! algorithms.
//!
//! # Limits
//!
//! Modularity has a **resolution limit**: below a size that depends on
//! the total edge weight, a genuine small cluster scores better merged
//! into a neighbour than kept apart. `γ > 1` favours smaller communities
//! and `γ < 1` larger ones; it shifts the limit rather than removing it,
//! and no value of it is "correct". Every community reports its size so a
//! reader who knows the codebase can tell when a cluster got absorbed.
//!
//! On a small or densely-connected graph nearly everything lands in one
//! community. That is a real answer about the graph, and the report says
//! so rather than splitting the noise into plausible-looking clusters.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Smallest community reported by default. A singleton community is a
/// node the edges gave no home to, which is a fact about the node rather
/// than a cluster worth naming.
pub const DEFAULT_MIN_COMMUNITY: usize = 2;

/// Newman's modularity: the resolution at which `Q_γ` is the textbook
/// `Q`.
pub const DEFAULT_RESOLUTION: f64 = 1.0;

/// A node whose community-mate sets under two partitions overlap below
/// this Jaccard index is listed as divergent. Half is where the two
/// partitions stop agreeing on more of the node's neighbourhood than they
/// disagree on.
pub const DIVERGENT_BELOW: f64 = 0.5;

/// A move or merge is taken only when it strictly improves `Q` by more
/// than this, relative to the total edge weight. The tolerance keeps
/// floating-point noise around zero from driving a change the exact
/// arithmetic would decline, which on a symmetric graph is where the last
/// shred of order-dependence would otherwise hide.
const MERGE_EPSILON: f64 = 1e-12;

/// Which partitioner [`detect_communities`] runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CommunityAlgorithm {
    /// Clauset–Newman–Moore greedy agglomeration.
    #[default]
    Greedy,
    /// Deterministic Leiden: local moving, refinement, aggregation.
    Leiden,
}

/// Knobs for one detection run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CommunityParams {
    pub algorithm: CommunityAlgorithm,
    /// `γ` in `Q_γ`. Must be finite and positive; the caller validates.
    pub resolution: f64,
    /// Smallest community listed. Does not change the partition.
    pub min_community: usize,
}

impl Default for CommunityParams {
    fn default() -> Self {
        Self {
            algorithm: CommunityAlgorithm::default(),
            resolution: DEFAULT_RESOLUTION,
            min_community: DEFAULT_MIN_COMMUNITY,
        }
    }
}

/// One node handed to the detector: a stable id, and the group the
/// repository's own structure files it under.
///
/// The caller decides what a node and a declared group are — a file and
/// its directory, a module and its parent module — so this module never
/// has to know the analyzed language's path syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunityNode {
    pub id: String,
    pub declared: String,
}

impl CommunityNode {
    pub fn new(id: impl Into<String>, declared: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            declared: declared.into(),
        }
    }
}

/// One undirected weighted edge. Direction is dropped on purpose:
/// community structure is about which nodes belong together, and a
/// dependency binds both ends whichever way it points.
#[derive(Debug, Clone, PartialEq)]
pub struct CommunityEdge {
    pub a: String,
    pub b: String,
    pub weight: f64,
}

impl CommunityEdge {
    pub fn new(a: impl Into<String>, b: impl Into<String>, weight: impl Into<f64>) -> Self {
        Self {
            a: a.into(),
            b: b.into(),
            weight: weight.into(),
        }
    }
}

/// How many members of a declared group landed in one community.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredShare {
    pub declared: String,
    pub members: usize,
}

/// One detected community.
#[derive(Debug, Clone, PartialEq)]
pub struct Community {
    /// Rank in canonical order (by smallest member), so the same graph
    /// always numbers the same cluster the same way.
    pub id: usize,
    pub size: usize,
    /// Members in ascending id order.
    pub members: Vec<String>,
    /// Declared groups represented in this community, most members
    /// first, ties broken by group name.
    pub breakdown: Vec<DeclaredShare>,
    /// The declared group holding the most members here — the module
    /// this cluster is "really" in.
    pub dominant_declared: String,
    /// Edge weight with both endpoints inside the community.
    pub internal_weight: f64,
    /// Edge weight leaving the community.
    pub external_weight: f64,
}

/// A node whose community is dominated by a declared group other than
/// its own, with the in/out edge weights that argue for the move.
///
/// This is the actionable row: not "these files are related" but "this
/// file is filed here and points there".
#[derive(Debug, Clone, PartialEq)]
pub struct MisfiledMember {
    pub node: String,
    /// The group the repository files it under.
    pub declared: String,
    /// The dominant declared group of the community it clustered into.
    pub suggested: String,
    pub community: usize,
    /// Edge weight to nodes in its own declared group.
    pub weight_to_declared: f64,
    /// Edge weight to nodes in the suggested group.
    pub weight_to_suggested: f64,
    /// `weight_to_suggested - weight_to_declared`: how lopsided the
    /// evidence is, and the ranking key. Size of the community is
    /// deliberately not part of it.
    pub evidence: f64,
}

/// A detected community spread across declared groups with none of them
/// owning it — a feature that never got a home of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanningCommunity {
    pub community: usize,
    pub size: usize,
    pub declared_group_count: usize,
    pub breakdown: Vec<DeclaredShare>,
}

/// The full comparison of the detected partition against the declared
/// one.
#[derive(Debug, Clone, PartialEq)]
pub struct CommunityReport {
    pub node_count: usize,
    /// Distinct undirected node pairs carrying positive weight.
    pub edge_count: usize,
    pub total_weight: f64,
    /// Nodes with no edge at all. They cannot be clustered, so they say
    /// nothing either way and are excluded from every listing.
    pub isolated_node_count: usize,
    /// Communities detected, before `min_community` filtering.
    pub community_count: usize,
    pub declared_group_count: usize,
    /// Members of the largest detected community. Read next to
    /// `node_count`: one community holding nearly every node is the
    /// honest answer on a small or densely-connected graph, not a
    /// finding.
    pub largest_community: usize,
    pub detected_modularity: f64,
    pub declared_modularity: f64,
    /// `detected - declared`. Zero means the declared boundaries are
    /// exactly the ones the edges form.
    pub modularity_gap: f64,
    /// `declared / detected`, when the detected partition scored above
    /// zero. `1.0` means the declared architecture scores exactly what
    /// the search found, and `None` that the graph has no community
    /// structure to compare against. The ratio can exceed `1.0`: neither
    /// partitioner is guaranteed optimal, so a declared grouping is
    /// occasionally the better of the two — which is the strongest
    /// possible "the boundaries are right" answer.
    pub declared_quality: Option<f64>,
    /// Communities at least `min_community` members large, canonical
    /// order.
    pub communities: Vec<Community>,
    /// Misfiled members, strongest evidence first.
    pub misfiled: Vec<MisfiledMember>,
    /// Spanning communities, widest span first.
    pub spanning: Vec<SpanningCommunity>,
}

/// Detect communities in `edges` over `nodes` and compare the result
/// against the declared grouping.
///
/// `params.min_community` bounds which communities are reported; it does
/// not change the partition, so the modularity figures and
/// `community_count` describe the whole graph either way. Nodes named
/// only by an edge are ignored: the node list is the population.
pub fn detect_communities(
    nodes: &[CommunityNode],
    edges: &[CommunityEdge],
    params: CommunityParams,
) -> CommunityReport {
    let graph = Graph::build(nodes, edges);
    let gamma = params.resolution;
    let partition = graph.partition(params.algorithm, gamma);
    let detected = graph.modularity(&partition.group_of, partition.count, gamma);
    let declared = graph.modularity(&graph.declared_of, graph.group_names.len(), gamma);
    let facts = graph.community_facts(&partition);
    let report_ready = facts
        .iter()
        .filter(|f| f.members.len() >= params.min_community);

    CommunityReport {
        node_count: graph.ids.len(),
        edge_count: graph.pairs.len(),
        total_weight: graph.total_weight,
        isolated_node_count: graph.isolated_count(),
        community_count: facts.len(),
        declared_group_count: graph.group_names.len(),
        largest_community: facts.iter().map(|f| f.members.len()).max().unwrap_or(0),
        detected_modularity: detected,
        declared_modularity: declared,
        modularity_gap: detected - declared,
        declared_quality: if detected > 0.0 {
            Some(declared / detected)
        } else {
            None
        },
        communities: report_ready.clone().map(|f| graph.render(f)).collect(),
        misfiled: graph.misfiled(report_ready.clone()),
        spanning: report_ready.filter_map(|f| graph.spanning(f)).collect(),
    }
}

/// The bare partition of one graph: which community each node landed in,
/// and which nodes had no edge to land anywhere with.
///
/// This is the form two partitions of the same population are compared
/// in — see [`compare_partitions`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NodePartition {
    /// Canonical community index per node id, isolated nodes included.
    pub community_of: BTreeMap<String, usize>,
    /// Nodes with no positive-weight edge.
    pub isolated: BTreeSet<String>,
}

/// Partition `edges` over `nodes` without scoring it against anything.
pub fn partition_nodes(
    nodes: &[CommunityNode],
    edges: &[CommunityEdge],
    algorithm: CommunityAlgorithm,
    resolution: f64,
) -> NodePartition {
    let graph = Graph::build(nodes, edges);
    let partition = graph.partition(algorithm, resolution);
    NodePartition {
        community_of: graph
            .ids
            .iter()
            .cloned()
            .zip(partition.group_of.iter().copied())
            .collect(),
        isolated: graph
            .ids
            .iter()
            .zip(&graph.degree)
            .filter(|(_, d)| **d <= 0.0)
            .map(|(id, _)| id.clone())
            .collect(),
    }
}

/// A node whose community-mates differ between two partitions.
#[derive(Debug, Clone, PartialEq)]
pub struct DivergentMember {
    pub node: String,
    /// Community-mates (excluding the node) under the first partition.
    pub first_mates: usize,
    /// Community-mates under the second partition.
    pub second_mates: usize,
    /// Mates the two partitions agree on.
    pub shared_mates: usize,
    /// `shared / |first ∪ second|`.
    pub jaccard: f64,
}

/// How far two partitions of the same population agree.
#[derive(Debug, Clone, PartialEq)]
pub struct PartitionAgreement {
    /// Nodes compared: present and non-isolated in both partitions.
    pub compared: usize,
    /// Normalised mutual information `2·I(A;B) / (H(A) + H(B))`, `1.0`
    /// for identical partitions and `0.0` for independent ones. `None`
    /// when fewer than two nodes are compared.
    pub nmi: Option<f64>,
    /// Nodes whose mate sets overlap below [`DIVERGENT_BELOW`], least
    /// overlap first, then largest combined neighbourhood, then id.
    pub divergent: Vec<DivergentMember>,
}

/// Compare two partitions over the nodes both of them could place.
///
/// A node isolated in either partition is left out: an edgeless node is
/// a singleton by default rather than by evidence, and counting it would
/// let the two sources "agree" on nodes neither saw.
pub fn compare_partitions(first: &NodePartition, second: &NodePartition) -> PartitionAgreement {
    let compared: Vec<(&str, usize, usize)> = first
        .community_of
        .iter()
        .filter(|(id, _)| !first.isolated.contains(*id) && !second.isolated.contains(*id))
        .filter_map(|(id, &a)| Some((id.as_str(), a, *second.community_of.get(id)?)))
        .collect();
    let nmi = (compared.len() >= 2).then(|| {
        let pairs: Vec<(usize, usize)> = compared.iter().map(|&(_, a, b)| (a, b)).collect();
        normalized_mutual_information(&pairs)
    });

    let mut first_groups: BTreeMap<usize, BTreeSet<&str>> = BTreeMap::new();
    let mut second_groups: BTreeMap<usize, BTreeSet<&str>> = BTreeMap::new();
    for &(id, a, b) in &compared {
        first_groups.entry(a).or_default().insert(id);
        second_groups.entry(b).or_default().insert(id);
    }
    let mut divergent: Vec<DivergentMember> = compared
        .iter()
        .filter_map(|&(id, a, b)| {
            let in_first = &first_groups[&a];
            let in_second = &second_groups[&b];
            // Both sets contain the node itself; mates exclude it.
            let shared = in_first.intersection(in_second).count() - 1;
            let union = in_first.union(in_second).count() - 1;
            let jaccard = if union == 0 {
                1.0
            } else {
                shared as f64 / union as f64
            };
            (jaccard < DIVERGENT_BELOW).then(|| DivergentMember {
                node: id.to_owned(),
                first_mates: in_first.len() - 1,
                second_mates: in_second.len() - 1,
                shared_mates: shared,
                jaccard,
            })
        })
        .collect();
    divergent.sort_by(|x, y| {
        x.jaccard
            .total_cmp(&y.jaccard)
            .then_with(|| (y.first_mates + y.second_mates).cmp(&(x.first_mates + x.second_mates)))
            .then_with(|| x.node.cmp(&y.node))
    });
    PartitionAgreement {
        compared: compared.len(),
        nmi,
        divergent,
    }
}

/// `2·I(A;B) / (H(A) + H(B))` over `(label in A, label in B)` pairs.
/// Two single-community partitions carry no information to disagree on
/// and score `1.0`.
fn normalized_mutual_information(pairs: &[(usize, usize)]) -> f64 {
    let n = pairs.len() as f64;
    let mut joint: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    let mut left: BTreeMap<usize, usize> = BTreeMap::new();
    let mut right: BTreeMap<usize, usize> = BTreeMap::new();
    for &(a, b) in pairs {
        *joint.entry((a, b)).or_default() += 1;
        *left.entry(a).or_default() += 1;
        *right.entry(b).or_default() += 1;
    }
    let entropy = |counts: &BTreeMap<usize, usize>| -> f64 {
        counts
            .values()
            .map(|&c| {
                let p = c as f64 / n;
                -p * p.ln()
            })
            .sum()
    };
    let (h_left, h_right) = (entropy(&left), entropy(&right));
    if h_left + h_right <= 0.0 {
        return 1.0;
    }
    let mutual: f64 = joint
        .iter()
        .map(|(&(a, b), &c)| {
            let p = c as f64 / n;
            let pa = left[&a] as f64 / n;
            let pb = right[&b] as f64 / n;
            p * (p / (pa * pb)).ln()
        })
        .sum();
    (2.0 * mutual / (h_left + h_right)).clamp(0.0, 1.0)
}

/// The canonical form every later step reads: nodes in sorted-id order,
/// declared groups as indices into a sorted name table, and one weight
/// per unordered node pair.
struct Graph {
    ids: Vec<String>,
    group_names: Vec<String>,
    /// Declared group index per node, parallel to `ids`.
    declared_of: Vec<usize>,
    /// `(i, j) -> weight` with `i < j`.
    pairs: BTreeMap<(usize, usize), f64>,
    /// Weighted degree per node.
    degree: Vec<f64>,
    /// Sum of all pair weights, each pair counted once.
    total_weight: f64,
}

impl Graph {
    fn build(nodes: &[CommunityNode], edges: &[CommunityEdge]) -> Self {
        let (ids, group_names, declared_of) = node_table(nodes);
        // Endpoints are resolved through a borrow of `ids`, which the
        // struct then takes ownership of, so the table is folded before
        // the graph is assembled rather than by a method on it.
        let index: BTreeMap<&str, usize> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        let mut parallel: BTreeMap<(usize, usize), Vec<f64>> = BTreeMap::new();
        for edge in edges {
            // Self-loops, endpoints outside the node list, and
            // non-positive or non-finite weights say nothing about who
            // belongs with whom, and counting them would inflate
            // `edge_count` for nothing.
            let (Some(&a), Some(&b)) = (index.get(edge.a.as_str()), index.get(edge.b.as_str()))
            else {
                continue;
            };
            if a == b || !informative(edge.weight) {
                continue;
            }
            parallel.entry(ordered(a, b)).or_default().push(edge.weight);
        }
        // Summing in sorted order makes each pair weight a function of
        // the weight multiset, not of the order the edges arrived in.
        let pairs: BTreeMap<(usize, usize), f64> = parallel
            .into_iter()
            .map(|(pair, mut weights)| {
                weights.sort_by(f64::total_cmp);
                (pair, weights.into_iter().sum())
            })
            .collect();
        let mut degree = vec![0.0; ids.len()];
        for (&(a, b), &w) in &pairs {
            degree[a] += w;
            degree[b] += w;
        }
        Self {
            total_weight: pairs.values().sum(),
            ids,
            group_names,
            declared_of,
            pairs,
            degree,
        }
    }

    fn isolated_count(&self) -> usize {
        self.degree.iter().filter(|d| **d <= 0.0).count()
    }

    fn partition(&self, algorithm: CommunityAlgorithm, gamma: f64) -> Partition {
        match algorithm {
            CommunityAlgorithm::Greedy => self.agglomerate(gamma),
            CommunityAlgorithm::Leiden => Partition::from_labels(leiden(self, gamma)),
        }
    }

    /// Reichardt–Bornholdt modularity of an arbitrary partition:
    /// `Q_γ = Σ_c [ L_c/m − γ·(D_c/2m)² ]`, where `L_c` is the edge weight
    /// inside community `c` and `D_c` the summed degree of its members.
    ///
    /// The same function scores the detected and the declared partition,
    /// which is the only reason the two numbers are comparable.
    fn modularity(&self, group_of: &[usize], group_count: usize, gamma: f64) -> f64 {
        if self.total_weight <= 0.0 {
            return 0.0;
        }
        let m = self.total_weight;
        let mut internal = vec![0.0; group_count];
        let mut degree = vec![0.0; group_count];
        for (&(i, j), &w) in &self.pairs {
            if group_of[i] == group_of[j] {
                internal[group_of[i]] += w;
            }
        }
        for (node, &g) in group_of.iter().enumerate() {
            degree[g] += self.degree[node];
        }
        (0..group_count)
            .map(|g| {
                let share = degree[g] / (2.0 * m);
                internal[g] / m - gamma * share * share
            })
            .sum()
    }

    /// Greedy modularity agglomeration, run to the point where no merge
    /// improves `Q_γ`.
    ///
    /// Every merge retires one community, so `n` nodes admit at most
    /// `n - 1` of them. The bound is what keeps a merge that fails to
    /// make progress — one the loop would otherwise re-propose forever —
    /// from hanging the analyzer instead of producing a wrong answer a
    /// test can see.
    fn agglomerate(&self, gamma: f64) -> Partition {
        let mut state = Agglomerator::new(self, gamma);
        for _ in 1..self.ids.len().max(1) {
            let Some((x, y)) = state.best_merge() else {
                break;
            };
            state.merge(x, y);
        }
        state.finish(self.ids.len())
    }

    /// Per-community bookkeeping every listing is derived from, in
    /// canonical order.
    fn community_facts(&self, partition: &Partition) -> Vec<CommunityFacts> {
        let mut facts: Vec<CommunityFacts> = partition
            .members
            .iter()
            .enumerate()
            .map(|(id, members)| CommunityFacts {
                id,
                members: members.clone(),
                breakdown: self.breakdown(members),
                internal_weight: 0.0,
                external_weight: 0.0,
            })
            .collect();
        for (&(i, j), &w) in &self.pairs {
            let (ci, cj) = (partition.group_of[i], partition.group_of[j]);
            if ci == cj {
                facts[ci].internal_weight += w;
            } else {
                facts[ci].external_weight += w;
                facts[cj].external_weight += w;
            }
        }
        facts
    }

    /// Declared groups represented in `members`, most members first and
    /// ties broken by group name so the dominant group is a function of
    /// the set.
    fn breakdown(&self, members: &[usize]) -> Vec<(usize, usize)> {
        let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
        for &node in members {
            *counts.entry(self.declared_of[node]).or_default() += 1;
        }
        let mut shares: Vec<(usize, usize)> = counts.into_iter().collect();
        shares.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        shares
    }

    fn render(&self, facts: &CommunityFacts) -> Community {
        Community {
            id: facts.id,
            size: facts.members.len(),
            members: facts.members.iter().map(|&n| self.ids[n].clone()).collect(),
            breakdown: self.shares(&facts.breakdown),
            dominant_declared: self.group_names[facts.dominant()].clone(),
            internal_weight: facts.internal_weight,
            external_weight: facts.external_weight,
        }
    }

    fn shares(&self, breakdown: &[(usize, usize)]) -> Vec<DeclaredShare> {
        breakdown
            .iter()
            .map(|&(group, members)| DeclaredShare {
                declared: self.group_names[group].clone(),
                members,
            })
            .collect()
    }

    /// A community counts as spanning when it crosses declared groups
    /// and no group holds a majority of it — the shape of a feature
    /// smeared across modules rather than one module reaching into a
    /// neighbour.
    fn spanning(&self, facts: &CommunityFacts) -> Option<SpanningCommunity> {
        let dominant_members = facts.breakdown.first()?.1;
        let spread = facts.breakdown.len() >= 2 && dominant_members * 2 <= facts.members.len();
        spread.then(|| SpanningCommunity {
            community: facts.id,
            size: facts.members.len(),
            declared_group_count: facts.breakdown.len(),
            breakdown: self.shares(&facts.breakdown),
        })
    }

    /// Members whose community is dominated by a group other than their
    /// own, kept only when they have more edge weight to that group than
    /// to the one they are filed under. Without that gate the row is a
    /// naming coincidence rather than evidence.
    fn misfiled<'a>(&self, facts: impl Iterator<Item = &'a CommunityFacts>) -> Vec<MisfiledMember> {
        let reach = self.weights_by_group();
        let mut rows = Vec::new();
        for community in facts {
            let dominant = community.dominant();
            rows.extend(
                community
                    .members
                    .iter()
                    .filter_map(|&node| self.misfiled_row(node, community.id, dominant, &reach)),
            );
        }
        rows.sort_by(|a, b| {
            b.evidence
                .total_cmp(&a.evidence)
                .then_with(|| b.weight_to_suggested.total_cmp(&a.weight_to_suggested))
                .then_with(|| a.node.cmp(&b.node))
        });
        rows
    }

    fn misfiled_row(
        &self,
        node: usize,
        community: usize,
        dominant: usize,
        reach: &[BTreeMap<usize, f64>],
    ) -> Option<MisfiledMember> {
        let declared = self.declared_of[node];
        // A member cannot move into itself. At a granularity where a
        // member and a declared group can share a name — a parent module
        // that is both a node and the group its children are filed in —
        // its own community naturally elects it, and the row would read
        // "move `hooks` into `hooks`".
        if declared == dominant || self.ids[node] == self.group_names[dominant] {
            return None;
        }
        let weight_to_declared = reach[node].get(&declared).copied().unwrap_or(0.0);
        let weight_to_suggested = reach[node].get(&dominant).copied().unwrap_or(0.0);
        (weight_to_suggested > weight_to_declared).then(|| MisfiledMember {
            node: self.ids[node].clone(),
            declared: self.group_names[declared].clone(),
            suggested: self.group_names[dominant].clone(),
            community,
            weight_to_declared,
            weight_to_suggested,
            evidence: weight_to_suggested - weight_to_declared,
        })
    }

    /// For every node, how much edge weight it sends to each declared
    /// group. This is the evidence column of a misfiled row.
    fn weights_by_group(&self) -> Vec<BTreeMap<usize, f64>> {
        let mut reach = vec![BTreeMap::new(); self.ids.len()];
        for (&(i, j), &w) in &self.pairs {
            *reach[i].entry(self.declared_of[j]).or_default() += w;
            *reach[j].entry(self.declared_of[i]).or_default() += w;
        }
        reach
    }
}

/// A detected community mid-flight: members plus the counts every
/// rendered listing reads.
struct CommunityFacts {
    id: usize,
    members: Vec<usize>,
    /// `(declared group index, member count)`, most members first.
    breakdown: Vec<(usize, usize)>,
    internal_weight: f64,
    external_weight: f64,
}

impl CommunityFacts {
    /// The declared group holding the most members. A community always
    /// has at least one member, so the breakdown is never empty.
    fn dominant(&self) -> usize {
        self.breakdown.first().map_or(0, |&(group, _)| group)
    }
}

/// The detected partition in both the forms the report needs: community
/// index per node, and the member list per community.
struct Partition {
    group_of: Vec<usize>,
    members: Vec<Vec<usize>>,
    count: usize,
}

impl Partition {
    /// Relabel an arbitrary per-node labelling canonically: communities
    /// numbered in order of their smallest member.
    fn from_labels(labels: Vec<usize>) -> Self {
        let mut renumber: BTreeMap<usize, usize> = BTreeMap::new();
        let mut members: Vec<Vec<usize>> = Vec::new();
        let mut group_of = vec![0; labels.len()];
        for (node, label) in labels.into_iter().enumerate() {
            let id = *renumber.entry(label).or_insert_with(|| {
                members.push(Vec::new());
                members.len() - 1
            });
            members[id].push(node);
            group_of[node] = id;
        }
        Self {
            count: members.len(),
            group_of,
            members,
        }
    }
}

/// Greedy modularity agglomeration state.
///
/// Communities are keyed by their smallest member index rather than by
/// allocation order, so every map iterated below yields the same
/// sequence for the same graph no matter what order the input arrived
/// in.
struct Agglomerator {
    members: BTreeMap<usize, Vec<usize>>,
    degree: BTreeMap<usize, f64>,
    between: BTreeMap<(usize, usize), f64>,
    neighbors: BTreeMap<usize, BTreeSet<usize>>,
    /// Total edge weight, each pair counted once.
    m: f64,
    gamma: f64,
}

impl Agglomerator {
    fn new(graph: &Graph, gamma: f64) -> Self {
        let mut neighbors: BTreeMap<usize, BTreeSet<usize>> =
            (0..graph.ids.len()).map(|i| (i, BTreeSet::new())).collect();
        for &(i, j) in graph.pairs.keys() {
            neighbors.entry(i).or_default().insert(j);
            neighbors.entry(j).or_default().insert(i);
        }
        Self {
            members: (0..graph.ids.len()).map(|i| (i, vec![i])).collect(),
            degree: graph.degree.iter().copied().enumerate().collect(),
            between: graph.pairs.clone(),
            neighbors,
            m: graph.total_weight,
            gamma,
        }
    }

    /// The merge with the largest modularity gain, or `None` once no
    /// merge helps.
    ///
    /// Merging communities `x` and `y` changes `Q_γ` by
    /// `w_xy/m − γ·D_x·D_y/(2m²)`: the weight the merge internalises,
    /// minus what two groups that well-connected would share by chance.
    /// Scanning `between` in ascending key order and improving on a
    /// strict `>` makes an exact tie resolve to the smallest pair.
    fn best_merge(&self) -> Option<(usize, usize)> {
        if self.m <= 0.0 {
            return None;
        }
        let mut best: Option<((usize, usize), f64)> = None;
        for (&(x, y), &w) in &self.between {
            let dx = self.degree.get(&x).copied().unwrap_or(0.0);
            let dy = self.degree.get(&y).copied().unwrap_or(0.0);
            let gain = w / self.m - self.gamma * dx * dy / (2.0 * self.m * self.m);
            if best.is_none_or(|(_, top)| gain > top) {
                best = Some(((x, y), gain));
            }
        }
        best.filter(|&(_, gain)| improves(gain))
            .map(|(pair, _)| pair)
    }

    /// Fold `y` into `x` (`x < y`, so the surviving label stays the
    /// smallest member index).
    fn merge(&mut self, x: usize, y: usize) {
        let absorbed = self.members.remove(&y).unwrap_or_default();
        self.members.entry(x).or_default().extend(absorbed);
        if let Some(members) = self.members.get_mut(&x) {
            members.sort_unstable();
        }
        let dy = self.degree.remove(&y).unwrap_or(0.0);
        *self.degree.entry(x).or_default() += dy;

        for z in self.neighbors.remove(&y).unwrap_or_default() {
            self.neighbors.entry(z).or_default().remove(&y);
            let w = self.between.remove(&ordered(y, z)).unwrap_or(0.0);
            if z == x {
                continue;
            }
            *self.between.entry(ordered(x, z)).or_default() += w;
            self.neighbors.entry(z).or_default().insert(x);
            self.neighbors.entry(x).or_default().insert(z);
        }
        self.neighbors.entry(x).or_default().remove(&y);
        self.between.remove(&ordered(x, y));
    }

    /// Renumber the surviving communities 0..n in canonical (smallest
    /// member) order.
    fn finish(self, node_count: usize) -> Partition {
        let mut group_of = vec![0; node_count];
        let members: Vec<Vec<usize>> = self.members.into_values().collect();
        for (id, community) in members.iter().enumerate() {
            for &node in community {
                group_of[node] = id;
            }
        }
        Partition {
            count: members.len(),
            group_of,
            members,
        }
    }
}

/// The graph one Leiden level works on: the original graph at the first
/// level, an aggregate of it at every later one. Node `i` of a level is
/// ordered by the smallest original node it holds, so the order stays
/// canonical through aggregation.
struct LevelGraph {
    /// Neighbours per node with the pair weight, ascending by index, no
    /// self entries.
    adjacency: Vec<Vec<(usize, f64)>>,
    /// Weight internal to an aggregate node, each pair counted once.
    self_weight: Vec<f64>,
    /// `k_i`: twice the internal weight plus every incident edge.
    degree: Vec<f64>,
}

impl LevelGraph {
    fn from_graph(graph: &Graph) -> Self {
        let mut adjacency = vec![Vec::new(); graph.ids.len()];
        for (&(i, j), &w) in &graph.pairs {
            adjacency[i].push((j, w));
            adjacency[j].push((i, w));
        }
        for row in &mut adjacency {
            row.sort_by_key(|&(n, _)| n);
        }
        Self {
            adjacency,
            self_weight: vec![0.0; graph.ids.len()],
            degree: graph.degree.clone(),
        }
    }

    fn len(&self) -> usize {
        self.degree.len()
    }

    /// Collapse every group of `group_of` into one node. Group `g`'s new
    /// index is its rank by first (smallest) member, which keeps the
    /// node order canonical.
    fn aggregate(&self, group_of: &[usize]) -> (Self, Vec<usize>) {
        let mut renumber: BTreeMap<usize, usize> = BTreeMap::new();
        let new_of: Vec<usize> = group_of
            .iter()
            .map(|g| {
                let next = renumber.len();
                *renumber.entry(*g).or_insert(next)
            })
            .collect();
        let size = renumber.len();
        let mut self_weight = vec![0.0; size];
        let mut degree = vec![0.0; size];
        let mut between: BTreeMap<(usize, usize), f64> = BTreeMap::new();
        for v in 0..self.len() {
            let gv = new_of[v];
            self_weight[gv] += self.self_weight[v];
            degree[gv] += self.degree[v];
            for &(u, w) in &self.adjacency[v] {
                if u <= v {
                    continue;
                }
                let gu = new_of[u];
                if gu == gv {
                    self_weight[gv] += w;
                } else {
                    *between.entry(ordered(gu, gv)).or_default() += w;
                }
            }
        }
        let mut adjacency = vec![Vec::new(); size];
        for (&(a, b), &w) in &between {
            adjacency[a].push((b, w));
            adjacency[b].push((a, w));
        }
        for row in &mut adjacency {
            row.sort_by_key(|&(n, _)| n);
        }
        (
            Self {
                adjacency,
                self_weight,
                degree,
            },
            new_of,
        )
    }
}

/// Deterministic Leiden over `graph`, returning a community label per
/// original node (not yet canonical; [`Partition::from_labels`] makes it
/// so).
///
/// Each level runs fast local moving, refines the result, and aggregates
/// by the refined partition with the unrefined one as the next level's
/// starting point. A level that cannot shrink the graph ends the search.
/// Each level either shrinks the node count or stops, so `n` levels bound
/// the loop.
fn leiden(graph: &Graph, gamma: f64) -> Vec<usize> {
    let n = graph.ids.len();
    let m = graph.total_weight;
    if n == 0 || m <= 0.0 {
        return (0..n).collect();
    }
    let ctx = LeidenContext {
        gamma,
        two_m: 2.0 * m,
        epsilon: MERGE_EPSILON * m,
    };
    let mut level = LevelGraph::from_graph(graph);
    // Level node each original node currently sits in.
    let mut node_of: Vec<usize> = (0..n).collect();
    let mut community: Vec<usize> = (0..n).collect();
    for _ in 0..n {
        ctx.move_nodes(&level, &mut community);
        let distinct: BTreeSet<usize> = community.iter().copied().collect();
        if distinct.len() == level.len() {
            break;
        }
        let refined = ctx.refine(&level, &community);
        let refined_count = refined.iter().collect::<BTreeSet<_>>().len();
        // Refinement that splits nothing would aggregate to the same
        // graph; fall back to the unrefined partition (a Louvain step),
        // which is guaranteed to shrink it.
        let by = if refined_count == level.len() {
            &community
        } else {
            &refined
        };
        let (next, new_of) = level.aggregate(by);
        // Carry the unrefined partition up as the next level's start,
        // relabelled into `0..next.len()` so labels stay valid indices.
        let mut label_of: BTreeMap<usize, usize> = BTreeMap::new();
        let mut next_community = vec![0; next.len()];
        for (v, &agg) in new_of.iter().enumerate() {
            let fresh = label_of.len();
            next_community[agg] = *label_of.entry(community[v]).or_insert(fresh);
        }
        for slot in &mut node_of {
            *slot = new_of[*slot];
        }
        level = next;
        community = next_community;
    }
    node_of.iter().map(|&v| community[v]).collect()
}

/// The constants every Leiden step reads.
struct LeidenContext {
    gamma: f64,
    two_m: f64,
    epsilon: f64,
}

impl LeidenContext {
    /// Gain, in edge-weight units, of putting a node of degree `k` that
    /// sends `w` to a community of total degree `total` into it — up to
    /// the `1/m` factor every candidate shares.
    fn gain(&self, w: f64, k: f64, total: f64) -> f64 {
        w - self.gamma * k * total / self.two_m
    }

    /// Fast local moving: visit nodes from a queue seeded in index order,
    /// move each to the neighbouring community with the best gain, and
    /// re-queue the neighbours a move could have changed the answer for.
    fn move_nodes(&self, level: &LevelGraph, community: &mut [usize]) {
        let n = level.len();
        let mut total = vec![0.0; n];
        let mut size = vec![0usize; n];
        for (v, &c) in community.iter().enumerate() {
            total[c] += level.degree[v];
            size[c] += 1;
        }
        let mut empty: BTreeSet<usize> = (0..n).filter(|&c| size[c] == 0).collect();
        let mut queue: VecDeque<usize> = (0..n).collect();
        let mut queued = vec![true; n];
        while let Some(v) = queue.pop_front() {
            queued[v] = false;
            let k = level.degree[v];
            let current = community[v];
            let to = weights_to_communities(level, v, community);
            let mut best = current;
            let mut best_gain = self.gain(
                to.get(&current).copied().unwrap_or(0.0),
                k,
                total[current] - k,
            );
            for (&c, &w) in &to {
                if c == current {
                    continue;
                }
                let gain = self.gain(w, k, total[c]);
                if gain > best_gain + self.epsilon {
                    best = c;
                    best_gain = gain;
                }
            }
            // An empty community gains nothing and costs nothing.
            if size[current] > 1
                && 0.0 > best_gain + self.epsilon
                && let Some(&fresh) = empty.first()
            {
                best = fresh;
            }
            if best == current {
                continue;
            }
            total[current] -= k;
            size[current] -= 1;
            if size[current] == 0 {
                empty.insert(current);
            }
            total[best] += k;
            size[best] += 1;
            empty.remove(&best);
            community[v] = best;
            for &(u, _) in &level.adjacency[v] {
                if community[u] != best && !queued[u] {
                    queued[u] = true;
                    queue.push_back(u);
                }
            }
        }
    }

    /// Refine each community into well-connected sub-communities.
    ///
    /// Every node starts alone. Inside each community `S`, a node that is
    /// still alone and well connected to `S` joins the well-connected
    /// sub-community of `S` it gains the most by joining, if any gain is
    /// positive. This is Leiden's refinement at `θ → 0`: the randomised
    /// choice collapses onto its best candidate.
    fn refine(&self, level: &LevelGraph, community: &[usize]) -> Vec<usize> {
        let n = level.len();
        let mut refined: Vec<usize> = (0..n).collect();
        let mut alone = vec![true; n];
        let mut sub_total: Vec<f64> = level.degree.clone();
        // Edge weight from each sub-community to the rest of its `S`.
        let mut sub_external = vec![0.0; n];
        let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (v, &c) in community.iter().enumerate() {
            members.entry(c).or_default().push(v);
        }
        let mut community_total: BTreeMap<usize, f64> = BTreeMap::new();
        for (v, &c) in community.iter().enumerate() {
            *community_total.entry(c).or_default() += level.degree[v];
            sub_external[v] = level.adjacency[v]
                .iter()
                .filter(|&&(u, _)| community[u] == c)
                .map(|&(_, w)| w)
                .sum();
        }
        for (c, nodes) in &members {
            let s_total = community_total[c];
            for &v in nodes {
                if !alone[v] {
                    continue;
                }
                let k = level.degree[v];
                let to_rest = sub_external[v];
                if to_rest < self.gamma * k * (s_total - k) / self.two_m {
                    continue;
                }
                let mut to: BTreeMap<usize, f64> = BTreeMap::new();
                for &(u, w) in &level.adjacency[v] {
                    if community[u] == *c {
                        *to.entry(refined[u]).or_default() += w;
                    }
                }
                let mut best: Option<(usize, f64)> = None;
                for (&r, &w) in &to {
                    let well_connected = sub_external[r]
                        >= self.gamma * sub_total[r] * (s_total - sub_total[r]) / self.two_m;
                    if !well_connected {
                        continue;
                    }
                    let gain = self.gain(w, k, sub_total[r]);
                    if gain > self.epsilon && best.is_none_or(|(_, top)| gain > top + self.epsilon)
                    {
                        best = Some((r, gain));
                    }
                }
                let Some((target, _)) = best else {
                    continue;
                };
                let w_into = to[&target];
                refined[v] = target;
                alone[v] = false;
                alone[target] = false;
                sub_total[target] += k;
                sub_external[target] += to_rest - 2.0 * w_into;
            }
        }
        refined
    }
}

/// Edge weight from `v` to each community among its neighbours,
/// ascending by community label.
fn weights_to_communities(
    level: &LevelGraph,
    v: usize,
    community: &[usize],
) -> BTreeMap<usize, f64> {
    let mut to: BTreeMap<usize, f64> = BTreeMap::new();
    for &(u, w) in &level.adjacency[v] {
        *to.entry(community[u]).or_default() += w;
    }
    to
}

fn ordered(a: usize, b: usize) -> (usize, usize) {
    (a.min(b), a.max(b))
}

/// Whether an edge weight says anything: positive and finite.
fn informative(weight: f64) -> bool {
    weight.is_finite() && weight > 0.0
}

/// Whether a candidate merge is worth taking.
///
/// Read through a named predicate rather than inline so the boundary is
/// something a test can hand a value: a gain *of exactly*
/// [`MERGE_EPSILON`] is noise and must not drive a merge, and inside the
/// loop that case is unreachable from any graph a test could build.
fn improves(gain: f64) -> bool {
    gain > MERGE_EPSILON
}

/// Canonicalise the node population: sorted ids, the sorted table of
/// declared group names, and each node's index into that table.
///
/// Sorting here is what makes the whole pipeline permutation-invariant —
/// every later step keys on a node index, and those indices are now a
/// function of the id *set* rather than of the order it arrived in. An id
/// listed twice keeps the lexicographically smaller declared group for
/// the same reason: "first wins" would depend on order.
fn node_table(nodes: &[CommunityNode]) -> (Vec<String>, Vec<String>, Vec<usize>) {
    let mut declared: BTreeMap<&str, &str> = BTreeMap::new();
    for node in nodes {
        let entry = declared.entry(&node.id).or_insert(&node.declared);
        *entry = (*entry).min(node.declared.as_str());
    }
    let group_names: Vec<String> = declared
        .values()
        .map(|g| (*g).to_owned())
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    let declared_of = declared
        .values()
        .map(|g| {
            group_names
                .binary_search_by(|probe| probe.as_str().cmp(g))
                .unwrap_or(0)
        })
        .collect();
    let ids = declared.into_keys().map(str::to_owned).collect();
    (ids, group_names, declared_of)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::collection::vec;
    use proptest::prelude::*;
    use rstest::rstest;

    fn greedy(min_community: usize) -> CommunityParams {
        CommunityParams {
            min_community,
            ..CommunityParams::default()
        }
    }

    fn leiden_params(min_community: usize) -> CommunityParams {
        CommunityParams {
            algorithm: CommunityAlgorithm::Leiden,
            min_community,
            ..CommunityParams::default()
        }
    }

    /// Two triangles joined by a single edge: the textbook case where
    /// the split is unambiguous.
    fn barbell() -> (Vec<CommunityNode>, Vec<CommunityEdge>) {
        let nodes = ["a1", "a2", "a3"]
            .into_iter()
            .map(|id| CommunityNode::new(id, "a"))
            .chain(
                ["b1", "b2", "b3"]
                    .into_iter()
                    .map(|id| CommunityNode::new(id, "b")),
            )
            .collect();
        let edges = [
            ("a1", "a2"),
            ("a2", "a3"),
            ("a1", "a3"),
            ("b1", "b2"),
            ("b2", "b3"),
            ("b1", "b3"),
            ("a3", "b1"),
        ]
        .into_iter()
        .map(|(a, b)| CommunityEdge::new(a, b, 1))
        .collect();
        (nodes, edges)
    }

    #[test]
    fn barbell_splits_into_its_two_triangles() {
        let (nodes, edges) = barbell();
        let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
        assert_eq!(report.community_count, 2, "got {report:?}");
        assert_eq!(report.communities.len(), 2);
        assert_eq!(report.communities[0].members, ["a1", "a2", "a3"]);
        assert_eq!(report.communities[1].members, ["b1", "b2", "b3"]);
        assert!(report.detected_modularity > 0.3, "got {report:?}");
    }

    /// The declared partition *is* the detected one here, so the gap is
    /// zero and the quality ratio is exactly 1 — the "architecture
    /// matches reality" reading.
    #[test]
    fn a_declared_partition_that_matches_scores_the_same_q() {
        let (nodes, edges) = barbell();
        let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
        assert_eq!(report.modularity_gap, 0.0, "got {report:?}");
        assert_eq!(report.declared_quality, Some(1.0));
        assert!(report.misfiled.is_empty(), "got {report:?}");
        assert!(report.spanning.is_empty(), "got {report:?}");
    }

    /// A planted misfile: `a3` is filed under `a` but every edge it has
    /// runs into `b`.
    #[test]
    fn a_member_wired_into_another_group_is_reported_with_its_edge_counts() {
        let mut nodes: Vec<CommunityNode> = ["a1", "a2"]
            .into_iter()
            .map(|id| CommunityNode::new(id, "a"))
            .chain(
                ["b1", "b2", "b3"]
                    .into_iter()
                    .map(|id| CommunityNode::new(id, "b")),
            )
            .collect();
        nodes.push(CommunityNode::new("a3", "a"));
        let edges: Vec<CommunityEdge> = [
            ("a1", "a2"),
            ("b1", "b2"),
            ("b2", "b3"),
            ("b1", "b3"),
            ("a3", "b1"),
            ("a3", "b2"),
            ("a3", "b3"),
        ]
        .into_iter()
        .map(|(a, b)| CommunityEdge::new(a, b, 1))
        .collect();

        let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
        let row = report
            .misfiled
            .iter()
            .find(|m| m.node == "a3")
            .unwrap_or_else(|| panic!("no a3 row in {report:?}"));
        assert_eq!(row.declared, "a");
        assert_eq!(row.suggested, "b");
        assert_eq!(row.weight_to_declared, 0.0);
        assert_eq!(row.weight_to_suggested, 3.0);
        assert_eq!(row.evidence, 3.0);
        assert!(report.modularity_gap > 0.0, "got {report:?}");
    }

    /// Sitting in a community someone else dominates is not enough: the
    /// node must have more edge weight there than at home, or the row is
    /// a coincidence rather than a move candidate.
    #[test]
    fn a_member_still_wired_to_its_own_group_is_not_reported() {
        let nodes = vec![
            CommunityNode::new("a1", "a"),
            CommunityNode::new("b1", "b"),
            CommunityNode::new("b2", "b"),
        ];
        let edges = vec![
            CommunityEdge::new("a1", "b1", 1),
            CommunityEdge::new("b1", "b2", 5),
        ];
        let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
        assert!(
            report.misfiled.iter().all(|m| m.node != "b1"),
            "b1 is wired to its own group: {report:?}",
        );
    }

    /// `Q` is the number every other figure in the report is read
    /// against, so the arithmetic is pinned to a hand-computed value
    /// rather than to an inequality. The barbell carries 7 units of
    /// weight; each triangle holds 3 of them internally and its members'
    /// degrees sum to 7, so each contributes `3/7 - (7/14)^2`.
    #[test]
    fn modularity_matches_the_hand_computed_newman_score() {
        let (nodes, edges) = barbell();
        let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
        let m = 7.0_f64;
        let per_triangle = 3.0 / m - (7.0 / (2.0 * m)).powi(2);
        let expected = 2.0 * per_triangle;
        assert!(
            (report.detected_modularity - expected).abs() < 1e-12,
            "detected {} != {expected}",
            report.detected_modularity,
        );
        // The declared partition is the same one here, so it must score
        // identically through the same function.
        assert!(
            (report.declared_modularity - expected).abs() < 1e-12,
            "declared {} != {expected}",
            report.declared_modularity,
        );
    }

    /// The declared breakdown is the evidence behind `dominant_declared`,
    /// so it has to carry the counts rather than just the winner.
    #[test]
    fn a_community_reports_which_declared_groups_it_is_made_of() {
        let nodes = vec![
            CommunityNode::new("x1", "one"),
            CommunityNode::new("x2", "one"),
            CommunityNode::new("y1", "two"),
        ];
        let edges = vec![
            CommunityEdge::new("x1", "x2", 4),
            CommunityEdge::new("x2", "y1", 4),
            CommunityEdge::new("x1", "y1", 4),
        ];
        let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
        assert_eq!(
            report.communities[0].breakdown,
            vec![
                DeclaredShare {
                    declared: "one".to_owned(),
                    members: 2
                },
                DeclaredShare {
                    declared: "two".to_owned(),
                    members: 1
                },
            ],
            "got {report:?}",
        );
        assert_eq!(report.communities[0].dominant_declared, "one");
    }

    /// Spanning is "no declared group owns a majority of this cluster".
    /// A cluster one group holds two of three members of is owned, and
    /// reporting it would turn every ordinary module that reaches into a
    /// neighbour into a finding.
    #[test]
    fn a_cluster_one_group_holds_a_majority_of_is_not_spanning() {
        let nodes = vec![
            CommunityNode::new("x1", "one"),
            CommunityNode::new("x2", "one"),
            CommunityNode::new("y1", "two"),
        ];
        let edges = vec![
            CommunityEdge::new("x1", "x2", 4),
            CommunityEdge::new("x2", "y1", 4),
            CommunityEdge::new("x1", "y1", 4),
        ];
        let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
        assert_eq!(report.communities.len(), 1, "got {report:?}");
        assert!(report.spanning.is_empty(), "got {report:?}");
    }

    /// The other side of the same boundary: a two-member cluster split
    /// one-and-one is owned by neither group.
    #[test]
    fn a_cluster_split_evenly_between_two_groups_is_spanning() {
        let nodes = vec![
            CommunityNode::new("x1", "one"),
            CommunityNode::new("y1", "two"),
        ];
        let edges = vec![CommunityEdge::new("x1", "y1", 4)];
        let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
        assert_eq!(report.spanning.len(), 1, "got {report:?}");
        assert_eq!(report.spanning[0].size, 2);
        assert_eq!(report.spanning[0].declared_group_count, 2);
    }

    /// The misfiled gate is strict: equal weight either way is not
    /// evidence for a move, so the tie stays home.
    ///
    /// `a1` sits in a community `b` dominates and carries 3 units to
    /// each side, so only the strictness of the comparison keeps it off
    /// the listing.
    #[test]
    fn a_member_pulled_equally_both_ways_is_not_reported() {
        let nodes = vec![
            CommunityNode::new("a1", "a"),
            CommunityNode::new("a2", "a"),
            CommunityNode::new("b1", "b"),
            CommunityNode::new("b2", "b"),
            CommunityNode::new("b3", "b"),
        ];
        let edges = vec![
            CommunityEdge::new("a1", "a2", 3),
            CommunityEdge::new("a1", "b1", 3),
            CommunityEdge::new("a2", "b2", 5),
            CommunityEdge::new("b1", "b2", 3),
            CommunityEdge::new("b2", "b3", 3),
            CommunityEdge::new("b1", "b3", 3),
        ];
        let report = detect_communities(&nodes, &edges, greedy(1));
        let community = report
            .communities
            .iter()
            .find(|c| c.members.contains(&"a1".to_owned()))
            .unwrap_or_else(|| panic!("no a1 community in {report:?}"));
        assert_eq!(community.dominant_declared, "b", "got {report:?}");
        assert!(
            report.misfiled.iter().all(|m| m.node != "a1"),
            "a tie is not evidence: {report:?}",
        );
    }

    /// `evidence` is the *difference* between the two weights and the
    /// ranking key, so it is pinned on a member that keeps real weight
    /// at home — where a sum and a difference disagree.
    #[test]
    fn evidence_is_the_gap_between_the_two_weights() {
        // `z` sorts last, so it is the higher-indexed endpoint of every
        // edge it has: both halves of the per-node weight tally have to
        // accumulate for its counts to come out right.
        let nodes = vec![
            CommunityNode::new("a1", "a"),
            CommunityNode::new("b1", "b"),
            CommunityNode::new("b2", "b"),
            CommunityNode::new("b3", "b"),
            CommunityNode::new("z", "a"),
        ];
        let edges = vec![
            CommunityEdge::new("a1", "z", 2),
            CommunityEdge::new("z", "b1", 6),
            CommunityEdge::new("z", "b2", 6),
            CommunityEdge::new("b1", "b2", 4),
            CommunityEdge::new("b2", "b3", 4),
            CommunityEdge::new("b1", "b3", 4),
        ];
        let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
        let row = report
            .misfiled
            .iter()
            .find(|m| m.node == "z")
            .unwrap_or_else(|| panic!("no z row in {report:?}"));
        assert_eq!(row.weight_to_suggested, 12.0, "got {report:?}");
        assert_eq!(row.weight_to_declared, 2.0, "got {report:?}");
        assert_eq!(row.evidence, 10.0, "got {report:?}");
    }

    /// An id listed twice keeps the smaller declared group. "First wins"
    /// would make the answer depend on arrival order, which is exactly
    /// what the rest of this module exists to rule out.
    #[test]
    fn a_repeated_node_keeps_the_smaller_declared_group() {
        let edges = vec![CommunityEdge::new("n", "other", 1)];
        let later_wins = vec![
            CommunityNode::new("n", "zeta"),
            CommunityNode::new("n", "alpha"),
            CommunityNode::new("other", "alpha"),
        ];
        let earlier_wins = vec![
            CommunityNode::new("n", "alpha"),
            CommunityNode::new("n", "zeta"),
            CommunityNode::new("other", "alpha"),
        ];
        for nodes in [later_wins, earlier_wins] {
            let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
            assert_eq!(report.declared_group_count, 1, "got {report:?}");
            assert_eq!(
                report.communities[0].dominant_declared, "alpha",
                "got {report:?}",
            );
        }
    }

    /// A gain of exactly the tolerance is noise, not an improvement.
    /// Inside the loop this boundary is unreachable from any graph a
    /// test could build, which is why the predicate is named.
    #[rstest]
    #[case(MERGE_EPSILON, false)]
    #[case(0.0, false)]
    #[case(-1.0, false)]
    #[case(MERGE_EPSILON * 2.0, true)]
    #[case(0.25, true)]
    fn only_a_gain_above_the_tolerance_drives_a_merge(#[case] gain: f64, #[case] expected: bool) {
        assert_eq!(improves(gain), expected);
    }

    /// The greedy pass must take the *best* merge available, not the
    /// first one it is offered. `{a1,a2}` is the lexicographically first
    /// candidate pair and the worst merge on this graph; the heavy
    /// `{b1,b2}` edge is the one that belongs in a community.
    #[test]
    fn the_best_merge_wins_over_the_first_candidate() {
        let nodes = ["a1", "a2", "b1", "b2"]
            .into_iter()
            .map(|id| CommunityNode::new(id, "g"))
            .collect::<Vec<_>>();
        let edges = vec![
            CommunityEdge::new("a1", "a2", 1),
            CommunityEdge::new("a2", "b1", 1),
            CommunityEdge::new("b1", "b2", 40),
        ];
        let report = detect_communities(&nodes, &edges, greedy(1));
        let holding_b = report
            .communities
            .iter()
            .find(|c| c.members.contains(&"b1".to_owned()))
            .unwrap_or_else(|| panic!("no b1 community in {report:?}"));
        assert_eq!(holding_b.members, ["b1", "b2"], "got {report:?}");
    }

    /// Ties are resolved to the *first* candidate pair, not the last.
    /// On a ring of unit edges every merge is worth the same at every
    /// step, so which end of the tie the scan keeps decides the whole
    /// partition — the case that separates a total rule from a merely
    /// deterministic one.
    #[test]
    fn a_tied_merge_resolves_to_the_first_candidate_pair() {
        let nodes: Vec<CommunityNode> = (0..5)
            .map(|i| CommunityNode::new(format!("n{i}"), "g"))
            .collect();
        let edges: Vec<CommunityEdge> = (0..5)
            .map(|i| CommunityEdge::new(format!("n{i}"), format!("n{}", (i + 1) % 5), 1))
            .collect();
        let report = detect_communities(&nodes, &edges, greedy(1));
        let partition: Vec<&[String]> = report
            .communities
            .iter()
            .map(|c| c.members.as_slice())
            .collect();
        assert_eq!(
            partition,
            vec![
                ["n0".to_owned(), "n1".to_owned(), "n4".to_owned()].as_slice(),
                ["n2".to_owned(), "n3".to_owned()].as_slice(),
            ],
            "got {report:?}",
        );
    }

    /// A parent that is both a node and the group its children are
    /// filed under elects itself as its community's dominant group. That
    /// is not a move candidate — "put `a` inside `a`" is not an action —
    /// so the row must not appear.
    #[test]
    fn a_member_is_never_reported_as_misfiled_into_itself() {
        let nodes = vec![
            CommunityNode::new("a", "root"),
            CommunityNode::new("a1", "a"),
            CommunityNode::new("a2", "a"),
        ];
        let edges = vec![
            CommunityEdge::new("a", "a1", 5),
            CommunityEdge::new("a", "a2", 5),
            CommunityEdge::new("a1", "a2", 5),
        ];
        let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
        assert!(
            report.misfiled.iter().all(|m| m.suggested != m.node),
            "got {report:?}",
        );
    }

    /// Three declared groups contributing one member each: nobody owns
    /// the cluster, which is the spanning-community shape.
    #[test]
    fn a_cluster_no_declared_group_owns_is_reported_as_spanning() {
        let nodes = vec![
            CommunityNode::new("x", "one"),
            CommunityNode::new("y", "two"),
            CommunityNode::new("z", "three"),
        ];
        let edges = vec![
            CommunityEdge::new("x", "y", 4),
            CommunityEdge::new("y", "z", 4),
            CommunityEdge::new("x", "z", 4),
        ];
        let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
        assert_eq!(report.spanning.len(), 1, "got {report:?}");
        assert_eq!(report.spanning[0].declared_group_count, 3);
        assert_eq!(report.spanning[0].size, 3);
    }

    #[test]
    fn an_edgeless_graph_reports_no_structure_rather_than_guessing() {
        let nodes = vec![CommunityNode::new("a", "a"), CommunityNode::new("b", "b")];
        let report = detect_communities(&nodes, &[], greedy(DEFAULT_MIN_COMMUNITY));
        assert_eq!(report.detected_modularity, 0.0);
        assert_eq!(report.declared_modularity, 0.0);
        assert_eq!(report.declared_quality, None);
        assert_eq!(report.isolated_node_count, 2);
        assert_eq!(report.community_count, 2);
        assert!(report.communities.is_empty(), "got {report:?}");
    }

    /// Self-loops, unknown endpoints, and zero weights carry no
    /// information about who belongs with whom, and counting them would
    /// inflate `edge_count` and `total_weight` for nothing.
    #[rstest]
    #[case::self_loop(CommunityEdge::new("a", "a", 3))]
    #[case::unknown_endpoint(CommunityEdge::new("a", "ghost", 3))]
    #[case::zero_weight(CommunityEdge::new("a", "b", 0))]
    fn uninformative_edges_are_dropped(#[case] edge: CommunityEdge) {
        let nodes = vec![CommunityNode::new("a", "g"), CommunityNode::new("b", "g")];
        let report = detect_communities(&nodes, std::slice::from_ref(&edge), greedy(1));
        assert_eq!(report.edge_count, 0, "{edge:?} should not become an edge");
        assert_eq!(report.total_weight, 0.0);
    }

    /// Parallel edges between the same pair are one weighted edge, not
    /// two: `edge_count` counts pairs and the weights add up.
    #[test]
    fn repeated_pairs_accumulate_into_one_weighted_edge() {
        let nodes = vec![CommunityNode::new("a", "g"), CommunityNode::new("b", "g")];
        let edges = vec![
            CommunityEdge::new("a", "b", 2),
            CommunityEdge::new("b", "a", 3),
        ];
        let report = detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY));
        assert_eq!(report.edge_count, 1);
        assert_eq!(report.total_weight, 5.0);
    }

    /// `min_community` bounds the listings only. The partition, and
    /// therefore both modularity figures, must be the same either way —
    /// otherwise a display cap would silently change the headline
    /// numbers.
    #[test]
    fn min_community_bounds_the_listings_not_the_partition() {
        let (nodes, edges) = barbell();
        let wide = detect_communities(&nodes, &edges, greedy(1));
        let narrow = detect_communities(&nodes, &edges, greedy(4));
        assert_eq!(wide.community_count, narrow.community_count);
        assert_eq!(wide.detected_modularity, narrow.detected_modularity);
        assert_eq!(wide.communities.len(), 2);
        assert!(narrow.communities.is_empty(), "got {narrow:?}");
    }

    /// Internal and external weight must account for every edge: each
    /// internal edge once for its community, each crossing edge once for
    /// each endpoint's community.
    #[test]
    fn community_weights_account_for_every_edge() {
        let (nodes, edges) = barbell();
        let report = detect_communities(&nodes, &edges, greedy(1));
        let internal: f64 = report.communities.iter().map(|c| c.internal_weight).sum();
        let external: f64 = report.communities.iter().map(|c| c.external_weight).sum();
        assert_eq!(internal, 6.0);
        assert_eq!(external, 2.0);
        assert_eq!(internal + external / 2.0, report.total_weight);
    }

    /// The determinism requirement, stated as a test: the same node and
    /// edge *sets* in any order produce byte-identical output. A rotation
    /// is enough to break an implementation that keys anything on arrival
    /// order.
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(5)]
    fn output_is_invariant_under_input_rotation(#[case] shift: usize) {
        let (nodes, edges) = barbell();
        let mut rotated_nodes = nodes.clone();
        rotated_nodes.rotate_left(shift % nodes.len());
        let mut rotated_edges = edges.clone();
        rotated_edges.rotate_left(shift % edges.len());

        assert_eq!(
            detect_communities(&nodes, &edges, greedy(DEFAULT_MIN_COMMUNITY)),
            detect_communities(
                &rotated_nodes,
                &rotated_edges,
                greedy(DEFAULT_MIN_COMMUNITY)
            ),
        );
    }

    /// Apply `permutation` to `items` by sorting on it, so any generated
    /// key vector produces a valid reordering without needing a shuffle
    /// RNG (which the domain crate deliberately has no access to).
    fn permute<T: Clone>(items: &[T], permutation: &[u16]) -> Vec<T> {
        let mut keyed: Vec<(u16, usize, T)> = items
            .iter()
            .enumerate()
            .map(|(i, item)| (permutation.get(i).copied().unwrap_or(0), i, item.clone()))
            .collect();
        keyed.sort_by_key(|&(key, i, _)| (key, i));
        keyed.into_iter().map(|(_, _, item)| item).collect()
    }

    proptest! {
        /// The hard requirement from the design: an arbitrary graph,
        /// with its node and edge lists independently permuted, must
        /// produce an identical report. Rotation covers cyclic shifts;
        /// this covers the rest of the symmetric group, including the
        /// orderings that would let a greedy pass see a different first
        /// candidate merge.
        #[test]
        fn output_is_invariant_under_input_permutation(
            group_of in vec(0_usize..4, 2..14),
            raw_edges in vec((0_usize..14, 0_usize..14, 1_u32..5), 0..40),
            leiden in any::<bool>(),
            resolution in prop_oneof![Just(1.0), Just(0.5), Just(2.0)],
            node_permutation in vec(any::<u16>(), 0..14),
            edge_permutation in vec(any::<u16>(), 0..40),
        ) {
            let nodes: Vec<CommunityNode> = group_of
                .iter()
                .enumerate()
                .map(|(i, group)| CommunityNode::new(format!("n{i:02}"), format!("g{group}")))
                .collect();
            let edges: Vec<CommunityEdge> = raw_edges
                .iter()
                .map(|&(a, b, w)| CommunityEdge::new(format!("n{a:02}"), format!("n{b:02}"), w))
                .collect();
            let params = CommunityParams {
                algorithm: if leiden { CommunityAlgorithm::Leiden } else { CommunityAlgorithm::Greedy },
                resolution,
                min_community: DEFAULT_MIN_COMMUNITY,
            };

            let baseline = detect_communities(&nodes, &edges, params);
            let shuffled = detect_communities(
                &permute(&nodes, &node_permutation),
                &permute(&edges, &edge_permutation),
                params,
            );
            prop_assert_eq!(&baseline, &shuffled);
        }
    }

    /// `k` cliques of `size` nodes joined into a ring by single edges:
    /// the canonical graph where the right answer is one community per
    /// clique.
    fn ring_of_cliques(k: usize, size: usize) -> (Vec<CommunityNode>, Vec<CommunityEdge>) {
        let id = |c: usize, i: usize| format!("c{c:02}n{i:02}");
        let nodes = (0..k)
            .flat_map(|c| (0..size).map(move |i| CommunityNode::new(id(c, i), format!("g{c:02}"))))
            .collect();
        let mut edges = Vec::new();
        for c in 0..k {
            for i in 0..size {
                for j in i + 1..size {
                    edges.push(CommunityEdge::new(id(c, i), id(c, j), 1));
                }
            }
            edges.push(CommunityEdge::new(id(c, size - 1), id((c + 1) % k, 0), 1));
        }
        (nodes, edges)
    }

    #[rstest]
    #[case::greedy(CommunityAlgorithm::Greedy)]
    #[case::leiden(CommunityAlgorithm::Leiden)]
    fn both_algorithms_split_the_barbell(#[case] algorithm: CommunityAlgorithm) {
        let (nodes, edges) = barbell();
        let params = CommunityParams {
            algorithm,
            ..CommunityParams::default()
        };
        let report = detect_communities(&nodes, &edges, params);
        assert_eq!(report.community_count, 2, "got {report:?}");
        assert_eq!(report.communities[0].members, ["a1", "a2", "a3"]);
        assert_eq!(report.communities[1].members, ["b1", "b2", "b3"]);
    }

    /// Leiden recovers every clique of a ring of cliques, which is also
    /// the declared partition here, so the declared grouping scores
    /// exactly what the search found.
    #[test]
    fn leiden_recovers_each_clique_of_a_ring() {
        let (nodes, edges) = ring_of_cliques(8, 5);
        let report = detect_communities(&nodes, &edges, leiden_params(1));
        assert_eq!(report.community_count, 8, "got {report:?}");
        assert!(
            report.communities.iter().all(|c| c.size == 5),
            "got {report:?}"
        );
        assert!(report.modularity_gap.abs() < 1e-12, "got {report:?}");
        assert!(report.misfiled.is_empty(), "got {report:?}");
    }

    /// Leiden never does worse than greedy agglomeration on the
    /// structured graphs it is meant for.
    #[rstest]
    #[case(4, 4)]
    #[case(10, 3)]
    #[case(6, 6)]
    fn leiden_scores_at_least_greedy_on_a_ring_of_cliques(#[case] k: usize, #[case] size: usize) {
        let (nodes, edges) = ring_of_cliques(k, size);
        let greedy_q = detect_communities(&nodes, &edges, greedy(1)).detected_modularity;
        let leiden_q = detect_communities(&nodes, &edges, leiden_params(1)).detected_modularity;
        assert!(
            leiden_q + 1e-12 >= greedy_q,
            "leiden {leiden_q} < greedy {greedy_q}"
        );
    }

    /// A low resolution merges what a high one keeps apart: at `γ` well
    /// below 1 the ring collapses into fewer, larger communities.
    #[rstest]
    #[case::greedy(CommunityAlgorithm::Greedy)]
    #[case::leiden(CommunityAlgorithm::Leiden)]
    fn a_lower_resolution_yields_fewer_communities(#[case] algorithm: CommunityAlgorithm) {
        let (nodes, edges) = ring_of_cliques(8, 4);
        let at = |resolution| {
            detect_communities(
                &nodes,
                &edges,
                CommunityParams {
                    algorithm,
                    resolution,
                    min_community: 1,
                },
            )
            .community_count
        };
        assert!(
            at(0.05) < at(1.0),
            "coarse {} vs default {}",
            at(0.05),
            at(1.0)
        );
    }

    /// `γ` enters the modularity itself, so the declared partition is
    /// scored at the same resolution as the detected one.
    #[test]
    fn resolution_scales_the_null_model_term() {
        let (nodes, edges) = barbell();
        let params = CommunityParams {
            resolution: 2.0,
            ..greedy(1)
        };
        let report = detect_communities(&nodes, &edges, params);
        let m = 7.0_f64;
        let expected = 2.0 * (3.0 / m - 2.0 * (7.0 / (2.0 * m)).powi(2));
        assert!(
            (report.declared_modularity - expected).abs() < 1e-12,
            "declared {} != {expected}",
            report.declared_modularity,
        );
    }

    /// Fractional weights cluster like integer ones: scaling every
    /// weight by a constant leaves the partition alone.
    #[rstest]
    #[case::greedy(CommunityAlgorithm::Greedy)]
    #[case::leiden(CommunityAlgorithm::Leiden)]
    fn scaling_every_weight_leaves_the_partition_alone(#[case] algorithm: CommunityAlgorithm) {
        let (nodes, edges) = ring_of_cliques(5, 4);
        let scaled: Vec<CommunityEdge> = edges
            .iter()
            .map(|e| CommunityEdge::new(e.a.clone(), e.b.clone(), e.weight * 0.013))
            .collect();
        let whole = partition_nodes(&nodes, &edges, algorithm, 1.0);
        let fraction = partition_nodes(&nodes, &scaled, algorithm, 1.0);
        assert_eq!(whole, fraction);
    }

    #[test]
    fn non_finite_and_negative_weights_are_dropped() {
        let nodes = vec![CommunityNode::new("a", "g"), CommunityNode::new("b", "g")];
        for weight in [f64::NAN, f64::INFINITY, -1.0] {
            let edge = CommunityEdge::new("a", "b", weight);
            let report = detect_communities(&nodes, std::slice::from_ref(&edge), greedy(1));
            assert_eq!(report.edge_count, 0, "{edge:?} should not become an edge");
        }
    }

    fn partition_of(groups: &[(&str, usize)], isolated: &[&str]) -> NodePartition {
        NodePartition {
            community_of: groups.iter().map(|&(id, c)| (id.to_owned(), c)).collect(),
            isolated: isolated.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn identical_partitions_agree_completely() {
        let p = partition_of(&[("a", 0), ("b", 0), ("c", 1), ("d", 1)], &[]);
        let agreement = compare_partitions(&p, &p);
        assert_eq!(agreement.compared, 4);
        assert_eq!(agreement.nmi, Some(1.0));
        assert!(agreement.divergent.is_empty(), "got {agreement:?}");
    }

    /// Relabelling communities is not disagreement: NMI is a function of
    /// the grouping, not of the numbers on it.
    #[test]
    fn relabelled_partitions_still_agree() {
        let p = partition_of(&[("a", 0), ("b", 0), ("c", 1), ("d", 1)], &[]);
        let q = partition_of(&[("a", 7), ("b", 7), ("c", 3), ("d", 3)], &[]);
        let nmi = compare_partitions(&p, &q).nmi.unwrap_or_default();
        assert!((nmi - 1.0).abs() < 1e-12, "got {nmi}");
    }

    /// Two crossed halvings carry no information about each other.
    #[test]
    fn independent_partitions_score_zero_and_list_the_divergent_nodes() {
        let p = partition_of(&[("a", 0), ("b", 0), ("c", 1), ("d", 1)], &[]);
        let q = partition_of(&[("a", 0), ("b", 1), ("c", 0), ("d", 1)], &[]);
        let agreement = compare_partitions(&p, &q);
        assert!(
            agreement.nmi.unwrap_or(1.0).abs() < 1e-12,
            "got {agreement:?}"
        );
        assert_eq!(agreement.divergent.len(), 4, "got {agreement:?}");
        assert_eq!(agreement.divergent[0].node, "a");
        assert_eq!(agreement.divergent[0].shared_mates, 0);
        assert_eq!(agreement.divergent[0].first_mates, 1);
        assert_eq!(agreement.divergent[0].second_mates, 1);
    }

    /// A node isolated in either partition is a default singleton, not a
    /// placement, so it is never compared.
    #[test]
    fn isolated_nodes_are_not_compared() {
        let p = partition_of(&[("a", 0), ("b", 0), ("c", 1)], &["c"]);
        let q = partition_of(&[("a", 0), ("b", 0), ("c", 0)], &[]);
        let agreement = compare_partitions(&p, &q);
        assert_eq!(agreement.compared, 2);
        assert!(agreement.divergent.is_empty(), "got {agreement:?}");
    }

    #[test]
    fn a_single_node_comparison_has_no_nmi() {
        let p = partition_of(&[("a", 0)], &[]);
        assert_eq!(compare_partitions(&p, &p).nmi, None);
    }

    /// The divergence cut is strict: a node whose mate sets overlap by
    /// exactly half is not listed.
    #[test]
    fn a_half_overlap_is_not_divergent() {
        // a's mates: {b} vs {b, c} -> 1/2.
        let p = partition_of(&[("a", 0), ("b", 0), ("c", 1)], &[]);
        let q = partition_of(&[("a", 0), ("b", 0), ("c", 0)], &[]);
        let agreement = compare_partitions(&p, &q);
        assert!(
            agreement.divergent.iter().all(|d| d.node != "a"),
            "got {agreement:?}",
        );
        let c = agreement.divergent.iter().find(|d| d.node == "c");
        assert_eq!(c.map(|d| d.jaccard), Some(0.0), "got {agreement:?}");
    }

    proptest! {
        /// Leiden communities are always connected: every member reaches
        /// every other through edges inside the community.
        #[test]
        fn leiden_communities_are_connected(
            raw_edges in vec((0_usize..16, 0_usize..16, 1_u32..6), 0..50),
        ) {
            let nodes: Vec<CommunityNode> = (0..16)
                .map(|i| CommunityNode::new(format!("n{i:02}"), "g"))
                .collect();
            let edges: Vec<CommunityEdge> = raw_edges
                .iter()
                .map(|&(a, b, w)| CommunityEdge::new(format!("n{a:02}"), format!("n{b:02}"), w))
                .collect();
            let partition = partition_nodes(&nodes, &edges, CommunityAlgorithm::Leiden, 1.0);
            let mut groups: BTreeMap<usize, BTreeSet<&str>> = BTreeMap::new();
            for (id, &c) in &partition.community_of {
                groups.entry(c).or_default().insert(id.as_str());
            }
            for members in groups.values() {
                let start = *members.iter().next().unwrap_or(&"");
                let mut seen: BTreeSet<&str> = BTreeSet::from([start]);
                let mut frontier = vec![start];
                while let Some(v) = frontier.pop() {
                    for e in &edges {
                        let other = if e.a == v { e.b.as_str() } else if e.b == v { e.a.as_str() } else { continue };
                        if members.contains(other) && seen.insert(other) {
                            frontier.push(other);
                        }
                    }
                }
                prop_assert_eq!(seen.len(), members.len(), "disconnected community {:?}", members);
            }
        }
    }
}
