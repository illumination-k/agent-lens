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
