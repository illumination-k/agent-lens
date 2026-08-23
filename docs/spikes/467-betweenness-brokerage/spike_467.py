#!/usr/bin/env python3
"""Spike for issue #467: betweenness + Burt structural holes vs existing
PageRank / fan-in / fan-out rankings, over agent-lens's own function and
module graphs.

Inputs (produced by the agent-lens CLI, `self`-profile scoping):
  function_graph.json  analyze function-graph crates/agent-lens --exclude-tests --exclude 'benches/**'
  hubs.json            analyze hubs           (same scoping)
  coupling.json        analyze coupling crates/agent-lens --exclude-tests
"""

import json
import math
import os
import sys
from collections import defaultdict, deque

S = os.path.dirname(os.path.abspath(__file__))


# ---------------------------------------------------------------- helpers

def spearman(xs, ys):
    """Spearman rho with average ranks for ties."""
    def ranks(v):
        order = sorted(range(len(v)), key=lambda i: v[i])
        r = [0.0] * len(v)
        i = 0
        while i < len(order):
            j = i
            while j + 1 < len(order) and v[order[j + 1]] == v[order[i]]:
                j += 1
            avg = (i + j) / 2.0 + 1.0
            for k in range(i, j + 1):
                r[order[k]] = avg
            i = j + 1
        return r

    rx, ry = ranks(xs), ranks(ys)
    n = len(xs)
    mx, my = sum(rx) / n, sum(ry) / n
    cov = sum((a - mx) * (b - my) for a, b in zip(rx, ry))
    sx = math.sqrt(sum((a - mx) ** 2 for a in rx))
    sy = math.sqrt(sum((b - my) ** 2 for b in ry))
    return cov / (sx * sy) if sx > 0 and sy > 0 else float('nan')


def pagerank(weighted_adj, n, damping=0.85, iterations=100):
    """Replica of algo.rs pagerank (weighted, dangling redistribution)."""
    if n == 0:
        return []
    out_weight = [sum(w for _, w in weighted_adj[u]) for u in range(n)]
    rank = [1.0 / n] * n
    for _ in range(iterations):
        dangling = sum(r for r, w in zip(rank, out_weight) if w <= 0.0)
        base = (1.0 - damping) / n + damping * dangling / n
        nxt = [base] * n
        for u in range(n):
            if out_weight[u] <= 0.0:
                continue
            share = damping * rank[u] / out_weight[u]
            for v, w in weighted_adj[u]:
                nxt[v] += share * w
        rank = nxt
    return rank


def brandes_betweenness(adj, n):
    """Exact betweenness on a directed unweighted graph (Brandes 2001)."""
    bc = [0.0] * n
    for s in range(n):
        stack = []
        preds = [[] for _ in range(n)]
        sigma = [0.0] * n
        dist = [-1] * n
        sigma[s] = 1.0
        dist[s] = 0
        q = deque([s])
        while q:
            v = q.popleft()
            stack.append(v)
            for w in adj[v]:
                if dist[w] < 0:
                    dist[w] = dist[v] + 1
                    q.append(w)
                if dist[w] == dist[v] + 1:
                    sigma[w] += sigma[v]
                    preds[w].append(v)
        delta = [0.0] * n
        while stack:
            w = stack.pop()
            for v in preds[w]:
                delta[v] += (sigma[v] / sigma[w]) * (1.0 + delta[w])
            if w != s:
                bc[w] += delta[w]
    return bc


def burt_metrics(und_weight, n):
    """Burt's effective size and constraint on an undirected weighted graph.

    und_weight: dict[(i, j)] -> symmetric weight (i < j).
    Returns (effective_size, constraint, degree) lists; constraint is None
    for isolates.
    """
    nbrs = defaultdict(dict)
    for (i, j), w in und_weight.items():
        nbrs[i][j] = w
        nbrs[j][i] = w
    eff = [0.0] * n
    cons = [None] * n
    for i in range(n):
        total = sum(nbrs[i].values())
        if total <= 0:
            continue
        p = {j: w / total for j, w in nbrs[i].items()}
        # marginal strength of j's ties, normalized by j's strongest tie
        eff_i = 0.0
        for j in nbrs[i]:
            mmax = max(nbrs[j].values())
            red = sum(p.get(q, 0.0) * (nbrs[j][q] / mmax)
                      for q in nbrs[j] if q != i)
            eff_i += 1.0 - red
        eff[i] = eff_i
        c = 0.0
        for j in nbrs[i]:
            indirect = sum(p[q] * (nbrs[q].get(j, 0.0) / sum(nbrs[q].values()))
                           for q in p if q != j and q != i)
            c += (p[j] + indirect) ** 2
        cons[i] = c
    return eff, cons


def top_set(names, scores, k=20):
    order = sorted(range(len(scores)), key=lambda i: (-scores[i], names[i]))
    return [names[i] for i in order[:k]]


def report_family(label, names, bet, pr, fan_in, fan_out, eff, cons, extra_flagged):
    n = len(names)
    deg = [fan_in[i] + fan_out[i] for i in range(n)]
    print(f'\n=== {label} (n={n}) ===')
    print(f'spearman betweenness vs pagerank:    {spearman(bet, pr):+.3f}')
    print(f'spearman betweenness vs degree:      {spearman(bet, deg):+.3f}')
    print(f'spearman eff_size    vs degree:      {spearman(eff, deg):+.3f}')
    print(f'spearman eff_size    vs pagerank:    {spearman(eff, pr):+.3f}')
    connected = [i for i in range(n) if cons[i] is not None]
    print(f'spearman constraint  vs degree (connected only, n={len(connected)}): '
          f'{spearman([cons[i] for i in connected], [deg[i] for i in connected]):+.3f}')
    print(f'spearman constraint  vs pagerank (connected only): '
          f'{spearman([cons[i] for i in connected], [pr[i] for i in connected]):+.3f}')

    t_bet = top_set(names, bet)
    t_pr = top_set(names, pr)
    t_deg = top_set(names, deg)
    t_eff = top_set(names, eff)
    inv_cons = [(-cons[i] if cons[i] is not None else -math.inf) for i in range(n)]
    t_brok = top_set(names, inv_cons)  # low constraint = broker
    print(f'top-20 overlap betweenness vs pagerank: {len(set(t_bet) & set(t_pr))}/20')
    print(f'top-20 overlap betweenness vs degree:   {len(set(t_bet) & set(t_deg))}/20')
    print(f'top-20 overlap eff_size vs degree:      {len(set(t_eff) & set(t_deg))}/20')
    print(f'top-20 overlap low-constraint vs degree:{len(set(t_brok) & set(t_deg))}/20')

    known = set(t_pr) | set(t_deg) | set(extra_flagged)
    print(f'\n-- top-20 betweenness rows NOT in pagerank/degree top-20 nor flagged by hubs --')
    idx = {name: i for i, name in enumerate(names)}
    for name in t_bet:
        if name not in known:
            i = idx[name]
            print(f'  {name}: bet={bet[i]:.0f} pr_rank={sorted(pr, reverse=True).index(pr[i]) + 1} '
                  f'fan_in={fan_in[i]} fan_out={fan_out[i]} eff={eff[i]:.1f} cons={cons[i]}')
    print(f'\n-- top-20 effective-size rows NOT in pagerank/degree top-20 nor flagged --')
    for name in t_eff:
        if name not in known:
            i = idx[name]
            print(f'  {name}: eff={eff[i]:.1f} bet={bet[i]:.0f} fan_in={fan_in[i]} fan_out={fan_out[i]}')
    print(f'\n-- top-20 low-constraint (broker) rows NOT in pagerank/degree top-20 nor flagged --')
    for name in t_brok:
        if name not in known:
            i = idx[name]
            print(f'  {name}: cons={cons[i]:.3f} eff={eff[i]:.1f} bet={bet[i]:.0f} '
                  f'fan_in={fan_in[i]} fan_out={fan_out[i]}')
    return t_bet, t_pr


# ---------------------------------------------------------------- function graph

fg = json.load(open(f'{S}/function_graph.json'))
hubs = json.load(open(f'{S}/hubs.json'))

nodes = [nd for nd in fg['nodes'] if not nd['is_test']]
idx_of = {nd['id']: i for i, nd in enumerate(nodes)}
n = len(nodes)
wadj = [[] for _ in range(n)]
edge_weight = defaultdict(float)
for e in fg['edges']:
    if e['resolution'] != 'resolved' or not e['from'] or not e['to']:
        continue
    if e['from'] not in idx_of or e['to'] not in idx_of:
        continue
    u, v = idx_of[e['from']], idx_of[e['to']]
    if u == v:
        continue
    edge_weight[(u, v)] += e['weights']['call_count']
adj = [[] for _ in range(n)]
for (u, v), w in sorted(edge_weight.items()):
    wadj[u].append((v, w))
    adj[u].append(v)

pr = pagerank(wadj, n)

# sanity: our percentiles should match hubs' pagerank_percentile
hub_pct = {f['id']: f['pagerank_percentile'] for f in hubs['functions']}
srt = sorted(pr)
mism = 0
for i, nd in enumerate(nodes):
    at_or_below = 0
    lo, hi = 0, len(srt)
    while lo < hi:
        mid = (lo + hi) // 2
        if srt[mid] <= pr[i]:
            lo = mid + 1
        else:
            hi = mid
    pct = (lo * 100) // max(len(srt), 1)
    if nd['id'] in hub_pct and pct != hub_pct[nd['id']]:
        mism += 1
print(f'pagerank percentile sanity check: {mism} mismatches vs hubs.json out of {n}')

bet = brandes_betweenness(adj, n)
und = defaultdict(float)
for (u, v), w in edge_weight.items():
    und[(min(u, v), max(u, v))] += w
eff, cons = burt_metrics(und, n)

fan_in = [0] * n
fan_out = [0] * n
for (u, v) in edge_weight:
    fan_out[u] += 1
    fan_in[v] += 1

names = [nd['qualified_name'].removeprefix('agent_lens::') for nd in nodes]
flagged = set()
for key in ('god_functions', 'load_bearing', 'bottlenecks'):
    for f in hubs[key]:
        flagged.add(f['qualified_name'].removeprefix('agent_lens::'))
print(f'hubs flags {len(flagged)} functions as god/load-bearing/bottleneck')

t_bet_f, t_pr_f = report_family(
    'FUNCTION GRAPH', names, bet, pr, fan_in, fan_out, eff, cons, flagged)

# ---------------------------------------------------------------- module graph

cp = json.load(open(f'{S}/coupling.json'))
mods = sorted({m['path'] for m in cp['modules']})
midx = {m: i for i, m in enumerate(mods)}
mn = len(mods)
m_weight = defaultdict(float)
for e in cp['edges']:
    u, v = midx[e['from']], midx[e['to']]
    if u != v:
        m_weight[(u, v)] += 1.0  # one symbol dependency = weight 1
m_wadj = [[] for _ in range(mn)]
m_adj = [[] for _ in range(mn)]
for (u, v), w in sorted(m_weight.items()):
    m_wadj[u].append((v, w))
    m_adj[u].append(v)
m_pr = pagerank(m_wadj, mn)
m_bet = brandes_betweenness(m_adj, mn)
m_und = defaultdict(float)
for (u, v), w in m_weight.items():
    m_und[(min(u, v), max(u, v))] += w
m_eff, m_cons = burt_metrics(m_und, mn)
m_fi = [0] * mn
m_fo = [0] * mn
for (u, v) in m_weight:
    m_fo[u] += 1
    m_fi[v] += 1
ifc_top = top_set(mods, [m['ifc'] for m in sorted(cp['modules'], key=lambda x: x['path'])])
report_family('MODULE GRAPH', mods, m_bet, m_pr, m_fi, m_fo, m_eff, m_cons,
              set(ifc_top))

# full top-20 lists for the qualitative read
print('\n=== function graph: top-20 by betweenness (with existing-metric ranks) ===')
pr_rank = {i: r for r, i in enumerate(
    sorted(range(n), key=lambda i: (-pr[i], names[i])), 1)}
for name in t_bet_f:
    i = names.index(name)
    print(f'  bet={bet[i]:8.0f} pr_rank={pr_rank[i]:4d} fan_in={fan_in[i]:3d} '
          f'fan_out={fan_out[i]:3d}  {name}')

print('\n=== module graph: top-15 by betweenness ===')
m_pr_rank = {i: r for r, i in enumerate(
    sorted(range(mn), key=lambda i: (-m_pr[i], mods[i])), 1)}
for i in sorted(range(mn), key=lambda i: -m_bet[i])[:15]:
    print(f'  bet={m_bet[i]:8.0f} pr_rank={m_pr_rank[i]:4d} fan_in={m_fi[i]:3d} '
          f'fan_out={m_fo[i]:3d} cons={m_cons[i] if m_cons[i] is None else round(m_cons[i],3)}  {mods[i]}')

print('\n=== module graph: top-15 brokers by low constraint (min degree 3) ===')
elig = [i for i in range(mn) if m_cons[i] is not None and (m_fi[i] + m_fo[i]) >= 3]
for i in sorted(elig, key=lambda i: m_cons[i])[:15]:
    print(f'  cons={m_cons[i]:.3f} eff={m_eff[i]:6.1f} bet={m_bet[i]:8.0f} '
          f'fan_in={m_fi[i]:3d} fan_out={m_fo[i]:3d}  {mods[i]}')
