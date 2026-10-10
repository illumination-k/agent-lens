"""Unit tests for the parts of run.py that do not need the network or the tools."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

import run


class CountTest(unittest.TestCase):
    def test_agent_lens_counts_functions_scored_above_zero(self):
        report = {"files": [{"functions": [{"cognitive": 0}, {"cognitive": 3}]}, {"functions": [{"cognitive": 1}]}]}
        self.assertEqual(run.count_agent_lens(report), 2)

    def test_eslint_counts_only_cognitive_complexity_messages(self):
        report = [
            {"messages": [{"ruleId": "sonarjs/cognitive-complexity"}, {"ruleId": None}]},
            {"messages": [{"ruleId": "sonarjs/cognitive-complexity"}]},
        ]
        self.assertEqual(run.count_eslint(report), 2)

    def test_gocognit_lists_only_scored_functions(self):
        self.assertEqual(run.count_gocognit([{"Complexity": 1}, {"Complexity": 4}]), 2)

    def test_complexipy_counts_functions_scored_above_zero(self):
        self.assertEqual(run.count_complexipy([{"complexity": 0}, {"complexity": 2}]), 1)


class KeepOnlyTest(unittest.TestCase):
    def test_trims_the_tree_to_the_language_both_tools_read(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for rel in ["a.go", "a_test.go", "doc.md", "sub/b.go", "testdata/c.go", "sub/testdata/d.go"]:
                path = root / rel
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("package p\n")
            run.keep_only(root, ".go", {"testdata"}, lambda n: n.endswith("_test.go"))
            left = sorted(str(p.relative_to(root)) for p in root.rglob("*") if p.is_file())
            self.assertEqual(left, ["a.go", "sub/b.go"])
            self.assertEqual(run.source_stats(root, ".go"), (2, 2))


class RenderTest(unittest.TestCase):
    def test_ratio_is_against_agent_lens_on_the_same_corpus(self):
        common = {"language": "go", "corpus": "cobra", "files": 19, "lines": 6564, "baseline": 0.05}
        rows = [
            common | {"tool": "agent-lens", "mean": 0.05, "stddev": 0.001, "scored": 148},
            common | {"tool": "gocognit", "mean": 0.025, "stddev": 0.0005, "scored": 148},
        ]
        table = run.render(rows)
        self.assertIn("| go | cobra | 19 | 6564 | agent-lens | 50.0 ms ± 1.0 ms | 1.0× | 148 |", table)
        self.assertIn("| gocognit | 25.0 ms ± 0.5 ms | 0.5× | 148 |", table)

    def test_milliseconds_drop_the_decimal_from_a_tenth_of_a_second(self):
        self.assertEqual(run.fmt_ms(0.0234), "23.4 ms")
        self.assertEqual(run.fmt_ms(13.5), "13,500 ms")


if __name__ == "__main__":
    unittest.main()
