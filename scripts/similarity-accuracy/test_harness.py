"""Unit tests for the similarity-accuracy harness:
`python3 -m unittest discover -s scripts/similarity-accuracy`."""

from __future__ import annotations

import ast
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import corpus  # noqa: E402
import gptclonebench  # noqa: E402
import run  # noqa: E402
import score  # noqa: E402

FAMILY = """\
@@ rename values=xs total=acc
@@ orig
def subject(values):
    total = 0
    for v in values:
        total += v
    return total
@@ t3-insert
def subject(values):
    total = 0
    for v in values:
        total += v
    print(total)
    return total
@@ t3-modify
def subject(values):
    total = 1
    for v in values:
        total *= v
    return total
@@ t4
def subject(values):
    return sum(values)
@@ hard
def subject(words):
    n = 0
    for w in words:
        n += len(w)
    return n
"""


class RocAucTest(unittest.TestCase):
    def test_perfect_separation(self):
        self.assertEqual(score.roc_auc([0.9, 0.8], [0.1, 0.2]), 1.0)

    def test_inverted_separation(self):
        self.assertEqual(score.roc_auc([0.1], [0.9, 0.8]), 0.0)

    def test_ties_count_half(self):
        self.assertEqual(score.roc_auc([0.5, 0.5], [0.5]), 0.5)

    def test_matches_pairwise_definition(self):
        pos, neg = [0.3, 0.7, 0.7, 0.9], [0.1, 0.7, 0.8]
        pairwise = sum(1.0 if p > n else 0.5 if p == n else 0.0 for p in pos for n in neg) / (len(pos) * len(neg))
        self.assertAlmostEqual(score.roc_auc(pos, neg), pairwise)

    def test_empty_side_is_none(self):
        self.assertIsNone(score.roc_auc([], [0.1]))
        self.assertIsNone(score.roc_auc([0.1], []))


class ThresholdMetricsTest(unittest.TestCase):
    def test_rate_at_is_inclusive(self):
        self.assertEqual(score.rate_at([0.85, 0.84, 0.9, 0.1], 0.85), 0.5)

    def test_best_f1_picks_separating_cut(self):
        f1, threshold = score.best_f1([0.9, 0.8], [0.5, 0.1])
        self.assertEqual((f1, threshold), (1.0, 0.8))

    def test_summarize_keeps_hard_negatives_out_of_the_pooled_auc(self):
        pairs = [
            {"label": True, "kind": "t1", "score": 0.9},
            {"label": True, "kind": "t4", "score": 0.4},
            {"label": False, "kind": "cross", "score": 0.1},
            {"label": False, "kind": "hard", "score": 0.95},
        ]
        s = score.summarize(pairs)
        self.assertEqual(s["auc"]["all"], 1.0)
        self.assertEqual(s["auc"]["vs_hard"], 0.0)
        self.assertEqual(s["fpr"]["hard"]["0.85"], 1.0)
        self.assertEqual(s["recall"]["t4"]["0.85"], 0.0)


class CorpusTest(unittest.TestCase):
    def test_parse_family(self):
        variants, rename = corpus.parse_family(FAMILY)
        self.assertEqual(rename, {"values": "xs", "total": "acc"})
        self.assertEqual(sorted(variants), sorted(corpus.HAND_WRITTEN))
        self.assertTrue(variants["t4"].startswith("def subject(values):"))

    def test_missing_variant_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "missing variants: hard"):
            corpus.parse_family(FAMILY.split("@@ hard")[0])

    def test_type1_only_adds_a_comment_and_a_blank_line(self):
        variants, _ = corpus.parse_family(FAMILY)
        t1 = corpus.type1(variants["orig"], "#")
        self.assertEqual(ast.dump(ast.parse(t1)), ast.dump(ast.parse(variants["orig"])))
        self.assertIn("    # copied from the original", t1)

    def test_type2_renames_whole_words_only(self):
        t2 = corpus.type2("total = values + totals\n", {"total": "acc", "values": "xs"})
        self.assertEqual(t2, "acc = xs + totals\n")

    def test_pairs_are_labelled_by_construction(self):
        families = {"a": None, "b": None}
        for name in families:
            variants, rename = corpus.parse_family(FAMILY)
            variants["t1"] = corpus.type1(variants["orig"], "#")
            variants["t2"] = corpus.type2(variants["orig"], rename)
            families[name] = variants
        pairs = corpus.build_pairs("python", families)
        kinds = [(p.kind, p.label) for p in pairs]
        self.assertEqual(kinds.count(("t3", True)), 4)
        self.assertEqual(kinds.count(("hard", False)), 2)
        n = len(corpus.NEGATIVE_VARIANTS)
        self.assertEqual(kinds.count(("cross", False)), n * n)
        for p in pairs:
            if p.label or p.kind == "hard":
                self.assertEqual(p.a_id.split("/")[0], p.b_id.split("/")[0])
            else:
                self.assertNotEqual(p.a_id.split("/")[0], p.b_id.split("/")[0])

    def test_committed_corpus_loads(self):
        for lang in corpus.LANGUAGES:
            with self.subTest(lang=lang):
                families = corpus.load_language(lang)
                self.assertGreaterEqual(len(families), 2)
                for name, variants in families.items():
                    self.assertEqual(set(variants), {*corpus.HAND_WRITTEN, "t1", "t2"})
                    for variant, source in variants.items():
                        signature = source.split("\n", 1)[0]
                        self.assertRegex(signature, rf"\b{corpus.PLACEHOLDER}\b", f"{lang}/{name}/{variant}")

    def test_committed_python_corpus_parses(self):
        for name, variants in corpus.load_language("python").items():
            for variant, source in variants.items():
                with self.subTest(family=name, variant=variant):
                    ast.parse(source)


class GptCloneBenchTest(unittest.TestCase):
    def test_split_pair_dedents_and_renames(self):
        text = "def foo(a) :\n\treturn foo(a - 1)\n\n\n def foo(b):\n  return b\n"
        a, b = gptclonebench.split_pair(text)
        self.assertEqual(a, "def subject(a) :\n\treturn subject(a - 1)\n")
        self.assertEqual(b, "def subject(b):\n return b\n")

    def test_split_pair_keeps_nested_defs(self):
        text = "def f():\n    def g():\n        pass\n    return g\n\ndef f():\n    return 1\n"
        a, _ = gptclonebench.split_pair(text)
        self.assertIn("def g()", a)

    def test_split_pair_rejects_other_shapes_and_invalid_python(self):
        self.assertIsNone(gptclonebench.split_pair("def f():\n    pass\n"))
        self.assertIsNone(gptclonebench.split_pair("def f():\n    if a ! = b:\n        pass\n\ndef f():\n    pass\n"))

    def test_load_and_sample(self):
        pair = "def f(x):\n    return x + {n}\n\ndef f(y):\n    return y + {n}\n"
        with tempfile.TemporaryDirectory() as tmp:
            archive = Path(tmp) / "a.zip"
            with zipfile.ZipFile(archive, "w") as z:
                for i in range(4):
                    z.writestr(f"standalone/true_semantic_clones/py/prompt_1/MT3/Clone_{i}.py", pair.format(n=i))
                    z.writestr(f"standalone/true_semantic_clones/py/prompt_2/T4/Clone_{i}.py", pair.format(n=10 + i))
                    z.writestr(f"standalone/false_semantic_clones/py/Gpt_false_pair_{i}.py", pair.format(n=20 + i))
                z.writestr("standalone/true_semantic_clones/java/prompt_1/MT3/Clone_0.java", "class A {}")
                z.writestr("standalone/false_semantic_clones/py/Gpt_false_pair_9.py", "def only():\n    pass\n")
            groups, skipped = gptclonebench.load(archive)
        self.assertEqual({k: len(v) for k, v in groups.items()}, {"mt3": 4, "t4": 4, "gcb_false": 4})
        self.assertEqual(skipped, 1)
        pairs = gptclonebench.build_pairs(groups, per_kind=2, seed=1)
        kinds = [p.kind for p in pairs]
        self.assertEqual({k: kinds.count(k) for k in set(kinds)}, {"mt3": 2, "t4": 2, "gcb_false": 2, "cross": 4})
        for p in pairs:
            self.assertEqual(p.label, p.kind in ("mt3", "t4"))
            if p.kind == "cross":
                self.assertNotEqual(p.a_id, p.b_id)


class RenderTest(unittest.TestCase):
    def rows(self):
        def row(lang, label, kind, value, a="x/orig", b="x/v"):
            return {"lang": lang, "method": "tsed", "label": label, "kind": kind, "a": a, "b": b, "score": value, "reported": value > 0}

        return [
            row("go", True, "t1", 1.0),
            row("go", True, "t4", 0.3),
            row("go", False, "cross", 0.2),
            row("go", False, "hard", 0.9),
            row("rust", True, "t1", 0.95),
            row("rust", False, "cross", 0.0),
            row("rust", False, "gcb_false", 1.0, a="same", b="same"),
        ]

    def test_report_has_per_language_and_pooled_rows(self):
        report, summary = run.render("t", self.rows(), ["tsed"], ["note"])
        self.assertEqual(set(summary), {"go/tsed", "rust/tsed", "all/tsed"})
        self.assertIn("| all | tsed |", report)
        self.assertIn("reported no score for (scored 0): 1 of 7", report)
        self.assertIn("| tsed | 0.85 |", report)

    def test_worst_cases_skip_sanity_rows(self):
        report, _ = run.render("t", self.rows(), ["tsed"], [])
        worst = report.split("Highest-scoring negatives")[1]
        self.assertIn("0.90 go hard", worst)
        self.assertNotIn("gcb_false", worst)

    def test_materialize_renames_per_pair(self):
        pairs = [corpus.Pair("go", "func subject() {}\n", "func subject() { subject() }\n", True, "t1", "a", "b")]
        with tempfile.TemporaryDirectory() as tmp:
            dirs = run.materialize(pairs, Path(tmp) / "work")
            b = (dirs["go"] / "p0" / "b.go").read_text()
        self.assertEqual(b, "package corpus\n\nfunc pair_0() { pair_0() }\n")


if __name__ == "__main__":
    unittest.main()
