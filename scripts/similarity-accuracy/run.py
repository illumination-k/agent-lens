#!/usr/bin/env python3
"""Measure `agent-lens analyze similarity` against labelled clone pairs.

    run.py [synthetic] [gptclonebench] [--methods tsed,token,lcs,pdg]
           [--per-kind N] [--seed S] [--out DIR]

`synthetic` (the default) is the committed corpus under corpus/; it needs no
network. `gptclonebench` fetches a third-party dataset and is a reference
only. Every pair is written as two files whose function shares a name unique
to the pair, and scored with `--paired-by qualified --drift-floor 0`, which
reports the similarity of every name match whatever its score. Metrics and
their meaning: docs/similarity-accuracy.md.

Set AGENT_LENS_BIN to score a binary other than target/release/agent-lens.
Standard library only (Python >= 3.11).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import corpus  # noqa: E402
import gptclonebench  # noqa: E402
import score  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
METHODS = ["tsed", "token", "lcs", "pdg"]
SUITES = ["synthetic", "gptclonebench"]
POSITIVE_ORDER = ["t1", "t2", "t3", "mt3", "t4"]
# Negative kinds listed under "worst cases"; a sanity row such as
# gptclonebench's `gcb_false` holds near-copies and would crowd them out.
TRUE_NEGATIVES = ["cross", "hard"]
_PAIR_KEY = re.compile(r"(\d+)$")


def materialize(pairs: list[corpus.Pair], work: Path) -> dict[str, Path]:
    """Write pair i as <lang>/p<i>/{a,b}.<ext>, function renamed to pair_<i>."""
    if work.exists():
        shutil.rmtree(work)
    dirs = {}
    for i, pair in enumerate(pairs):
        spec = corpus.LANGUAGES[pair.lang]
        d = work / pair.lang / f"p{i}"
        d.mkdir(parents=True)
        name = f"pair_{i}"
        for side, source in (("a", pair.a), ("b", pair.b)):
            renamed = re.sub(rf"\b{corpus.PLACEHOLDER}\b", name, source)
            (d / f"{side}.{spec['ext']}").write_text(spec["preamble"] + renamed)
        dirs[pair.lang] = work / pair.lang
    return dirs


def similarity(binary: Path, root: Path, method: str) -> dict[int, float]:
    """{pair index: similarity} for every name-matched pair agent-lens reports."""
    cmd = [str(binary), "analyze", "similarity", str(root), "--paired-by", "qualified", "--drift-floor", "0", "--min-lines", "1", "--method", method, "--format", "json"]
    proc = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed:\n{proc.stderr}")
    out = {}
    for group in json.loads(proc.stdout)["groups"]:
        key = _PAIR_KEY.search(group["key"])
        if key and group["pairs"]:
            out[int(key.group(1))] = group["pairs"][0]["similarity"]
    return out


def score_suite(binary: Path, pairs: list[corpus.Pair], work: Path, methods: list[str]) -> list[dict]:
    dirs = materialize(pairs, work)
    rows = []
    for method in methods:
        for lang, root in sorted(dirs.items()):
            scores = similarity(binary, root, method)
            for i, pair in enumerate(pairs):
                if pair.lang != lang:
                    continue
                rows.append({"lang": lang, "method": method, "label": pair.label, "kind": pair.kind, "a": pair.a_id, "b": pair.b_id, "score": scores.get(i, 0.0), "reported": i in scores})
    return rows


def _case(row: dict) -> str:
    ids = f"`{row['a']}`" if row["a"] == row["b"] else f"`{row['a']}` ~ `{row['b']}`"
    return f"- {row['score']:.2f} {row['lang']} {row['kind']} {ids}"


def _order(kinds: set[str]) -> list[str]:
    return [k for k in POSITIVE_ORDER if k in kinds] + sorted(kinds - set(POSITIVE_ORDER))


def render(title: str, rows: list[dict], methods: list[str], notes: list[str]) -> tuple[str, dict]:
    langs = sorted({r["lang"] for r in rows})
    cells = {}
    for lang in [*langs, "all"] if len(langs) > 1 else langs:
        for method in methods:
            subset = [r for r in rows if r["method"] == method and lang in ("all", r["lang"])]
            cells[(lang, method)] = score.summarize(subset)
    pos_kinds = _order({r["kind"] for r in rows if r["label"]})
    neg_kinds = sorted({r["kind"] for r in rows if not r["label"]})
    extra = [f"vs_{k}" for k in neg_kinds if k != "cross"]

    out = [f"# Similarity accuracy: {title}", ""]
    out += [f"- {n}" for n in notes] + [""]
    counts = cells[(langs[0] if len(langs) == 1 else "all", methods[0])]["counts"]
    out += ["Pairs: " + ", ".join(f"{k} {v}" for k, v in counts.items()), ""]
    missing = sum(not r["reported"] for r in rows)
    if missing:
        out += [f"Pairs agent-lens reported no score for (scored 0): {missing} of {len(rows)} (method x pair)", ""]

    out += ["## ROC-AUC", "", "Positives of each type against `cross` negatives; `all` pools the positive types; `vs_<kind>` is every positive against that negative kind alone.", ""]
    head = ["lang", "method", *pos_kinds, "all", *extra, "best F1 @ thr"]
    out += ["| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
    for (lang, method), c in cells.items():
        f1 = c["best_f1"]
        best = "-" if f1 is None else f"{f1['f1']:.2f} @ {f1['threshold']:.2f}"
        vals = [score.fmt(c["auc"].get(k)) for k in [*pos_kinds, "all", *extra]]
        out.append("| " + " | ".join([lang, method, *vals, best]) + " |")

    scope = "all" if len(langs) > 1 else langs[0]
    out += ["", f"## Recall and false-positive rate by threshold ({scope})", "", "Share of pairs scoring at or above the threshold: recall for a positive type, false-positive rate for a negative kind.", ""]
    head = ["method", "threshold", *pos_kinds, *(f"FPR {k}" for k in neg_kinds)]
    out += ["| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
    for method in methods:
        c = cells[(scope, method)]
        for t in score.THRESHOLDS:
            vals = [score.fmt(c["recall"][k][str(t)]) for k in pos_kinds] + [score.fmt(c["fpr"][k][str(t)]) for k in neg_kinds]
            out.append("| " + " | ".join([method, f"{t:.2f}", *vals]) + " |")

    out += ["", "## Worst cases (tsed)" if "tsed" in methods else "## Worst cases", ""]
    method = "tsed" if "tsed" in methods else methods[0]
    sub = [r for r in rows if r["method"] == method]
    low = sorted((r for r in sub if r["label"]), key=lambda r: r["score"])[:8]
    high = sorted((r for r in sub if r["kind"] in TRUE_NEGATIVES), key=lambda r: -r["score"])[:8]
    out += ["Lowest-scoring positives:", ""] + [_case(r) for r in low]
    out += ["", "Highest-scoring negatives (" + ", ".join(k for k in TRUE_NEGATIVES if k in neg_kinds) + "):", ""] + [_case(r) for r in high]
    summary = {f"{lang}/{method}": c for (lang, method), c in cells.items()}
    return "\n".join(out) + "\n", summary


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("suites", nargs="*", metavar="suite", help="synthetic (default) and/or gptclonebench")
    ap.add_argument("--methods", default=",".join(METHODS))
    ap.add_argument("--per-kind", type=int, default=500, help="gptclonebench: pairs sampled per kind")
    ap.add_argument("--seed", type=int, default=579)
    ap.add_argument("--out", type=Path, default=ROOT / "target" / "similarity-accuracy")
    args = ap.parse_args(argv)
    binary = Path(os.environ.get("AGENT_LENS_BIN", ROOT / "target" / "release" / "agent-lens"))
    if not binary.exists():
        print(f"agent-lens binary not found at {binary}; build it or set AGENT_LENS_BIN", file=sys.stderr)
        return 2
    methods = [m for m in args.methods.split(",") if m]
    unknown = sorted(set(args.suites) - set(SUITES)) + sorted(set(methods) - set(METHODS))
    if unknown:
        ap.error(f"unknown suite or method: {', '.join(unknown)}")

    for suite in args.suites or ["synthetic"]:
        out = args.out / suite
        if suite == "synthetic":
            pairs = [p for lang in corpus.LANGUAGES for p in corpus.build_pairs(lang, corpus.load_language(lang))]
            notes = ["Committed corpus `scripts/similarity-accuracy/corpus/`: t1/t2 derived from each original, t3/t4/hard hand-written."]
        else:
            archive = gptclonebench.fetch(args.out / "external" / "gptclonebench")
            groups, skipped = gptclonebench.load(archive)
            pairs = gptclonebench.build_pairs(groups, args.per_kind, args.seed)
            sizes = ", ".join(f"{k} {len(v)}" for k, v in groups.items())
            notes = [
                f"GPTCloneBench @ {gptclonebench.COMMIT[:12]}, Python stand-alone clones (available: {sizes}; {skipped} files skipped as unsplittable or not valid Python 3); sampled with seed {args.seed}.",
                "Reference only: `gcb_false` are pairs rejected as *semantic* clones, many still textual near-copies, so they are kept out of the pooled AUC.",
            ]
        rows = score_suite(binary, pairs, out / "work", methods)
        report, summary = render(suite, rows, methods, notes)
        out.mkdir(parents=True, exist_ok=True)
        (out / "report.md").write_text(report)
        (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        (out / "scores.json").write_text(json.dumps(rows, indent=1) + "\n")
        print(report)
        print(f"written to {out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
