//! Reusable graph algorithms over node indices.
//!
//! These operate on plain adjacency lists (`Vec<Vec<usize>>`, indices
//! `0..n`) so any analyzer can run them against
//! [`super::CallGraph::resolved_adjacency`] or a derived subgraph.
//! Both algorithms are deterministic regardless of input neighbor
//! order, and [`condense`] is iterative: at function granularity a
//! recursive Tarjan (like the private module-level one in
//! `lens-domain/src/coupling.rs`) risks stack overflow on deep call
//! chains.

const UNVISITED: usize = usize::MAX;

/// Strongly-connected-component condensation of a directed graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Condensation {
    /// SCCs in reverse topological order (every edge in the condensed
    /// DAG points from a higher component index to a lower one).
    /// Members of each component are sorted ascending. A node with a
    /// self-loop still forms a size-1 component; callers that care
    /// about self-recursion must inspect the original adjacency.
    pub(crate) components: Vec<Vec<usize>>,
    /// `component_of[v]` is the index into `components` containing `v`.
    pub(crate) component_of: Vec<usize>,
    /// Condensed DAG adjacency over component indices, sorted and
    /// deduplicated, self-edges removed.
    pub(crate) edges: Vec<Vec<usize>>,
}

/// Iterative Tarjan SCC over `adjacency` (nodes `0..adjacency.len()`,
/// neighbor values must be in range).
pub(crate) fn condense(adjacency: &[Vec<usize>]) -> Condensation {
    let n = adjacency.len();
    let mut index = vec![UNVISITED; n];
    let mut lowlink = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut components: Vec<Vec<usize>> = Vec::new();
    let mut component_of = vec![UNVISITED; n];
    // Explicit DFS frames: (node, next neighbor offset). Replaces the
    // recursion in the classic formulation.
    let mut frames: Vec<(usize, usize)> = Vec::new();

    for root in 0..n {
        if index[root] != UNVISITED {
            continue;
        }
        index[root] = next_index;
        lowlink[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;
        frames.push((root, 0));

        while let Some(&(v, neighbor_offset)) = frames.last() {
            if let Some(&w) = adjacency[v].get(neighbor_offset) {
                if let Some(frame) = frames.last_mut() {
                    frame.1 += 1;
                }
                if index[w] == UNVISITED {
                    index[w] = next_index;
                    lowlink[w] = next_index;
                    next_index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    frames.push((w, 0));
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index[w]);
                }
            } else {
                frames.pop();
                if let Some(&(parent, _)) = frames.last() {
                    lowlink[parent] = lowlink[parent].min(lowlink[v]);
                }
                if lowlink[v] == index[v] {
                    let mut component = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        component_of[w] = components.len();
                        component.push(w);
                        if w == v {
                            break;
                        }
                    }
                    component.sort_unstable();
                    components.push(component);
                }
            }
        }
    }

    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); components.len()];
    for (v, neighbors) in adjacency.iter().enumerate() {
        for &w in neighbors {
            let (from, to) = (component_of[v], component_of[w]);
            if from != to {
                edges[from].push(to);
            }
        }
    }
    for neighbors in &mut edges {
        neighbors.sort_unstable();
        neighbors.dedup();
    }

    Condensation {
        components,
        component_of,
        edges,
    }
}

/// A weighted directed edge handed to [`greedy_feedback_arcs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WeightedEdge {
    pub(crate) from: usize,
    pub(crate) to: usize,
    pub(crate) weight: usize,
}

/// Approximate minimum-weight feedback arc set via the Eades–Lin–Smyth
/// greedy vertex-ordering heuristic (GR), weighted.
///
/// Builds a linear arrangement by repeatedly peeling weighted sinks to
/// the back, weighted sources to the front, and otherwise the vertex
/// with the largest `out_weight - in_weight`, then returns the indices
/// into `edges` of every edge pointing backwards in that arrangement.
/// Removing the returned edges from the graph is guaranteed to leave it
/// acyclic (self-edges excepted: they are ignored throughout and never
/// returned). The result is deterministic — ties are broken by the
/// lowest vertex index — and advisory: a cheapest edge by weight can
/// still be load-bearing in the design.
pub(crate) fn greedy_feedback_arcs(node_count: usize, edges: &[WeightedEdge]) -> Vec<usize> {
    let n = node_count;
    let mut out_edges: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    let mut in_edges: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    let mut out_weight = vec![0isize; n];
    let mut in_weight = vec![0isize; n];
    for edge in edges {
        if edge.from == edge.to {
            continue;
        }
        out_edges[edge.from].push((edge.to, edge.weight));
        in_edges[edge.to].push((edge.from, edge.weight));
        out_weight[edge.from] += edge.weight as isize;
        in_weight[edge.to] += edge.weight as isize;
    }

    let mut removed = vec![false; n];
    let mut remaining = n;
    let mut front: Vec<usize> = Vec::new();
    let mut back: Vec<usize> = Vec::new();
    let remove = |v: usize,
                  removed: &mut Vec<bool>,
                  out_weight: &mut Vec<isize>,
                  in_weight: &mut Vec<isize>| {
        removed[v] = true;
        for &(to, w) in &out_edges[v] {
            if !removed[to] {
                in_weight[to] -= w as isize;
            }
        }
        for &(from, w) in &in_edges[v] {
            if !removed[from] {
                out_weight[from] -= w as isize;
            }
        }
    };

    while remaining > 0 {
        while let Some(v) = (0..n).find(|&v| !removed[v] && out_weight[v] == 0) {
            remove(v, &mut removed, &mut out_weight, &mut in_weight);
            remaining -= 1;
            back.push(v);
        }
        while let Some(v) = (0..n).find(|&v| !removed[v] && in_weight[v] == 0) {
            remove(v, &mut removed, &mut out_weight, &mut in_weight);
            remaining -= 1;
            front.push(v);
        }
        if remaining > 0 {
            // max_by_key keeps the last max on ties, so Reverse(v)
            // makes the lowest vertex index win deterministically.
            if let Some(v) = (0..n)
                .filter(|&v| !removed[v])
                .max_by_key(|&v| (out_weight[v] - in_weight[v], std::cmp::Reverse(v)))
            {
                remove(v, &mut removed, &mut out_weight, &mut in_weight);
                remaining -= 1;
                front.push(v);
            }
        }
    }

    let mut position = vec![0usize; n];
    for (pos, &v) in front.iter().chain(back.iter().rev()).enumerate() {
        position[v] = pos;
    }
    edges
        .iter()
        .enumerate()
        .filter(|(_, e)| e.from != e.to && position[e.from] > position[e.to])
        .map(|(idx, _)| idx)
        .collect()
}

/// One node reached by a breadth-first traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BfsVisit {
    pub(crate) node: usize,
    /// Minimum edge distance from the nearest start node.
    pub(crate) depth: usize,
}

/// Breadth-first traversal from `starts` following edges forward.
///
/// Visits are emitted level by level with each level sorted by node
/// index, so the output order is deterministic regardless of the
/// adjacency's neighbor order. Start nodes appear at depth 0
/// (deduplicated); out-of-range start indices are ignored.
pub(crate) fn bfs(adjacency: &[Vec<usize>], starts: &[usize]) -> Vec<BfsVisit> {
    let n = adjacency.len();
    let mut seen = vec![false; n];
    let mut level: Vec<usize> = starts.iter().copied().filter(|&v| v < n).collect();
    level.sort_unstable();
    level.dedup();
    for &v in &level {
        seen[v] = true;
    }
    let mut visits = Vec::new();
    let mut depth = 0usize;
    while !level.is_empty() {
        visits.extend(level.iter().map(|&node| BfsVisit { node, depth }));
        let mut next: Vec<usize> = Vec::new();
        for &v in &level {
            for &w in &adjacency[v] {
                if !seen[w] {
                    seen[w] = true;
                    next.push(w);
                }
            }
        }
        next.sort_unstable();
        level = next;
        depth += 1;
    }
    visits
}

/// Breadth-first traversal from `starts` following edges backwards
/// (callers of callers, for blast-radius queries).
pub(crate) fn reverse_bfs(adjacency: &[Vec<usize>], starts: &[usize]) -> Vec<BfsVisit> {
    bfs(&reverse_adjacency(adjacency), starts)
}

/// Reverse every edge, keeping neighbor lists sorted.
pub(crate) fn reverse_adjacency(adjacency: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut reversed: Vec<Vec<usize>> = vec![Vec::new(); adjacency.len()];
    for (v, neighbors) in adjacency.iter().enumerate() {
        for &w in neighbors {
            reversed[w].push(v);
        }
    }
    for neighbors in &mut reversed {
        neighbors.sort_unstable();
    }
    reversed
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    /// Naive reference: set of nodes reachable from `start`, with
    /// minimum distances, via repeated relaxation.
    fn naive_distances(adjacency: &[Vec<usize>], starts: &[usize]) -> Vec<Option<usize>> {
        let n = adjacency.len();
        let mut dist: Vec<Option<usize>> = vec![None; n];
        for &s in starts {
            if s < n {
                dist[s] = Some(0);
            }
        }
        loop {
            let mut changed = false;
            for v in 0..n {
                let Some(dv) = dist[v] else { continue };
                for &w in &adjacency[v] {
                    if dist[w].is_none_or(|dw| dw > dv + 1) {
                        dist[w] = Some(dv + 1);
                        changed = true;
                    }
                }
            }
            if !changed {
                return dist;
            }
        }
    }

    fn reachable(adjacency: &[Vec<usize>], from: usize) -> Vec<bool> {
        let dist = naive_distances(adjacency, &[from]);
        dist.into_iter().map(|d| d.is_some()).collect()
    }

    fn arb_graph() -> impl Strategy<Value = Vec<Vec<usize>>> {
        (1usize..20)
            .prop_flat_map(|n| proptest::collection::vec(proptest::collection::vec(0..n, 0..=n), n))
    }

    #[rstest]
    #[case::empty(vec![], vec![])]
    #[case::single(vec![vec![]], vec![vec![0]])]
    #[case::self_loop_is_size_one(vec![vec![0]], vec![vec![0]])]
    #[case::two_cycle(vec![vec![1], vec![0]], vec![vec![0, 1]])]
    #[case::chain_reverse_topological(
        vec![vec![1], vec![2], vec![]],
        vec![vec![2], vec![1], vec![0]]
    )]
    #[case::cycle_with_tail(
        vec![vec![1], vec![2], vec![0, 3], vec![]],
        vec![vec![3], vec![0, 1, 2]]
    )]
    fn condense_finds_known_components(
        #[case] adjacency: Vec<Vec<usize>>,
        #[case] expected: Vec<Vec<usize>>,
    ) {
        assert_eq!(condense(&adjacency).components, expected);
    }

    #[test]
    fn condense_emits_condensed_dag_edges() {
        // 0 <-> 1 form one SCC feeding node 2; node 3 is isolated.
        let adjacency = vec![vec![1], vec![0, 2], vec![], vec![]];
        let condensation = condense(&adjacency);
        assert_eq!(condensation.components, vec![vec![2], vec![0, 1], vec![3]]);
        assert_eq!(condensation.edges, vec![vec![], vec![0], vec![]]);
    }

    #[test]
    fn condense_survives_deep_recursion_shaped_graphs() {
        // A 100k-node cycle would overflow the stack with a recursive
        // Tarjan; the iterative rewrite must fold it into one SCC.
        let n = 100_000;
        let adjacency: Vec<Vec<usize>> = (0..n).map(|v| vec![(v + 1) % n]).collect();
        let condensation = condense(&adjacency);
        assert_eq!(condensation.components.len(), 1);
        assert_eq!(condensation.components[0].len(), n);
        assert!(condensation.edges[0].is_empty());
    }

    #[rstest]
    #[case::single_start(vec![vec![1], vec![2], vec![]], vec![0], vec![(0, 0), (1, 1), (2, 2)])]
    #[case::unsorted_neighbors_visit_in_index_order(
        vec![vec![2, 1], vec![], vec![]],
        vec![0],
        vec![(0, 0), (1, 1), (2, 1)]
    )]
    #[case::duplicate_starts_dedupe(vec![vec![], vec![]], vec![1, 1, 0], vec![(0, 0), (1, 0)])]
    #[case::cycle_visits_once(vec![vec![1], vec![0]], vec![0], vec![(0, 0), (1, 1)])]
    #[case::out_of_range_start_ignored(vec![vec![]], vec![7], vec![])]
    fn bfs_visits_deterministically(
        #[case] adjacency: Vec<Vec<usize>>,
        #[case] starts: Vec<usize>,
        #[case] expected: Vec<(usize, usize)>,
    ) {
        let visits: Vec<(usize, usize)> = bfs(&adjacency, &starts)
            .into_iter()
            .map(|v| (v.node, v.depth))
            .collect();
        assert_eq!(visits, expected);
    }

    fn weighted(edges: &[(usize, usize, usize)]) -> Vec<WeightedEdge> {
        edges
            .iter()
            .map(|&(from, to, weight)| WeightedEdge { from, to, weight })
            .collect()
    }

    #[rstest]
    #[case::empty(0, vec![], vec![])]
    #[case::acyclic_chain_keeps_all_edges(3, vec![(0, 1, 1), (1, 2, 1)], vec![])]
    #[case::two_cycle_cuts_one_edge(2, vec![(0, 1, 1), (1, 0, 1)], vec![1])]
    #[case::two_cycle_cuts_the_cheaper_edge(2, vec![(0, 1, 3), (1, 0, 1)], vec![1])]
    #[case::two_cycle_cheaper_edge_wins_regardless_of_order(2, vec![(0, 1, 1), (1, 0, 3)], vec![0])]
    #[case::three_cycle_cuts_one_edge(3, vec![(0, 1, 1), (1, 2, 1), (2, 0, 1)], vec![2])]
    #[case::self_edge_is_never_cut(1, vec![(0, 0, 5)], vec![])]
    fn greedy_feedback_arcs_finds_known_cuts(
        #[case] node_count: usize,
        #[case] edges: Vec<(usize, usize, usize)>,
        #[case] expected: Vec<usize>,
    ) {
        assert_eq!(
            greedy_feedback_arcs(node_count, &weighted(&edges)),
            expected
        );
    }

    #[test]
    fn greedy_feedback_arcs_source_peeling_updates_downstream_in_weights() {
        // 0 is a source feeding the 1 <-> 2 cycle with a heavy edge.
        // Peeling it must subtract that weight from node 1's in-weight,
        // or the greedy pick flips to node 2 and cuts the heavy
        // 1 -> 2 edge instead of the cheap 2 -> 1 one.
        let edges = weighted(&[(0, 1, 10), (1, 2, 5), (2, 1, 1)]);
        assert_eq!(greedy_feedback_arcs(3, &edges), vec![2]);
    }

    #[test]
    fn greedy_feedback_arcs_sink_peeling_updates_upstream_out_weights() {
        // 0 is a sink fed by node 1 with a heavy edge. Peeling it must
        // subtract that weight from node 1's out-weight, or node 1's
        // inflated degree wins the greedy pick and the 1 <-> 2 cycle
        // is cut at the expensive 2 -> 1 edge instead of 1 -> 2.
        let edges = weighted(&[(1, 0, 6), (1, 2, 2), (2, 1, 3)]);
        assert_eq!(greedy_feedback_arcs(3, &edges), vec![1]);
    }

    #[test]
    fn greedy_feedback_arcs_in_weight_updates_subtract_exactly() {
        // Peeling source 0 must drop node 1's in-weight from 12 to 8
        // (subtracting the peeled edge). An inexact update that leaves
        // it near 3 flips the greedy pick to node 1 and cuts the
        // heavier 2 -> 1 edge instead of 1 -> 2.
        let edges = weighted(&[(0, 1, 4), (1, 2, 6), (2, 1, 8)]);
        assert_eq!(greedy_feedback_arcs(3, &edges), vec![1]);
    }

    #[test]
    fn greedy_feedback_arcs_out_weight_updates_subtract_exactly() {
        // Peeling sink 0 must drop node 1's out-weight from 12 to 10
        // (subtracting the peeled edge). An inexact update that leaves
        // it near 6 flips the greedy pick to node 2 and cuts the
        // heavier 1 -> 2 edge instead of 2 -> 1.
        let edges = weighted(&[(1, 0, 2), (1, 2, 10), (2, 1, 9)]);
        assert_eq!(greedy_feedback_arcs(3, &edges), vec![2]);
    }

    #[test]
    fn greedy_feedback_arcs_ranks_vertices_by_weight_difference() {
        // Node 1 has the largest out - in difference (10 - 8 = 2) and
        // must be ordered first; ranking by ratio instead of
        // difference would pick node 0 (2 / 1) and reverse a
        // different edge set.
        let edges = weighted(&[(0, 1, 2), (1, 0, 1), (1, 2, 9), (2, 1, 6)]);
        assert_eq!(greedy_feedback_arcs(3, &edges), vec![0, 3]);
    }

    #[test]
    fn greedy_feedback_arcs_prefers_cutting_light_edges() {
        // 0 -> 1 -> 2 -> 0 is a heavy cycle (weight 5 each) crossed by
        // a light back-edge 2 -> 1. The heuristic must not pay for a
        // heavy edge when reversing lighter ones suffices.
        let edges = weighted(&[(0, 1, 5), (1, 2, 5), (2, 0, 5), (2, 1, 1)]);
        let arcs = greedy_feedback_arcs(3, &edges);
        let cut_weight: usize = arcs.iter().map(|&i| edges[i].weight).sum();
        assert!(cut_weight <= 6, "cut {arcs:?} weighs {cut_weight}");
    }

    #[test]
    fn reverse_bfs_walks_callers() {
        // 0 -> 1 -> 2: reverse traversal from 2 reaches its transitive
        // callers with correct depths.
        let adjacency = vec![vec![1], vec![2], vec![]];
        let visits: Vec<(usize, usize)> = reverse_bfs(&adjacency, &[2])
            .into_iter()
            .map(|v| (v.node, v.depth))
            .collect();
        assert_eq!(visits, vec![(2, 0), (1, 1), (0, 2)]);
    }

    proptest! {
        #[test]
        fn condense_partitions_nodes(adjacency in arb_graph()) {
            let condensation = condense(&adjacency);
            let mut seen = vec![false; adjacency.len()];
            for (idx, component) in condensation.components.iter().enumerate() {
                prop_assert!(!component.is_empty());
                prop_assert!(component.windows(2).all(|w| w[0] < w[1]));
                for &v in component {
                    prop_assert!(!seen[v]);
                    seen[v] = true;
                    prop_assert_eq!(condensation.component_of[v], idx);
                }
            }
            prop_assert!(seen.into_iter().all(|s| s));
        }

        #[test]
        fn condense_matches_mutual_reachability(adjacency in arb_graph()) {
            let condensation = condense(&adjacency);
            let reach: Vec<Vec<bool>> =
                (0..adjacency.len()).map(|v| reachable(&adjacency, v)).collect();
            for (v, reach_from_v) in reach.iter().enumerate() {
                for (w, reach_from_w) in reach.iter().enumerate() {
                    let same_component =
                        condensation.component_of[v] == condensation.component_of[w];
                    let mutually_reachable = reach_from_v[w] && reach_from_w[v];
                    prop_assert_eq!(same_component, mutually_reachable);
                }
            }
        }

        #[test]
        fn condense_orders_components_reverse_topologically(adjacency in arb_graph()) {
            let condensation = condense(&adjacency);
            for (from, neighbors) in condensation.edges.iter().enumerate() {
                for &to in neighbors {
                    // Reverse topological order also proves the
                    // condensed graph is a DAG: no equal indices, no
                    // back edges.
                    prop_assert!(to < from);
                }
            }
        }

        #[test]
        fn greedy_feedback_arcs_always_break_every_cycle(
            (node_count, edges) in (1usize..12).prop_flat_map(|n| {
                (
                    Just(n),
                    proptest::collection::vec((0..n, 0..n, 1usize..10), 0..40),
                )
            }),
        ) {
            let edges = weighted(&edges);
            let arcs = greedy_feedback_arcs(node_count, &edges);
            let cut: std::collections::HashSet<usize> = arcs.iter().copied().collect();
            let mut adjacency = vec![Vec::new(); node_count];
            for (idx, edge) in edges.iter().enumerate() {
                if !cut.contains(&idx) && edge.from != edge.to {
                    adjacency[edge.from].push(edge.to);
                }
            }
            let condensation = condense(&adjacency);
            for component in &condensation.components {
                prop_assert_eq!(
                    component.len(),
                    1,
                    "removing the feedback arcs must leave the graph acyclic",
                );
            }
        }

        #[test]
        fn bfs_depths_are_shortest_distances(
            adjacency in arb_graph(),
            raw_starts in proptest::collection::vec(0usize..20, 1..4),
        ) {
            let starts: Vec<usize> =
                raw_starts.into_iter().filter(|&s| s < adjacency.len()).collect();
            let visits = bfs(&adjacency, &starts);
            let expected = naive_distances(&adjacency, &starts);
            let mut seen = vec![None; adjacency.len()];
            for visit in &visits {
                prop_assert!(seen[visit.node].is_none(), "node visited twice");
                seen[visit.node] = Some(visit.depth);
            }
            prop_assert_eq!(seen, expected);
            let ordered = visits
                .windows(2)
                .all(|w| (w[0].depth, w[0].node) < (w[1].depth, w[1].node));
            prop_assert!(ordered, "visits must be sorted by (depth, node)");
        }

        #[test]
        fn reverse_bfs_inverts_forward_reachability(adjacency in arb_graph()) {
            for v in 0..adjacency.len() {
                let backward: Vec<usize> =
                    reverse_bfs(&adjacency, &[v]).into_iter().map(|x| x.node).collect();
                for w in 0..adjacency.len() {
                    let forward_reaches = reachable(&adjacency, w)[v];
                    prop_assert_eq!(backward.contains(&w), forward_reaches);
                }
            }
        }
    }
}
