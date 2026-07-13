//! Wrap-chain discovery over a directed propagation graph.
//!
//! Input: the subgraph of *wrap-only* functions (see
//! [`FunctionErrorShape::wrap_only_error_path`]) connected by resolved
//! caller→callee edges. Every node on such a path does nothing with an
//! error except (possibly wrap and) hand it on, so a long path means
//! the error crosses many layers before anything actually happens to
//! it — the "wrap at every layer" smell.
//!
//! Output: one maximal chain per entry point (a node no other
//! wrap-only node calls into), following the *longest* propagation
//! path downward. Cycles (mutual recursion) are collapsed into a
//! single [`ChainLink`] via strongly-connected components, so the
//! walk always terminates and a recursive pair reads as one link
//! rather than an infinite path.
//!
//! Like the rest of the domain layer this is pure graph shape: nodes
//! are `0..node_count` indices, and the caller owns the mapping back
//! to real functions.
//!
//! [`FunctionErrorShape::wrap_only_error_path`]: crate::error_shape::FunctionErrorShape::wrap_only_error_path

/// One step of a chain: a single function, or a group of mutually
/// recursive functions collapsed into one strongly-connected
/// component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainLink {
    /// Node indices in this link, ascending. Length 1 for the normal
    /// non-recursive case.
    pub members: Vec<usize>,
}

impl ChainLink {
    /// True when the link is a mutual-recursion group.
    pub fn is_cycle(&self) -> bool {
        self.members.len() > 1
    }
}

/// A maximal caller→callee chain through wrap-only nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapChain {
    /// Links in caller→callee order: `links[0]` is the entry point no
    /// other wrap-only node calls, `links.last()` is the deepest
    /// propagation-only callee.
    pub links: Vec<ChainLink>,
}

impl WrapChain {
    /// Number of functions on the chain (cycle members all count).
    pub fn depth(&self) -> usize {
        self.links.iter().map(|l| l.members.len()).sum()
    }

    /// True when any link is a mutual-recursion group.
    pub fn has_cycle(&self) -> bool {
        self.links.iter().any(ChainLink::is_cycle)
    }
}

/// Compute one maximal wrap chain per entry point of the graph
/// `0..node_count` with directed `edges` (caller → callee).
///
/// * Every node appears in exactly one strongly-connected component;
///   components become [`ChainLink`]s.
/// * An *entry point* is a component no other component points into.
///   From each, the chain follows the successor that maximises total
///   depth, so parallel branches yield the longest representative
///   path, not an exhaustive enumeration.
/// * Self-loops and duplicate edges are tolerated; out-of-range
///   endpoints are ignored.
///
/// Chains are sorted by depth (descending), then by first member
/// index for determinism. Isolated nodes come back as depth-1 chains;
/// callers typically drop those with a minimum-depth filter.
pub fn compute_wrap_chains(node_count: usize, edges: &[(usize, usize)]) -> Vec<WrapChain> {
    let mut adj = vec![Vec::new(); node_count];
    for &(from, to) in edges {
        if from < node_count && to < node_count {
            adj[from].push(to);
        }
    }

    // Tarjan emits each SCC only after every SCC it points into, so
    // component ids come out in reverse-topological order: successors
    // of component `c` always have ids < `c`.
    let sccs = tarjan_sccs(node_count, &adj);
    let mut comp_of = vec![0usize; node_count];
    for (comp, members) in sccs.iter().enumerate() {
        for &v in members {
            comp_of[v] = comp;
        }
    }

    // Condensation edges, deduplicated.
    let mut comp_succ = vec![Vec::new(); sccs.len()];
    let mut has_incoming = vec![false; sccs.len()];
    for (from, tos) in adj.iter().enumerate() {
        for &to in tos {
            let (cf, ct) = (comp_of[from], comp_of[to]);
            if cf != ct && !comp_succ[cf].contains(&ct) {
                comp_succ[cf].push(ct);
                has_incoming[ct] = true;
            }
        }
    }

    // Longest-path DP in emission order: successors have smaller ids,
    // so they are always computed before the components that call them.
    let mut best_depth = vec![0usize; sccs.len()];
    let mut best_next: Vec<Option<usize>> = vec![None; sccs.len()];
    for comp in 0..sccs.len() {
        let own = sccs[comp].len();
        let mut best = 0usize;
        let mut next = None;
        for &succ in &comp_succ[comp] {
            if best_depth[succ] > best {
                best = best_depth[succ];
                next = Some(succ);
            }
        }
        best_depth[comp] = own + best;
        best_next[comp] = next;
    }

    let mut chains: Vec<WrapChain> = (0..sccs.len())
        .filter(|&comp| !has_incoming[comp])
        .map(|head| {
            let mut links = Vec::new();
            let mut cursor = Some(head);
            while let Some(comp) = cursor {
                let mut members = sccs[comp].clone();
                members.sort_unstable();
                links.push(ChainLink { members });
                cursor = best_next[comp];
            }
            WrapChain { links }
        })
        .collect();

    chains.sort_by(|a, b| {
        b.depth()
            .cmp(&a.depth())
            .then_with(|| a.links[0].members.cmp(&b.links[0].members))
    });
    chains
}

/// Iterative Tarjan SCC. Returns components in emission order, which
/// is the reverse-topological order of the condensation.
fn tarjan_sccs(node_count: usize, adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNVISITED: usize = usize::MAX;
    let mut index = vec![UNVISITED; node_count];
    let mut low = vec![0usize; node_count];
    let mut on_stack = vec![false; node_count];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut sccs: Vec<Vec<usize>> = Vec::new();

    // Explicit DFS frames: (node, next adjacency offset).
    let mut frames: Vec<(usize, usize)> = Vec::new();
    for root in 0..node_count {
        if index[root] != UNVISITED {
            continue;
        }
        frames.push((root, 0));
        index[root] = next_index;
        low[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;

        while let Some(&mut (v, ref mut edge_idx)) = frames.last_mut() {
            if let Some(&w) = adj[v].get(*edge_idx) {
                *edge_idx += 1;
                if index[w] == UNVISITED {
                    index[w] = next_index;
                    low[w] = next_index;
                    next_index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    frames.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                frames.pop();
                if let Some(&(parent, _)) = frames.last() {
                    low[parent] = low[parent].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut component = Vec::new();
                    loop {
                        let w = stack.pop().unwrap_or(v);
                        on_stack[w] = false;
                        component.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(component);
                }
            }
        }
    }
    sccs
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn flat(chain: &WrapChain) -> Vec<Vec<usize>> {
        chain.links.iter().map(|l| l.members.clone()).collect()
    }

    #[test]
    fn empty_graph_yields_no_chains() {
        assert!(compute_wrap_chains(0, &[]).is_empty());
    }

    #[test]
    fn straight_line_is_one_chain() {
        // 0 → 1 → 2
        let chains = compute_wrap_chains(3, &[(0, 1), (1, 2)]);
        assert_eq!(chains.len(), 1);
        assert_eq!(flat(&chains[0]), [[0], [1], [2]]);
        assert_eq!(chains[0].depth(), 3);
        assert!(!chains[0].has_cycle());
    }

    #[test]
    fn diamond_reports_one_longest_path_per_head() {
        // 0 → 1 → 3, 0 → 2 → 3 → 4: single head 0, longest depth 4
        // via either middle; the tie-break must stay deterministic.
        let chains = compute_wrap_chains(5, &[(0, 1), (0, 2), (1, 3), (2, 3), (3, 4)]);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].depth(), 4);
        assert_eq!(chains[0].links[0].members, [0]);
        assert_eq!(chains[0].links.last().unwrap().members, [4]);
    }

    #[test]
    fn two_heads_sharing_a_tail_yield_two_chains() {
        // 0 → 2, 1 → 2
        let chains = compute_wrap_chains(3, &[(0, 2), (1, 2)]);
        assert_eq!(chains.len(), 2);
        assert_eq!(flat(&chains[0]), [[0], [2]]);
        assert_eq!(flat(&chains[1]), [[1], [2]]);
    }

    #[test]
    fn mutual_recursion_collapses_into_one_cyclic_link() {
        // 0 → (1 ⇄ 2) → 3
        let chains = compute_wrap_chains(4, &[(0, 1), (1, 2), (2, 1), (2, 3)]);
        assert_eq!(chains.len(), 1);
        assert_eq!(flat(&chains[0]), vec![vec![0], vec![1, 2], vec![3]]);
        assert_eq!(chains[0].depth(), 4);
        assert!(chains[0].has_cycle());
        assert!(chains[0].links[1].is_cycle());
    }

    #[test]
    fn isolated_nodes_come_back_as_depth_one_chains() {
        let chains = compute_wrap_chains(2, &[]);
        assert_eq!(chains.len(), 2);
        assert!(chains.iter().all(|c| c.depth() == 1));
    }

    #[test]
    fn chains_are_sorted_by_depth_descending() {
        // 3 → 4 (depth 2) and 0 → 1 → 2 (depth 3).
        let chains = compute_wrap_chains(5, &[(3, 4), (0, 1), (1, 2)]);
        assert_eq!(chains[0].depth(), 3);
        assert_eq!(chains[1].depth(), 2);
    }

    #[rstest]
    #[case::self_loop(&[(0, 0)])]
    #[case::duplicate_edges(&[(0, 1), (0, 1)])]
    #[case::out_of_range(&[(0, 9), (9, 0), (0, 1)])]
    fn degenerate_edges_are_tolerated(#[case] edges: &[(usize, usize)]) {
        let chains = compute_wrap_chains(2, edges);
        // Every node is still covered exactly once across all chains'
        // heads-following paths… at minimum the call must not panic
        // and each chain must be non-empty.
        assert!(!chains.is_empty());
        assert!(chains.iter().all(|c| c.depth() >= 1));
    }

    use proptest::prelude::*;

    fn arb_graph() -> impl Strategy<Value = (usize, Vec<(usize, usize)>)> {
        (1usize..12).prop_flat_map(|n| {
            let edge = (0..n, 0..n);
            (Just(n), proptest::collection::vec(edge, 0..30))
        })
    }

    proptest! {
        /// depth() is exactly the sum of link sizes, and every link is
        /// non-empty.
        #[test]
        fn depth_is_sum_of_link_sizes((n, edges) in arb_graph()) {
            for chain in compute_wrap_chains(n, &edges) {
                prop_assert!(chain.links.iter().all(|l| !l.members.is_empty()));
                let sum: usize = chain.links.iter().map(|l| l.members.len()).sum();
                prop_assert_eq!(chain.depth(), sum);
            }
        }

        /// Consecutive links are really connected: some original edge
        /// leads from a member of link i into a member of link i+1.
        #[test]
        fn consecutive_links_are_connected((n, edges) in arb_graph()) {
            for chain in compute_wrap_chains(n, &edges) {
                for pair in chain.links.windows(2) {
                    let connected = edges.iter().any(|&(f, t)| {
                        pair[0].members.contains(&f) && pair[1].members.contains(&t)
                    });
                    prop_assert!(connected, "links not connected: {pair:?}");
                }
            }
        }

        /// No node appears twice within one chain (SCC condensation
        /// makes chains simple paths).
        #[test]
        fn chains_never_repeat_a_node((n, edges) in arb_graph()) {
            for chain in compute_wrap_chains(n, &edges) {
                let mut seen = std::collections::HashSet::new();
                for link in &chain.links {
                    for &m in &link.members {
                        prop_assert!(seen.insert(m), "node {m} repeated");
                    }
                }
            }
        }

        /// Every node belongs to at least one chain when it is a head,
        /// and chain heads have no incoming condensation edges — i.e.
        /// no chain's head is the tail of another edge from a
        /// different component.
        #[test]
        fn heads_have_no_wrap_only_callers((n, edges) in arb_graph()) {
            let chains = compute_wrap_chains(n, &edges);
            for chain in &chains {
                let head = &chain.links[0].members;
                let head_has_external_caller = edges.iter().any(|&(f, t)| {
                    head.contains(&t) && !head.contains(&f) && f < n && t < n
                });
                prop_assert!(!head_has_external_caller, "head {head:?} has a caller");
            }
        }
    }
}
