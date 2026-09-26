"""Unit tests for score.py: `python3 -m unittest discover -s scripts/callgraph-accuracy`."""

from __future__ import annotations

import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import score  # noqa: E402


def node(name: str, start: int, end: int, file: str = "m.py") -> dict:
    return {"id": f"{file}:{name}:{start}", "name": name.split("::")[-1], "qualified_name": f"m::{name}", "file": file, "start_line": start, "end_line": end}


def resolved(a: dict, b: dict, method: str, line: int = 0) -> dict:
    return {"from": a["id"], "to": b["id"], "resolution": "resolved", "resolution_method": method, "call_lines": [line]}


def edge(caller: dict, call_line: int, callee: dict | int, dispatch: str = "static", file: str = "m.py") -> dict:
    callee_line = callee if isinstance(callee, int) else callee["start_line"]
    return {"caller_file": caller["file"], "caller_def_line": caller["start_line"], "call_line": call_line, "callee_file": file, "callee_def_line": callee_line, "dispatch": dispatch}


# m.py
#  1 def helper():          helper  1-2
#  3 def other():           other   3-4
#  5 def main():            main    5-12
#  7     def inner(): ...   (no node, nested)
# 14 class A: __init__      A::__init__ 14-15, A::go 16-17
HELPER = node("helper", 1, 2)
OTHER = node("other", 3, 4)
MAIN = node("main", 5, 12)
INIT = node("A::__init__", 14, 15)
GO = node("A::go", 16, 17)
NODES = [HELPER, OTHER, MAIN, INIT, GO]


def graph(edges: list[dict]) -> dict:
    return {"nodes": NODES, "edges": edges}


def oracle(edges: list[dict], kind: str = "type-checker", executed: list[dict] | None = None) -> dict:
    o = {"oracle": "go-vta" if kind != "dynamic" else "python-setprofile", "language": "go", "kind": kind, "root": "/nonexistent", "edges": edges}
    if executed is not None:
        o["executed_functions"] = [{"file": n["file"], "def_line": n["start_line"]} for n in executed]
    return o


class PrecisionRecallTest(unittest.TestCase):
    def setUp(self):
        self.graph = graph([
            resolved(MAIN, HELPER, "lexical", 6),  # TP
            resolved(MAIN, OTHER, "last_segment", 8),  # FP
            resolved(INIT, GO, "self_method", 15),  # TP
            {"from": MAIN["id"], "to": None, "resolution": "ambiguous", "candidates": [INIT["id"], GO["id"]], "resolution_method": "crate_narrowed"},
            {"from": MAIN["id"], "to": None, "resolution": "unresolved"},
        ])
        self.oracle = oracle([
            edge(MAIN, 6, HELPER),
            edge(MAIN, 6, HELPER),  # duplicate collapses to one pair
            edge(INIT, 15, GO),
            edge(MAIN, 9, GO, "dynamic"),  # only in the candidate set
            edge(OTHER, 4, HELPER),  # missed
            edge(OTHER, 4, HELPER, "callback"),  # same pair: counted once, as static
        ])
        self.result = score.score(self.graph, self.oracle)

    def test_precision_per_method(self):
        p = self.result["precision"]
        self.assertEqual(p["lexical"], {"resolved": 1, "tp": 1})
        self.assertEqual(p["last_segment"], {"resolved": 1, "fp": 1})
        self.assertEqual(p["overall"], {"resolved": 3, "tp": 2, "fp": 1})

    def test_recall_per_dispatch(self):
        r = self.result["recall"]
        self.assertEqual(r["static"], {"oracle": 3, "found": 2, "by_lexical": 1, "by_self_method": 1, "missed": 1, "missed_no_site": 1})
        self.assertEqual(r["dynamic"], {"oracle": 1, "only_candidate": 1})
        self.assertNotIn("callback", r)

    def test_candidate_sets(self):
        c = self.result["candidates"]["crate_narrowed"]
        self.assertEqual(c, {"sets": 1, "size": 2, "hit_sets": 1, "hit_candidates": 1})

    def test_disagreements(self):
        sides = sorted((d["side"], d["caller"]["qualified_name"], d["callee"]["qualified_name"]) for d in self.result["disagreements"])
        self.assertEqual(sides, [
            ("agent_lens_only", "m::main", "m::other"),
            ("oracle_only", "m::main", "m::A::go"),
            ("oracle_only", "m::other", "m::helper"),
        ])
        self.assertEqual(self.result["unadjudicated"], 3)

    def test_render_mentions_every_table(self):
        md = score.render(self.result, "t")
        for heading in ("Precision of resolved", "Recall over oracle edges", "Candidate sets", "Unknown / adjudicated"):
            self.assertIn(heading, md)
        self.assertIn("| overall | 3 | 0 | 2 | 1 | 0.667 |", md)


class MissedCallSiteTest(unittest.TestCase):
    def test_missed_on_an_unresolved_call_site_is_not_a_missing_site(self):
        g = graph([{"from": OTHER["id"], "to": None, "resolution": "unresolved", "call_lines": [4]}])
        r = score.score(g, oracle([edge(OTHER, 4, HELPER), edge(MAIN, 9, HELPER)]))
        self.assertEqual(r["recall"]["static"], {"oracle": 2, "missed": 2, "missed_no_site": 1})


class MappingTest(unittest.TestCase):
    def test_unmapped_reasons(self):
        g = graph([])
        o = oracle([
            edge(MAIN, 30, HELPER) | {"caller_def_line": 30},  # caller outside any node
            edge(MAIN, 6, 7),  # callee def line inside main: a nested def
            edge(MAIN, 6, HELPER) | {"callee_file": "gone.py"},
        ])
        r = score.score(g, o)
        self.assertEqual(r["unmapped"], {"caller_no_enclosing_node": 1, "callee_nested_or_anonymous": 1, "callee_file_not_in_graph": 1})
        self.assertEqual(r["recall"], {})

    def test_call_inside_nested_def_maps_to_enclosing_node(self):
        r = score.score(graph([resolved(MAIN, HELPER, "lexical", 8)]), oracle([edge(MAIN, 8, HELPER) | {"caller_def_line": 7}]))
        self.assertEqual(r["precision"]["overall"], {"resolved": 1, "tp": 1})

    def test_decorated_callee_maps_through_decorators(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "m.py").write_text("@decorator\n@other(1)\ndef f():\n    pass\n\ndef g():\n    f()\n")
            f, g = node("f", 1, 4), node("g", 6, 7)
            gr = {"nodes": [f, g], "edges": [resolved(g, f, "lexical", 7)]}
            o = oracle([edge(g, 7, 3)]) | {"root": tmp}
            r = score.score(gr, o)
        self.assertEqual(r["precision"]["overall"], {"resolved": 1, "tp": 1})

    def test_same_line_nodes_prefer_exact_start(self):
        a, b = node("a", 3, 3, "x.rs"), node("b", 3, 3, "x.rs")
        index = score.NodeIndex({"nodes": [a, b]}, None)
        self.assertEqual(index.innermost("x.rs", 3), (None, "ambiguous_node"))


class DynamicOracleTest(unittest.TestCase):
    def test_unobserved_rate_only_over_executed_callers(self):
        g = graph([
            resolved(MAIN, HELPER, "lexical"),  # observed
            resolved(MAIN, OTHER, "lexical"),  # caller ran, never observed
            resolved(INIT, GO, "self_method"),  # caller never ran: excluded
        ])
        o = oracle([edge(MAIN, 6, HELPER)], kind="dynamic", executed=[MAIN, HELPER])
        r = score.score(g, o)
        self.assertEqual(r["precision"]["overall"], {"resolved": 3, "tp": 1, "fp": 1, "caller_not_executed": 1})
        self.assertEqual([d["callee"]["qualified_name"] for d in r["disagreements"]], ["m::other"])
        md = score.render(r, "t")
        self.assertIn("unobserved rate", md)
        self.assertNotIn("Precision of resolved", md)


    def test_candidate_sets_only_over_executed_callers(self):
        g = graph([
            {"from": MAIN["id"], "to": None, "resolution": "ambiguous", "candidates": [INIT["id"], GO["id"]], "resolution_method": "last_segment"},
            {"from": OTHER["id"], "to": None, "resolution": "ambiguous", "candidates": [HELPER["id"], GO["id"]], "resolution_method": "last_segment"},
        ])
        o = oracle([edge(OTHER, 4, HELPER)], kind="dynamic", executed=[OTHER, HELPER])
        r = score.score(g, o)
        self.assertEqual(r["candidates"]["overall"], {"sets": 1, "size": 2, "hit_sets": 1, "hit_candidates": 1, "excluded": 1})
        self.assertIn("| overall | 1 | 1 | 2.00 | 1.000 | 0.500 |", score.render(r, "t"))


class AnalyzedFilesTest(unittest.TestCase):
    def test_callers_in_files_the_oracle_did_not_type_check_are_excluded(self):
        debug = node("debug", 1, 3, "debug.go")
        g = {"nodes": NODES + [debug], "edges": [resolved(MAIN, HELPER, "lexical"), resolved(debug, HELPER, "lexical")]}
        o = oracle([edge(MAIN, 6, HELPER)]) | {"analyzed_files": ["m.py"]}
        r = score.score(g, o)
        self.assertEqual(r["precision"]["lexical"], {"resolved": 2, "tp": 1, "caller_not_analyzed": 1})
        self.assertEqual(r["disagreements"], [])
        self.assertIn("| lexical | 2 | 1 | 1 | 0 | 1.000 |", score.render(r, "t"))


class AdjudicationTest(unittest.TestCase):
    def setUp(self):
        self.graph = graph([resolved(MAIN, OTHER, "last_segment"), resolved(MAIN, HELPER, "lexical")])
        self.oracle = oracle([edge(MAIN, 6, HELPER), edge(OTHER, 4, HELPER)])

    def run_with(self, *adjs):
        return score.score(self.graph, self.oracle, [dict(a) for a in adjs])

    def test_oracle_wrong_on_agent_lens_only_edge_becomes_tp(self):
        r = self.run_with({"caller": "m::main", "callee": "m::other", "verdict": "oracle_wrong"})
        self.assertEqual(r["precision"]["overall"], {"resolved": 2, "tp": 2})
        self.assertEqual(r["adjudicated"], {"oracle_wrong": 1})

    def test_oracle_wrong_on_oracle_only_edge_is_removed(self):
        r = self.run_with({"caller": "m::other", "callee": "m::helper", "verdict": "oracle_wrong"})
        self.assertEqual(r["recall"]["static"], {"oracle": 1, "found": 1, "by_lexical": 1})

    def test_definition_gap_excludes_from_both_sides(self):
        r = self.run_with(
            {"caller": "m::main", "callee": "m::other", "verdict": "definition_gap"},
            {"caller": "m::other", "callee": "m::helper", "verdict": "definition_gap"},
        )
        self.assertEqual(r["precision"]["overall"], {"resolved": 1, "tp": 1})
        self.assertEqual(r["recall"]["static"]["oracle"], 1)
        self.assertEqual(r["adjudicated"], {"definition_gap": 2})
        self.assertEqual(r["unadjudicated"], 0)

    def test_agent_lens_wrong_is_counted_without_changing_numbers(self):
        r = self.run_with({"caller": "m::main", "callee": "m::other", "verdict": "agent_lens_wrong"})
        self.assertEqual(r["precision"]["overall"], {"resolved": 2, "tp": 1, "fp": 1})
        self.assertEqual(r["adjudicated"], {"agent_lens_wrong": 1})
        self.assertEqual(r["unadjudicated"], 1)

    def test_stale_adjudication_is_reported(self):
        r = self.run_with({"caller": "m::main", "callee": "m::helper", "verdict": "oracle_wrong"})
        self.assertEqual(r["stale_adjudications"], 1)
        self.assertEqual(r["adjudicated"], {})

    def test_adjudication_on_out_of_scope_caller_is_stale(self):
        # MAIN never ran under the dynamic oracle, so main -> other is never
        # listed as a disagreement and a record on it must not apply.
        g = graph([resolved(MAIN, OTHER, "last_segment", 8)])
        o = oracle([edge(INIT, 15, GO)], kind="dynamic", executed=[INIT, GO])
        r = score.score(g, o, [{"caller": "m::main", "callee": "m::other", "verdict": "oracle_wrong"}])
        self.assertEqual(r["recall"]["static"], {"oracle": 1, "missed": 1, "missed_no_site": 1})
        self.assertEqual(r["precision"]["overall"], {"resolved": 1, "caller_not_executed": 1})
        self.assertEqual(r["adjudicated"], {})
        self.assertEqual(r["stale_adjudications"], 1)

    def test_shared_qualified_name_counts_once_and_can_be_pinned(self):
        a, b = node("dup", 1, 2, "a.go"), node("dup", 1, 2, "b.go")
        for n in (a, b):
            n["qualified_name"] = "p::dup"
        g = {"nodes": NODES + [a, b], "edges": [resolved(MAIN, a, "lexical"), resolved(MAIN, b, "lexical")]}
        o = oracle([edge(MAIN, 6, HELPER)])
        warn = io.StringIO()
        with redirect_stderr(warn):
            r = score.score(g, o, [{"caller": "m::main", "callee": "p::dup", "verdict": "definition_gap"}])
        self.assertIn("matches 2 node pairs", warn.getvalue())
        self.assertEqual(r["adjudicated"], {"definition_gap": 1})
        self.assertNotIn("overall", r["precision"])  # both namesakes removed
        self.assertEqual(r["unadjudicated"], 1)  # main -> helper, oracle only
        pinned = score.score(g, o, [{"caller": "m::main", "callee": "p::dup", "callee_file": "a.go", "verdict": "definition_gap"}])
        self.assertEqual(pinned["adjudicated"], {"definition_gap": 1})
        self.assertEqual([d["callee"]["location"] for d in pinned["disagreements"] if d["side"] == "agent_lens_only"], ["b.go:1"])

    def test_unknown_verdict_is_rejected(self):
        with self.assertRaises(ValueError):
            self.run_with({"caller": "m::main", "callee": "m::other", "verdict": "maybe"})


class CliTest(unittest.TestCase):
    def test_cli_writes_json_disagreements_and_markdown(self):
        with tempfile.TemporaryDirectory() as tmp:
            t = Path(tmp)
            (t / "g.json").write_text(json.dumps(graph([resolved(MAIN, OTHER, "last_segment")])))
            (t / "o.json").write_text(json.dumps(oracle([edge(OTHER, 4, HELPER)])))
            (t / "adj.toml").write_text('[[adjudication]]\ntarget = "other"\ncaller = "m::main"\ncallee = "m::other"\nverdict = "oracle_wrong"\n')
            out = io.StringIO()
            with redirect_stdout(out):
                score.main(["--graph", str(t / "g.json"), "--oracle", str(t / "o.json"), "--adjudications", str(t / "adj.toml"), "--target", "t1", "--json", str(t / "r.json"), "--disagreements", str(t / "d.json")])
            result = json.loads((t / "r.json").read_text())
            disagreements = json.loads((t / "d.json").read_text())
        # The adjudication belongs to another target, so it does not apply.
        self.assertEqual(result["adjudicated"], {})
        self.assertEqual(len(disagreements), 2)
        self.assertIn('caller = "m::main"', disagreements[0]["toml"])
        self.assertIn('target = "t1"', disagreements[0]["toml"])
        self.assertIn("## t1", out.getvalue())

    def test_merge_sums_counts(self):
        r = score.score(graph([resolved(MAIN, HELPER, "lexical")]), oracle([edge(MAIN, 6, HELPER)]))
        r.pop("disagreements")
        m = score.merge([r, r])
        self.assertEqual(m["precision"]["overall"], {"resolved": 2, "tp": 2})
        self.assertEqual(m["oracle_edges"], 2)
        self.assertEqual(m["kind"], "type-checker")


class AdjudicationFileTest(unittest.TestCase):
    def test_committed_adjudications_parse(self):
        path = Path(__file__).resolve().parent / "adjudications.toml"
        data = score.tomllib.loads(path.read_text())
        for adj in data.get("adjudication", []):
            self.assertIn(adj["verdict"], score.VERDICTS)
            for key in ("target", "caller", "callee", "note"):
                self.assertTrue(adj.get(key), f"{key} missing in {adj}")


if __name__ == "__main__":
    unittest.main()
