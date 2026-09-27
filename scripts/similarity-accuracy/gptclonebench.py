"""GPTCloneBench (ICSME 2023) Python stand-alone clones as a reference suite.

Reference only, not the headline number: the pairs are GPT-generated from
SemanticCloneBench seeds and validated by hand, and the data is CC BY-NC-ND,
so it is fetched at run time into target/ and never committed. See
docs/similarity-accuracy.md for how its labels map to ours.

Each `.py` file of the archive holds one pair: two functions one after the
other. Files whose split does not yield exactly two top-level `def`s are
skipped and counted.

The archive is not all valid Python 3 (`! =`, Python 2 `print`, tabs mixed
with spaces). agent-lens drops a file it cannot parse, which would score a
broken pair 0 and charge the tool for the dataset; so a function is kept only
if `ast.parse` accepts it as is or with its tabs expanded, and a pair only if
both sides survive. Dropped files are counted.

- positive `mt3` / `t4`: `true_semantic_clones/py/prompt_*/{MT3,T4}`;
- `cross` negative: the first function of one pair against the second of
  another, sampled;
- `gcb_false` negative: `false_semantic_clones/py`, pairs the validators
  rejected as *semantic* clones. Many are still textual near-copies, so this
  row is reported apart and never pooled into the AUC.

Standard library only.
"""

from __future__ import annotations

import ast
import random
import re
import subprocess
import zipfile
from pathlib import Path

from corpus import PLACEHOLDER, Pair

REPO = "https://github.com/srlabUsask/gptclonebench"
COMMIT = "d3e953916b9323fe1fbc55c8418a2b05f7b649ed"
ARCHIVE = "GPTCloneBench_semantic_standalone_clones.zip"
PREFIX = "standalone/"

_DEF = re.compile(r"^( {0,3})def\s+(\w+)\s*\(")


def fetch(dest: Path) -> Path:
    """Shallow-fetch the pinned commit; return the archive path."""
    if not (dest / ".git").exists():
        dest.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "init", "-q", str(dest)], check=True)
        subprocess.run(["git", "-C", str(dest), "remote", "add", "origin", REPO], check=True)
    subprocess.run(["git", "-C", str(dest), "fetch", "-q", "--depth", "1", "origin", COMMIT], check=True)
    subprocess.run(["git", "-C", str(dest), "checkout", "-q", COMMIT], check=True)
    return dest / ARCHIVE


def parseable(source: str) -> str | None:
    """`source`, or its tab-expanded form, whichever Python 3 parses first."""
    for candidate in (source, source.expandtabs(4)):
        try:
            ast.parse(candidate)
        except (SyntaxError, ValueError):
            continue
        return candidate
    return None


def split_pair(text: str) -> tuple[str, str] | None:
    """The two functions of a pair file, dedented and renamed to PLACEHOLDER.

    None unless the file holds exactly two top-level `def`s that both parse.
    """
    lines = text.replace("\r\n", "\n").split("\n")
    starts = [i for i, line in enumerate(lines) if _DEF.match(line)]
    if len(starts) != 2:
        return None
    out = []
    for begin, end in ((starts[0], starts[1]), (starts[1], len(lines))):
        chunk = lines[begin:end]
        match = _DEF.match(chunk[0])
        indent, name = len(match.group(1)), match.group(2)
        chunk = [line[indent:] if line[:indent].isspace() else line for line in chunk]
        source = parseable("\n".join(chunk).rstrip() + "\n")
        if source is None:
            return None
        out.append(re.sub(rf"\b{re.escape(name)}\b", PLACEHOLDER, source))
    return out[0], out[1]


def load(archive: Path) -> tuple[dict[str, list[tuple[str, str, str]]], int]:
    """{kind: [(file id, a, b)]} for mt3, t4, gcb_false; plus the skip count."""
    groups: dict[str, list[tuple[str, str, str]]] = {"mt3": [], "t4": [], "gcb_false": []}
    skipped = 0
    with zipfile.ZipFile(archive) as z:
        for name in sorted(z.namelist()):
            if not name.endswith(".py") or not name.startswith(PREFIX):
                continue
            parts = name[len(PREFIX) :].split("/")
            if parts[:2] == ["true_semantic_clones", "py"] and len(parts) == 5:
                kind = parts[3].lower()
            elif parts[:2] == ["false_semantic_clones", "py"]:
                kind = "gcb_false"
            else:
                continue
            pair = split_pair(z.read(name).decode("utf-8", errors="replace"))
            if pair is None:
                skipped += 1
                continue
            groups[kind].append((name, *pair))
    return groups, skipped


def build_pairs(groups: dict[str, list[tuple[str, str, str]]], per_kind: int, seed: int) -> list[Pair]:
    """Sample `per_kind` pairs of each kind and `2 * per_kind` cross negatives."""
    rng = random.Random(seed)
    pairs = []
    for kind in ("mt3", "t4", "gcb_false"):
        items = groups[kind]
        for fid, a, b in rng.sample(items, min(per_kind, len(items))):
            pairs.append(Pair("python", a, b, kind != "gcb_false", kind, fid, fid))
    # The same SemanticCloneBench seed is the first function of several
    # files (prompts, MT3 and T4), so a cross pair whose two seeds are equal
    # would be a clone labelled negative: skip it.
    pool = groups["mt3"] + groups["t4"]
    wanted = min(2 * per_kind, len(pool)) if len({seed for _, seed, _ in pool}) > 1 else 0
    while wanted > 0:
        (fa, a, _), (fb, seed_b, b) = rng.sample(pool, 2)
        if a == seed_b:
            continue
        pairs.append(Pair("python", a, b, False, "cross", fa, fb))
        wanted -= 1
    return pairs
