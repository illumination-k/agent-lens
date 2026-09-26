#!/usr/bin/env python3
"""Score an agent-lens function graph against a call-graph oracle.

The edge definition, the oracle JSON contract, and the meaning of every
number printed here are in docs/callgraph-accuracy.md. Standard library
only (Python >= 3.11, for tomllib).

    score.py --graph graph.json --oracle oracle.json \
        [--adjudications adjudications.toml --target NAME] \
        [--json out.json] [--disagreements out.json]

The Markdown report goes to stdout. `run.py` imports `score`, `merge` and
`render` to build the combined per-language table from the same counts.
"""

from __future__ import annotations

import argparse
import json
import sys
import tomllib
from collections import Counter, defaultdict
from pathlib import Path

# Order of the rows in every per-method table; any other method the graph
# reports is appended after these.
METHODS = ["lexical", "self_method", "last_segment", "path_suffix", "crate_narrowed"]
DISPATCHES = ["static", "dynamic", "callback"]
VERDICTS = ["oracle_wrong", "agent_lens_wrong", "definition_gap"]
# A pair the oracle reports under several dispatch kinds counts once, under
# the most specific one.
DISPATCH_PRIORITY = {d: i for i, d in enumerate(DISPATCHES)}

Pair = tuple[str, str]


# ---------------------------------------------------------------- mapping


class NodeIndex:
    """Maps (file, line) to the innermost agent-lens node containing it."""

    def __init__(self, graph: dict, source_root: Path | None):
        self.nodes = {n["id"]: n for n in graph["nodes"]}
        self.by_file: dict[str, list[dict]] = defaultdict(list)
        for n in graph["nodes"]:
            self.by_file[n["file"]].append(n)
        self.source_root = source_root
        self._lines: dict[str, list[str] | None] = {}

    def innermost(self, file: str, line: int) -> tuple[str | None, str | None]:
        """Return (node_id, None) or (None, reason)."""
        if file not in self.by_file:
            return None, "file_not_in_graph"
        hits = [n for n in self.by_file[file] if n["start_line"] <= line <= n["end_line"]]
        if not hits:
            return None, "no_enclosing_node"
        span = min(n["end_line"] - n["start_line"] for n in hits)
        tight = [n for n in hits if n["end_line"] - n["start_line"] == span]
        exact = [n for n in tight if n["start_line"] == line]
        if len(tight) > 1 and len(exact) == 1:
            tight = exact
        if len(tight) > 1:
            return None, "ambiguous_node"
        return tight[0]["id"], None

    def definition(self, file: str, def_line: int) -> tuple[str | None, str | None]:
        """Map a def/func line to the node it defines.

        The innermost node containing a nested def, a lambda or a Go func
        literal is its *enclosing* function, which is not the callee, so the
        def line has to be the node's own: its first line, or (Python
        decorators) reached from its first line without passing another def.
        """
        node_id, reason = self.innermost(file, def_line)
        if node_id is None:
            return None, reason
        start = self.nodes[node_id]["start_line"]
        if def_line == start:
            return node_id, None
        lines = self._source(file)
        if lines is None or def_line > len(lines):
            return None, "callee_nested_or_anonymous"
        between = [line.strip() for line in lines[start - 1 : def_line - 1]]
        if any(_is_def(line) for line in between) or not _is_def(lines[def_line - 1].strip()):
            return None, "callee_nested_or_anonymous"
        return node_id, None

    def _source(self, file: str) -> list[str] | None:
        if file not in self._lines:
            self._lines[file] = None
            if self.source_root is not None:
                try:
                    self._lines[file] = (self.source_root / file).read_text(errors="replace").splitlines()
                except OSError:
                    pass
        return self._lines[file]


def _is_def(line: str) -> bool:
    return line.startswith(("def ", "async def ", "class ", "func "))


# ---------------------------------------------------------------- scoring


def _oracle_pairs(oracle: dict, index: NodeIndex) -> tuple[dict[Pair, str], dict[Pair, set[int]], Counter]:
    pairs: dict[Pair, str] = {}
    lines: dict[Pair, set[int]] = defaultdict(set)
    unmapped: Counter = Counter()
    innermost_caller = oracle.get("caller_is_innermost_function", False)
    for e in oracle["edges"]:
        if innermost_caller:
            # caller_def_line is the innermost function around the call,
            # anonymous ones included: its node, else the node enclosing it.
            # (By call_line, a callback starting on the call's line would win.)
            caller, reason = index.definition(e["caller_file"], e["caller_def_line"])
            if caller is None:
                caller, reason = index.innermost(e["caller_file"], e["caller_def_line"])
        else:
            caller, reason = index.innermost(e["caller_file"], e["call_line"])
            if caller is None:
                caller, _ = index.innermost(e["caller_file"], e["caller_def_line"])
        if caller is None:
            unmapped[f"caller_{reason}"] += 1
            continue
        callee, reason = index.definition(e["callee_file"], e["callee_def_line"])
        if callee is None:
            unmapped[f"callee_{reason}" if not reason.startswith("callee_") else reason] += 1
            continue
        dispatch = e["dispatch"]
        prev = pairs.get((caller, callee))
        if prev is None or DISPATCH_PRIORITY[dispatch] < DISPATCH_PRIORITY[prev]:
            pairs[(caller, callee)] = dispatch
        lines[(caller, callee)].add(e["call_line"])
    return pairs, lines, unmapped


def _agent_lens_edges(graph: dict) -> tuple[dict[Pair, set[str]], list[tuple[str, list[str], str]]]:
    resolved: dict[Pair, set[str]] = defaultdict(set)
    candidate_sets: list[tuple[str, list[str], str]] = []
    for e in graph["edges"]:
        method = e.get("resolution_method") or "unknown"
        if e["resolution"] == "resolved":
            resolved[(e["from"], e["to"])].add(method)
        elif e["resolution"] == "ambiguous":
            candidate_sets.append((e["from"], list(e.get("candidates") or []), method))
    return resolved, candidate_sets


def _qualified_index(graph: dict) -> dict[str, set[str]]:
    out: dict[str, set[str]] = defaultdict(set)
    for n in graph["nodes"]:
        out[n["qualified_name"]].add(n["id"])
    return out


def _apply_adjudications(adjudications, graph, resolved, oracle_pairs, out_of_scope):
    """Correct both sides in place; return verdict counts (one per record) and stale entries.

    A record applies to the pairs the disagreement listing would show: an
    agent-lens-only pair whose caller the oracle can see, or an oracle-only
    pair. A record whose names match no such pair is stale. `caller_file` /
    `callee_file` optionally pin a record to one node when a qualified name is
    shared (build-tagged twins, same-named scripts).
    """
    by_qn = _qualified_index(graph)
    nodes = {n["id"]: n for n in graph["nodes"]}
    applied: Counter = Counter()
    stale: list[dict] = []
    adjudicated: set[Pair] = set()

    def candidates(adj: dict, side: str) -> list[str]:
        pin = adj.get(f"{side}_file")
        return [n for n in by_qn.get(adj[side], ()) if pin is None or nodes[n]["file"] == pin]

    def disagreement(pair: Pair) -> bool:
        if pair in resolved:
            return pair not in oracle_pairs and not out_of_scope(pair[0])
        return pair in oracle_pairs

    for adj in adjudications:
        verdict = adj["verdict"]
        if verdict not in VERDICTS:
            raise ValueError(f"unknown verdict {verdict!r} in {adj}")
        pairs = [(a, b) for a in candidates(adj, "caller") for b in candidates(adj, "callee") if disagreement((a, b))]
        if not pairs:
            stale.append(adj)
            continue
        if len(pairs) > 1:
            print(
                f"score: warning: adjudication {adj['caller']} -> {adj['callee']} matches {len(pairs)} node pairs; "
                "pin it with caller_file / callee_file",
                file=sys.stderr,
            )
        applied[verdict] += 1
        for pair in pairs:
            adjudicated.add(pair)
            if verdict == "oracle_wrong":
                if pair in resolved:
                    oracle_pairs[pair] = "static"
                else:
                    del oracle_pairs[pair]
            elif verdict == "definition_gap":
                resolved.pop(pair, None)
                oracle_pairs.pop(pair, None)
    return applied, stale, adjudicated


def score(graph: dict, oracle: dict, adjudications: list[dict] | None = None, source_root: Path | None = None) -> dict:
    """Return raw counts (summable across targets) plus the disagreement list."""
    if source_root is None and oracle.get("root"):
        source_root = Path(oracle["root"])
    # A call in module-level code (a Rust `const` / `static` initialiser, a
    # TS top-level statement) has no caller node: outside every edge set.
    module_level = sum(e["resolution"] == "resolved" for e in graph["edges"] if e["from"] is None)
    graph = graph | {"edges": [e for e in graph["edges"] if e["from"] is not None]}
    index = NodeIndex(graph, source_root)
    oracle_pairs, oracle_lines, unmapped = _oracle_pairs(oracle, index)
    resolved, candidate_sets = _agent_lens_edges(graph)

    executed: set[str] | None = None
    if oracle.get("kind") == "dynamic":
        executed = set()
        for f in oracle.get("executed_functions", []):
            node_id, _ = index.definition(f["file"], f["def_line"])
            if node_id is not None:
                executed.add(node_id)

    # A type-checker oracle lists the files it type-checked; a caller in any
    # other file (excluded by a build constraint) is out of its scope.
    analyzed: set[str] | None = set(oracle["analyzed_files"]) if "analyzed_files" in oracle else None
    # Functions the oracle saw but could not resolve (cfg-inactive Rust code).
    unanalyzed = {index.definition(f["file"], f["def_line"])[0] for f in oracle.get("unanalyzed_functions", [])}
    unanalyzed.discard(None)
    out_of_scope = _out_of_scope(index.nodes, executed, analyzed, unanalyzed)
    applied, stale, adjudicated = _apply_adjudications(adjudications or [], graph, resolved, oracle_pairs, out_of_scope)

    # Precision side: every agent-lens resolved pair, per method.
    precision: dict[str, Counter] = defaultdict(Counter)
    for pair, methods in resolved.items():
        rows = sorted(methods) + ["overall"]
        reason = out_of_scope(pair[0])
        for m in rows:
            precision[m]["resolved"] += 1
            if reason:
                precision[m][reason] += 1
            elif pair in oracle_pairs:
                precision[m]["tp"] += 1
            else:
                precision[m]["fp"] += 1

    # Recall side: every mapped oracle pair, per dispatch kind.
    in_candidates: set[Pair] = {(src, c) for src, cands, _ in candidate_sets for c in cands}
    # Lines where agent-lens saw a call site of any resolution, per caller: an
    # oracle edge missed on such a line is a resolution miss, one missed
    # elsewhere had no syntactic call site (implicit call, dropped closure body).
    sites: dict[str, set[int]] = defaultdict(set)
    for e in graph["edges"]:
        sites[e["from"]].update(e.get("call_lines") or [])
    recall: dict[str, Counter] = defaultdict(Counter)
    for pair, dispatch in oracle_pairs.items():
        row = recall[dispatch]
        row["oracle"] += 1
        if pair in resolved:
            row["found"] += 1
            for m in resolved[pair]:
                row[f"by_{m}"] += 1
        elif pair in in_candidates:
            row["only_candidate"] += 1
        else:
            row["missed"] += 1
            if not oracle_lines.get(pair, set()) & sites[pair[0]]:
                row["missed_no_site"] += 1

    # Candidate sets (ambiguous edges), per method. A set whose caller the
    # oracle cannot see is counted as excluded, as in precision.
    candidates: dict[str, Counter] = defaultdict(Counter)
    for src, cands, method in candidate_sets:
        if out_of_scope(src):
            for m in (method, "overall"):
                candidates[m]["excluded"] += 1
            continue
        hits = sum((src, c) in oracle_pairs for c in cands)
        for m in (method, "overall"):
            row = candidates[m]
            row["sets"] += 1
            row["size"] += len(cands)
            row["hit_sets"] += hits > 0
            row["hit_candidates"] += hits

    disagreements = _disagreements(graph, index, resolved, oracle_pairs, in_candidates, out_of_scope, adjudicated)
    return {
        "language": oracle.get("language"),
        "oracle": oracle.get("oracle"),
        "kind": oracle.get("kind"),
        "oracle_edges": len(oracle["edges"]),
        "precision": {k: dict(v) for k, v in precision.items()},
        "recall": {k: dict(v) for k, v in recall.items()},
        "candidates": {k: dict(v) for k, v in candidates.items()},
        "unmapped": dict(unmapped),
        "adjudicated": dict(applied),
        "stale_adjudications": len(stale),
        "unadjudicated": len(disagreements),
        "module_level_resolved": module_level,
        "disagreements": disagreements,
    }


def _out_of_scope(nodes: dict, executed: set[str] | None, analyzed: set[str] | None, unanalyzed: set[str] = frozenset()):
    """Return a function giving why a caller node is outside the oracle's view, or None."""

    def reason(node_id: str) -> str | None:
        if analyzed is not None and nodes[node_id]["file"] not in analyzed:
            return "caller_not_analyzed"
        if node_id in unanalyzed:
            return "caller_not_analyzed"
        if executed is not None and node_id not in executed:
            return "caller_not_executed"
        return None

    return reason


def _disagreements(graph, index, resolved, oracle_pairs, in_candidates, out_of_scope, adjudicated):
    nodes = index.nodes
    out = []

    def describe(node_id):
        n = nodes[node_id]
        return {"qualified_name": n["qualified_name"], "location": f"{n['file']}:{n['start_line']}"}

    call_lines = defaultdict(list)
    for e in graph["edges"]:
        if e["resolution"] == "resolved":
            call_lines[(e["from"], e["to"])].extend(e.get("call_lines") or [])

    for pair, methods in sorted(resolved.items()):
        if pair in oracle_pairs or pair in adjudicated:
            continue
        if out_of_scope(pair[0]):
            continue
        out.append({
            "side": "agent_lens_only",
            "caller": describe(pair[0]),
            "callee": describe(pair[1]),
            "method": sorted(methods),
            "call_lines": sorted(set(call_lines[pair])),
        })
    for pair, dispatch in sorted(oracle_pairs.items()):
        if pair in resolved or pair in adjudicated:
            continue
        out.append({
            "side": "oracle_only",
            "caller": describe(pair[0]),
            "callee": describe(pair[1]),
            "dispatch": dispatch,
            "in_candidate_set": pair in in_candidates,
        })
    return out


def toml_snippet(target: str, d: dict) -> str:
    return (
        "[[adjudication]]\n"
        f"target = {json.dumps(target)}\n"
        f"caller = {json.dumps(d['caller']['qualified_name'])}\n"
        f"callee = {json.dumps(d['callee']['qualified_name'])}\n"
        f"# {d['side']}: {d['caller']['location']} -> {d['callee']['location']}\n"
        'verdict = ""  # oracle_wrong | agent_lens_wrong | definition_gap\n'
        'note = ""\n'
    )


def load_adjudications(path: Path, target: str) -> list[dict]:
    data = tomllib.loads(path.read_text())
    return [a for a in data.get("adjudication", []) if a.get("target") == target]


# ---------------------------------------------------------------- merge and render


def merge(results: list[dict]) -> dict:
    """Sum the counts of several `score` results (one language, one oracle kind)."""
    out: dict = {"precision": {}, "recall": {}, "candidates": {}, "unmapped": Counter(), "adjudicated": Counter()}
    for key in ("precision", "recall", "candidates"):
        acc: dict[str, Counter] = defaultdict(Counter)
        for r in results:
            for row, counts in r[key].items():
                acc[row].update(counts)
        out[key] = {k: dict(v) for k, v in acc.items()}
    for r in results:
        out["unmapped"].update(r["unmapped"])
        out["adjudicated"].update(r["adjudicated"])
    out["unmapped"] = dict(out["unmapped"])
    out["adjudicated"] = dict(out["adjudicated"])
    for key in ("oracle_edges", "stale_adjudications", "unadjudicated", "module_level_resolved"):
        out[key] = sum(r.get(key, 0) for r in results)
    first = results[0] if results else {}
    for key in ("language", "oracle", "kind"):
        values = {r.get(key) for r in results}
        out[key] = first.get(key) if len(values) == 1 else "+".join(sorted(map(str, values)))
    return out


def _ratio(num: int, den: int) -> str:
    return f"{num / den:.3f}" if den else "-"


def _method_rows(table: dict) -> list[str]:
    extra = sorted(k for k in table if k not in METHODS and k != "overall")
    return [m for m in METHODS + extra if m in table] + (["overall"] if "overall" in table else [])


def _md_table(header: list[str], rows: list[list]) -> list[str]:
    lines = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    lines += ["| " + " | ".join(str(c) for c in row) + " |" for row in rows]
    return lines


def render(result: dict, title: str) -> str:
    kind = result.get("kind")
    lines = [f"## {title}", "", f"oracle: {result.get('oracle')} ({kind}), language: {result.get('language')}, oracle edges: {result.get('oracle_edges')}", ""]

    p = result["precision"]
    if kind == "dynamic":
        lines += [
            "### Resolved edges vs observed calls (unobserved rate is an indicator, not a precision verdict)",
            "",
        ]
        rows = []
        for m in _method_rows(p):
            c = p[m]
            scope = c.get("tp", 0) + c.get("fp", 0)
            rows.append([m, c.get("resolved", 0), c.get("caller_not_executed", 0), scope, c.get("tp", 0), c.get("fp", 0), _ratio(c.get("fp", 0), scope)])
        lines += _md_table(["method", "resolved", "caller never ran (excluded)", "caller ran", "observed", "unobserved", "unobserved rate"], rows)
    else:
        lines += ["### Precision of resolved edges", ""]
        rows = []
        for m in _method_rows(p):
            c = p[m]
            tp, fp = c.get("tp", 0), c.get("fp", 0)
            rows.append([m, c.get("resolved", 0), c.get("caller_not_analyzed", 0), tp, fp, _ratio(tp, tp + fp)])
        lines += _md_table(["method", "resolved", "caller not type-checked (excluded)", "TP", "FP", "precision"], rows)

    r = result["recall"]
    methods = [m for m in METHODS if any(f"by_{m}" in r.get(d, {}) for d in DISPATCHES)]
    methods += sorted({k[3:] for d in r.values() for k in d if k.startswith("by_") and k[3:] not in METHODS})
    lines += ["", "### Recall over oracle edges", ""]
    rows = []
    for d in DISPATCHES:
        if d not in r:
            continue
        c = r[d]
        rows.append([d, c.get("oracle", 0), c.get("found", 0), *(c.get(f"by_{m}", 0) for m in methods), c.get("only_candidate", 0), c.get("missed", 0), c.get("missed_no_site", 0), _ratio(c.get("found", 0), c.get("oracle", 0))])
    lines += _md_table(["dispatch", "oracle", "found", *(f"via {m}" for m in methods), "only in candidate set", "missed", "of which no call site", "recall"], rows)

    cs = result["candidates"]
    if cs:
        lines += ["", "### Candidate sets (ambiguous edges)", ""]
        rows = []
        for m in _method_rows(cs):
            c = cs[m]
            sets, size = c.get("sets", 0), c.get("size", 0)
            mean = f"{size / sets:.2f}" if sets else "-"
            rows.append([m, sets, c.get("excluded", 0), mean, _ratio(c.get("hit_sets", 0), sets), _ratio(c.get("hit_candidates", 0), size)])
        lines += _md_table(["method", "sets", "caller out of scope (excluded)", "mean size", "hit rate", "candidate precision"], rows)

    lines += ["", "### Unknown / adjudicated", ""]
    rows = [["unmapped oracle edge", reason, n] for reason, n in sorted(result["unmapped"].items())]
    rows += [["adjudicated", v, result["adjudicated"].get(v, 0)] for v in VERDICTS]
    rows += [["stale adjudication", "matches no disagreement", result["stale_adjudications"]]]
    rows += [["unadjudicated disagreement", "agent-lens only + oracle only", result["unadjudicated"]]]
    rows += [["resolved edge excluded", "no caller node (module-level code)", result.get("module_level_resolved", 0)]]
    lines += _md_table(["bucket", "reason", "count"], rows)
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------- CLI


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--graph", required=True, type=Path, help="agent-lens analyze function-graph --format json output")
    ap.add_argument("--oracle", required=True, type=Path, help="oracle JSON (see docs/callgraph-accuracy.md)")
    ap.add_argument("--adjudications", type=Path, help="adjudications.toml")
    ap.add_argument("--target", default="", help="target name the adjudications are filtered by")
    ap.add_argument("--json", type=Path, help="write the counts here")
    ap.add_argument("--disagreements", type=Path, help="write the unadjudicated disagreements here")
    args = ap.parse_args(argv)

    graph = json.loads(args.graph.read_text())
    oracle = json.loads(args.oracle.read_text())
    adjudications = load_adjudications(args.adjudications, args.target) if args.adjudications else []
    result = score(graph, oracle, adjudications)

    disagreements = result.pop("disagreements")
    if args.json:
        args.json.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    if args.disagreements:
        for d in disagreements:
            d["toml"] = toml_snippet(args.target, d)
        args.disagreements.write_text(json.dumps(disagreements, indent=2) + "\n")
    sys.stdout.write(render(result, args.target or args.graph.name))
    return 0


if __name__ == "__main__":
    sys.exit(main())
