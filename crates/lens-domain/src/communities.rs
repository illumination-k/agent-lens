//! Community detection over a module dependency graph, compared against
//! the grouping the repository *declares*.
//!
//! `coupling` and `layers` answer whether the dependency **direction** is
//! sane. This answers the orthogonal question: is the **grouping** sane?
//! Every repository declares a partition of its files — the directory or
//! module each one is filed under — and the dependency graph forms a
//! partition of its own. Where the two disagree, one of them is wrong.
//!
//! Two numbers carry the report:
//!
//! * **`detected_modularity`** — Newman's `Q` for the partition this
//!   module finds, i.e. the best grouping the edges support.
//! * **`declared_modularity`** — the same `Q` computed for the declared
//!   partition, with no re-grouping at all.
//!
//! A declared score close to the detected one means the architecture
//! matches reality. The gap between them is what the misfiled rows are
//! made of.
//!
//! # Determinism
//!
//! Community detection is usually order-dependent: Louvain visits nodes
//! in input order and breaks ties at random, so the same graph gives
//! different answers on different runs. An analyzer that does that is
//! useless — a report an agent cannot diff against the last one is not
//! evidence.
//!
//! So the partition here is greedy modularity agglomeration (Clauset-
//! Newman-Moore) made total:
//!
//! * nodes are sorted by id before anything else, so the node indices
//!   every later step keys on are a function of the *set* of nodes, not
//!   of the order they arrived in;
//! * a community is labelled by its smallest member index, so labels are
//!   canonical rather than allocation-ordered;
//! * candidate merges are scanned in ascending `(label, label)` order and
//!   the best `ΔQ` is taken with a strict `>`, so an exact tie resolves to
//!   the lexicographically smallest pair rather than to whichever the
//!   iterator happened to yield first.
//!
//! The result is invariant under permutation of the input node and edge
//! lists, which [`detect_communities`] is property-tested for.
//!
//! # Limits
//!
//! Modularity has a **resolution limit**: below a size that depends on
//! the total edge weight, a genuine small cluster scores better merged
//! into a neighbour than kept apart, and no amount of tuning inside this
//! function can see it. That is why every community reports its size —
//! a reader who knows the codebase can tell when a cluster got absorbed.
//! There is deliberately no resolution parameter: it is not calibratable
//! against anything an agent could check.
//!
//! On a small or densely-connected graph nearly everything lands in one
//! community. That is a real answer about the graph, and the report says
//! so rather than splitting the noise into plausible-looking clusters.

use std::collections::{BTreeMap, BTreeSet};

/// Smallest community reported by default. A singleton community is a
/// node the edges gave no home to, which is a fact about the node rather
/// than a cluster worth naming.
pub const DEFAULT_MIN_COMMUNITY: usize = 2;

/// A merge is taken only when it strictly improves `Q`. The tolerance
/// keeps floating-point noise around zero from driving a merge that the
/// exact arithmetic would decline, which on a symmetric graph is where
/// the last shred of order-dependence would otherwise hide.
const MERGE_EPSILON: f64 = 1e-12;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunityEdge {
    pub a: String,
    pub b: String,
    pub weight: u64,
}

impl CommunityEdge {
    pub fn new(a: impl Into<String>, b: impl Into<String>, weight: u64) -> Self {
        Self {
            a: a.into(),
            b: b.into(),
            weight,
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
    pub internal_weight: u64,
    /// Edge weight leaving the community.
    pub external_weight: u64,
}

/// A node whose community is dominated by a declared group other than
/// its own, with the in/out edge counts that argue for the move.
///
/// This is the actionable row: not "these files are related" but "this
/// file is filed here and points there".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MisfiledMember {
    pub node: String,
    /// The group the repository files it under.
    pub declared: String,
    /// The dominant declared group of the community it clustered into.
    pub suggested: String,
    pub community: usize,
    /// Edge weight to nodes in its own declared group.
    pub weight_to_declared: u64,
    /// Edge weight to nodes in the suggested group.
    pub weight_to_suggested: u64,
    /// `weight_to_suggested - weight_to_declared`: how lopsided the
    /// evidence is, and the ranking key. Size of the community is
    /// deliberately not part of it.
    pub evidence: u64,
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
    /// Distinct undirected node pairs carrying at least one reference.
    pub edge_count: usize,
    pub total_weight: u64,
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
    /// exactly the ones the dependencies form.
    pub modularity_gap: f64,
    /// `declared / detected`, when the detected partition scored above
    /// zero. `1.0` means the declared architecture is as good a
    /// partition as any this graph supports; `None` means the graph has
    /// no community structure to compare against.
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
/// `min_community` bounds which communities are reported; it does not
/// change the partition, so the modularity figures and
/// `community_count` describe the whole graph either way. Nodes named
/// only by an edge are ignored: the node list is the population.
pub fn detect_communities(
    nodes: &[CommunityNode],
    edges: &[CommunityEdge],
    min_community: usize,
) -> CommunityReport {
    let graph = Graph::build(nodes, edges);
    let partition = graph.agglomerate();
    let detected = graph.modularity(&partition.group_of, partition.count);
    let declared = graph.modularity(&graph.declared_of, graph.group_names.len());
    let facts = graph.community_facts(&partition);
    let report_ready = facts.iter().filter(|f| f.members.len() >= min_community);

    CommunityReport {
        node_count: graph.ids.len(),
        edge_count: graph.pairs.len(),
        total_weight: graph.total_weight,
        isolated_node_count: graph.degree.iter().filter(|d| **d == 0).count(),
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

/// The canonical form every later step reads: nodes in sorted-id order,
/// declared groups as indices into a sorted name table, and one weight
/// per unordered node pair.
struct Graph {
    ids: Vec<String>,
    group_names: Vec<String>,
    /// Declared group index per node, parallel to `ids`.
    declared_of: Vec<usize>,
    /// `(i, j) -> weight` with `i < j`.
    pairs: BTreeMap<(usize, usize), u64>,
    /// Weighted degree per node.
    degree: Vec<u64>,
    /// Sum of all pair weights, each pair counted once.
    total_weight: u64,
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
        let mut pairs: BTreeMap<(usize, usize), u64> = BTreeMap::new();
        let mut degree = vec![0u64; ids.len()];
        for edge in edges {
            // Self-loops, endpoints outside the node list, and
            // zero-weight edges say nothing about who belongs with whom,
            // and counting them would inflate `edge_count` for nothing.
            let (Some(&a), Some(&b)) = (index.get(edge.a.as_str()), index.get(edge.b.as_str()))
            else {
                continue;
            };
            if a == b || edge.weight == 0 {
                continue;
            }
            *pairs.entry(ordered(a, b)).or_default() += edge.weight;
            degree[a] += edge.weight;
            degree[b] += edge.weight;
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

    /// Newman modularity of an arbitrary partition:
    /// `Q = Σ_c [ L_c/m − (D_c/2m)² ]`, where `L_c` is the edge weight
    /// inside community `c` and `D_c` the summed degree of its members.
    ///
    /// The same function scores the detected and the declared partition,
    /// which is the only reason the two numbers are comparable.
    fn modularity(&self, group_of: &[usize], group_count: usize) -> f64 {
        if self.total_weight == 0 {
            return 0.0;
        }
        let m = self.total_weight as f64;
        let mut internal = vec![0u64; group_count];
        let mut degree = vec![0u64; group_count];
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
                let share = degree[g] as f64 / (2.0 * m);
                internal[g] as f64 / m - share * share
            })
            .sum()
    }

    /// Greedy modularity agglomeration, run to the point where no merge
    /// improves `Q`.
    ///
    /// Every merge retires one community, so `n` nodes admit at most
    /// `n - 1` of them. The bound is what keeps a merge that fails to
    /// make progress — one the loop would otherwise re-propose forever —
    /// from hanging the analyzer instead of producing a wrong answer a
    /// test can see.
    fn agglomerate(&self) -> Partition {
        let mut state = Agglomerator::new(self);
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
                internal_weight: 0,
                external_weight: 0,
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
                .cmp(&a.evidence)
                .then_with(|| b.weight_to_suggested.cmp(&a.weight_to_suggested))
                .then_with(|| a.node.cmp(&b.node))
        });
        rows
    }

    fn misfiled_row(
        &self,
        node: usize,
        community: usize,
        dominant: usize,
        reach: &[BTreeMap<usize, u64>],
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
        let weight_to_declared = reach[node].get(&declared).copied().unwrap_or(0);
        let weight_to_suggested = reach[node].get(&dominant).copied().unwrap_or(0);
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
    fn weights_by_group(&self) -> Vec<BTreeMap<usize, u64>> {
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
    internal_weight: u64,
    external_weight: u64,
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

/// Greedy modularity agglomeration state.
///
/// Communities are keyed by their smallest member index rather than by
/// allocation order, so every map iterated below yields the same
/// sequence for the same graph no matter what order the input arrived
/// in.
struct Agglomerator {
    members: BTreeMap<usize, Vec<usize>>,
    degree: BTreeMap<usize, u64>,
    between: BTreeMap<(usize, usize), u64>,
    neighbors: BTreeMap<usize, BTreeSet<usize>>,
    /// Total edge weight, each pair counted once.
    m: f64,
}

impl Agglomerator {
    fn new(graph: &Graph) -> Self {
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
            m: graph.total_weight as f64,
        }
    }

    /// The merge with the largest modularity gain, or `None` once no
    /// merge helps.
    ///
    /// Merging communities `x` and `y` changes `Q` by
    /// `w_xy/m − D_x·D_y/(2m²)`: the weight the merge internalises,
    /// minus what two groups that well-connected would share by chance.
    /// Scanning `between` in ascending key order and improving on a
    /// strict `>` makes an exact tie resolve to the smallest pair.
    fn best_merge(&self) -> Option<(usize, usize)> {
        if self.m == 0.0 {
            return None;
        }
        let mut best: Option<((usize, usize), f64)> = None;
        for (&(x, y), &w) in &self.between {
            let dx = self.degree.get(&x).copied().unwrap_or(0) as f64;
            let dy = self.degree.get(&y).copied().unwrap_or(0) as f64;
            let gain = w as f64 / self.m - dx * dy / (2.0 * self.m * self.m);
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
        let dy = self.degree.remove(&y).unwrap_or(0);
        *self.degree.entry(x).or_default() += dy;

        for z in self.neighbors.remove(&y).unwrap_or_default() {
            self.neighbors.entry(z).or_default().remove(&y);
            let w = self.between.remove(&ordered(y, z)).unwrap_or(0);
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

fn ordered(a: usize, b: usize) -> (usize, usize) {
    (a.min(b), a.max(b))
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
        if node.declared.as_str() < *entry {
            *entry = &node.declared;
        }
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
        let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
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
        let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
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

        let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
        let row = report
            .misfiled
            .iter()
            .find(|m| m.node == "a3")
            .unwrap_or_else(|| panic!("no a3 row in {report:?}"));
        assert_eq!(row.declared, "a");
        assert_eq!(row.suggested, "b");
        assert_eq!(row.weight_to_declared, 0);
        assert_eq!(row.weight_to_suggested, 3);
        assert_eq!(row.evidence, 3);
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
        let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
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
        let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
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
        let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
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
        let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
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
        let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
        assert_eq!(report.spanning.len(), 1, "got {report:?}");
        assert_eq!(report.spanning[0].size, 2);
        assert_eq!(report.spanning[0].declared_group_count, 2);
    }

    /// The misfiled gate is strict: equal weight either way is not
    /// evidence for a move, so the tie stays home.
    #[test]
    fn a_member_pulled_equally_both_ways_is_not_reported() {
        let nodes = vec![
            CommunityNode::new("a1", "a"),
            CommunityNode::new("b1", "b"),
            CommunityNode::new("b2", "b"),
            CommunityNode::new("b3", "b"),
        ];
        // a1 has 2 units into `b` and 2 into its own `a` — a tie, so no
        // row, even though its community is dominated by `b`.
        let nodes = nodes
            .into_iter()
            .chain([CommunityNode::new("a2", "a")])
            .collect::<Vec<_>>();
        let edges = vec![
            CommunityEdge::new("a1", "a2", 2),
            CommunityEdge::new("a1", "b1", 2),
            CommunityEdge::new("b1", "b2", 3),
            CommunityEdge::new("b2", "b3", 3),
            CommunityEdge::new("b1", "b3", 3),
        ];
        let report = detect_communities(&nodes, &edges, 1);
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
        let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
        let row = report
            .misfiled
            .iter()
            .find(|m| m.node == "z")
            .unwrap_or_else(|| panic!("no z row in {report:?}"));
        assert_eq!(row.weight_to_suggested, 12, "got {report:?}");
        assert_eq!(row.weight_to_declared, 2, "got {report:?}");
        assert_eq!(row.evidence, 10, "got {report:?}");
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
            let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
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
        let report = detect_communities(&nodes, &edges, 1);
        let holding_b = report
            .communities
            .iter()
            .find(|c| c.members.contains(&"b1".to_owned()))
            .unwrap_or_else(|| panic!("no b1 community in {report:?}"));
        assert_eq!(holding_b.members, ["b1", "b2"], "got {report:?}");
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
        let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
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
        let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
        assert_eq!(report.spanning.len(), 1, "got {report:?}");
        assert_eq!(report.spanning[0].declared_group_count, 3);
        assert_eq!(report.spanning[0].size, 3);
    }

    #[test]
    fn an_edgeless_graph_reports_no_structure_rather_than_guessing() {
        let nodes = vec![CommunityNode::new("a", "a"), CommunityNode::new("b", "b")];
        let report = detect_communities(&nodes, &[], DEFAULT_MIN_COMMUNITY);
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
        let report = detect_communities(&nodes, std::slice::from_ref(&edge), 1);
        assert_eq!(report.edge_count, 0, "{edge:?} should not become an edge");
        assert_eq!(report.total_weight, 0);
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
        let report = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
        assert_eq!(report.edge_count, 1);
        assert_eq!(report.total_weight, 5);
    }

    /// `min_community` bounds the listings only. The partition, and
    /// therefore both modularity figures, must be the same either way —
    /// otherwise a display cap would silently change the headline
    /// numbers.
    #[test]
    fn min_community_bounds_the_listings_not_the_partition() {
        let (nodes, edges) = barbell();
        let wide = detect_communities(&nodes, &edges, 1);
        let narrow = detect_communities(&nodes, &edges, 4);
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
        let report = detect_communities(&nodes, &edges, 1);
        let internal: u64 = report.communities.iter().map(|c| c.internal_weight).sum();
        let external: u64 = report.communities.iter().map(|c| c.external_weight).sum();
        assert_eq!(internal, 6);
        assert_eq!(external, 2);
        assert_eq!(internal + external / 2, report.total_weight);
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
            detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY),
            detect_communities(&rotated_nodes, &rotated_edges, DEFAULT_MIN_COMMUNITY),
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
            raw_edges in vec((0_usize..14, 0_usize..14, 1_u64..5), 0..40),
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

            let baseline = detect_communities(&nodes, &edges, DEFAULT_MIN_COMMUNITY);
            let shuffled = detect_communities(
                &permute(&nodes, &node_permutation),
                &permute(&edges, &edge_permutation),
                DEFAULT_MIN_COMMUNITY,
            );
            prop_assert_eq!(&baseline, &shuffled);
        }
    }
}
