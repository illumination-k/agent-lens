# Spike #467: betweenness / structural-hole brokerage vs the existing PageRank + IFC ranking

Result: **negative — do not ship.** Neither family survives the kill
criterion in issue #467 ("surfaces top-N rows the current metrics miss
_and_ those rows survive the qualitative check").

## Method

Graphs were taken from the tool's own output over its own sources,
scoped like the `self` profile:

```bash
agent-lens analyze function-graph crates/agent-lens --exclude-tests --exclude 'benches/**' > function_graph.json
agent-lens analyze hubs           crates/agent-lens --exclude-tests --exclude 'benches/**' --format json > hubs.json
agent-lens analyze coupling       crates/agent-lens --exclude-tests --format json > coupling.json
python3 spike_467.py   # expects the three JSON files next to it
```

`spike_467.py` rebuilds the candidate subgraph exactly as `hubs` does
(resolved edges only, self-loops dropped, call-count weights), replicates
the fixed-iteration PageRank from `call_graph/algo.rs` (verified: 0
percentile mismatches against `hubs.json` across all 1111 nodes), and
computes exact Brandes betweenness (directed, unweighted) plus Burt's
effective size and constraint (undirected, call-count-weighted). Function
graph: 1111 nodes, 1183 resolved distinct edges. Module graph: 148
modules, 930 symbol edges from `coupling`.

## Measured correlations (Spearman)

| pair                                   | function graph | module graph |
| -------------------------------------- | -------------- | ------------ |
| betweenness vs PageRank                | +0.24          | +0.76        |
| betweenness vs degree (fan_in+fan_out) | +0.68          | +0.73        |
| effective size vs degree               | **+0.99**      | **+0.98**    |
| constraint vs degree (connected nodes) | **−0.98**      | **−0.92**    |

Top-20 set overlap:

| pair                    | function graph | module graph |
| ----------------------- | -------------- | ------------ |
| betweenness ∩ PageRank  | 1/20           | 7/20         |
| betweenness ∩ degree    | 6/20           | 10/20        |
| effective size ∩ degree | 19/20          | 20/20        |
| low-constraint ∩ degree | 18/20          | 15/20        |

## Burt's structural holes: redundant by construction here

The issue's hypothesis was that constraint/effective size are "genuinely
not a monotone function of degree". Empirically, on this graph class they
are: ρ(effective size, degree) = 0.99. Call and dependency graphs have
near-zero clustering — a function's callees almost never call each other —
so the redundancy term in effective size vanishes and it degenerates to
degree; constraint degenerates to ~1/degree. Zero rows in either graph's
top-20 that degree/PageRank/hubs did not already surface.

## Betweenness: novel rows exist but fail the qualitative check

Function graph, top-20 betweenness rows absent from PageRank top-20,
degree top-20, and every `hubs` outlier list:

| row                                           | bet | pr_rank | fan_in | fan_out |
| --------------------------------------------- | --- | ------- | ------ | ------- |
| `analyze::diff::indexed_changed_line_ranges`  | 170 | 260     | 1      | 3       |
| `analyze::module_graph::build_graph_uncached` | 141 | 152     | 1      | 7       |
| `cli::main`                                   | 84  | 190     | 1      | 2       |
| `analyze::diff::diff_repository`              | 76  | 331     | 1      | 1       |
| `cli::baseline::compare_baseline`             | 72  | 328     | 1      | 8       |
| `algo::Tarjan::visit_tree_rooted_at`          | 64  | 129     | 1      | 3       |
| `analyze::similarity::collect_changed_ranges` | 64  | 336     | 1      | 1       |
| `cli::baseline::run_baseline`                 | 63  | 705     | 1      | 2       |

Every one has fan_in = 1. These are not brokers whose change ripples
across otherwise-separate parts; they are single-caller pass-through
plumbing (`diff_repository` and `collect_changed_ranges` are fan_in=1 /
fan_out=1 chain links) and the CLI dispatch spine (`cli::main`,
`run_baseline`). On a tree-shaped region, betweenness of a chain link is
just ancestors × descendants — the "forwarding hop" pattern `analyze
delegation` already reports, misread as brokerage. The genuinely
bridge-like high-betweenness rows (`changed_line_ranges`, `condense`,
`AnalyzePathFilter::compile`, `path_looks_like_test`) all sit in the
PageRank top-50 or on a `hubs` outlier list already.

The decisive cut: restricting to plausible broker shapes (fan_in ≥ 2 and
fan_out ≥ 2, 28 nodes), **all 20** of the top-20 betweenness rows are
already surfaced by PageRank top-20, degree top-20, or a `hubs` outlier
list. Novel broker candidates: 0.

Module graph: the betweenness top-4 (`analyze::index`, `config`,
`analyze::call_graph`, `analyze::module_graph`) is what `coupling`'s
fan-in/IFC already reports. The four rows unique to betweenness
(`hooks::core::session_summary`, `analyze::hotspot`, `analyze::complexity`,
`analyze::wrapper`) sit at PageRank ranks 24–39 — a reshuffle inside the
same mid-tier, not a discovery; none is a module a maintainer would call
an architectural broker.

A fragility note that compounds the negative: betweenness is far more
sensitive to unresolved edges than PageRank. A single recovered edge that
bypasses a chain zeroes the chain's betweenness, where PageRank degrades
smoothly — and several of the "novel" rows are exactly such chain
artifacts of the partial graph.

## Conclusion

Close #467 without shipping. The correlations above are the recorded
negative result; re-opening the question should start from a graph class
with meaningfully non-zero clustering (e.g. a co-change graph), not from
the static call graph.
