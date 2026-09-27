"""Synthetic clone corpus: parse `corpus/<lang>/<family>.clone`, derive the
mechanical variants, and build the labelled pairs.

A `.clone` file holds one family: an original function and hand-written
variants of it, each under an `@@ <variant>` header line. Every function is
named `subject` so the name carries no label; `run.py` renames it per pair.

    @@ rename values=xs total=acc      optional: identifier map for Type-2
    @@ orig                            the original
    @@ t3-insert                       statements added
    @@ t3-modify                       statements changed or removed
    @@ t4                              same behaviour, different implementation
    @@ hard                            same shape, different behaviour (negative)

Type-1 (`t1`: comments and blank lines added) and Type-2 (`t2`: identifiers
renamed per the `rename` map) are derived from `orig`, so they never drift
from it. The pair set, per language:

- positive: `orig` against each of t1, t2, t3-*, t4 of the same family;
- `hard` negative: `orig` against the family's `hard`;
- `cross` negative: every variant in NEGATIVE_VARIANTS of one family against
  every one of another family.

Standard library only.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path

CORPUS_DIR = Path(__file__).resolve().parent / "corpus"
PLACEHOLDER = "subject"

# Per language: file extension, line-comment prefix, text prepended to every
# materialised file (Go needs a package clause to parse as a file).
LANGUAGES = {
    "python": {"ext": "py", "comment": "#", "preamble": ""},
    "typescript": {"ext": "ts", "comment": "//", "preamble": ""},
    "rust": {"ext": "rs", "comment": "//", "preamble": ""},
    "go": {"ext": "go", "comment": "//", "preamble": "package corpus\n\n"},
}
HAND_WRITTEN = ["orig", "t3-insert", "t3-modify", "t4", "hard"]
POSITIVE_VARIANTS = ["t1", "t2", "t3-insert", "t3-modify", "t4"]
NEGATIVE_VARIANTS = ["orig", "t3-insert", "t3-modify", "t4"]

_HEADER = re.compile(r"^@@ (\S+)(?: (.*))?$")


@dataclass(frozen=True)
class Pair:
    """Two functions and the truth about them.

    `kind` is the clone type for a positive (`t1`..`t4`) and `hard` or
    `cross` for a negative.
    """

    lang: str
    a: str
    b: str
    label: bool
    kind: str
    a_id: str
    b_id: str


def parse_family(text: str) -> tuple[dict[str, str], dict[str, str]]:
    """Split a `.clone` file into (variants, rename map)."""
    variants: dict[str, list[str]] = {}
    rename: dict[str, str] = {}
    current: list[str] | None = None
    for line in text.splitlines():
        header = _HEADER.match(line)
        if header is None:
            if current is None:
                if line.strip():
                    raise ValueError(f"code before the first @@ header: {line!r}")
                continue
            current.append(line)
            continue
        name, rest = header.group(1), header.group(2) or ""
        if name == "rename":
            for item in rest.split():
                old, new = item.split("=", 1)
                rename[old] = new
            current = None
            continue
        if name in variants:
            raise ValueError(f"duplicate variant {name!r}")
        current = variants[name] = []
    out = {name: "\n".join(lines).strip("\n") + "\n" for name, lines in variants.items()}
    missing = [v for v in HAND_WRITTEN if v not in out]
    if missing:
        raise ValueError(f"missing variants: {', '.join(missing)}")
    return out, rename


def type1(source: str, comment: str) -> str:
    """Type-1 clone: a comment after the signature and a blank line before the end."""
    lines = source.rstrip("\n").split("\n")
    if len(lines) < 3:
        raise ValueError("a function needs at least three lines for a Type-1 variant")
    indent = re.match(r"\s*", lines[1]).group(0)
    body = [lines[0], f"{indent}{comment} copied from the original", *lines[1:-1], "", lines[-1]]
    return "\n".join(body) + "\n"


def type2(source: str, rename: dict[str, str]) -> str:
    """Type-2 clone: identifiers renamed on word boundaries."""
    if not rename:
        raise ValueError("a family needs a `@@ rename` map for its Type-2 variant")
    pattern = re.compile(r"\b(" + "|".join(map(re.escape, rename)) + r")\b")
    renamed = pattern.sub(lambda m: rename[m.group(1)], source)
    if renamed == source:
        raise ValueError("the rename map changed nothing")
    return renamed


def load_language(lang: str, root: Path = CORPUS_DIR) -> dict[str, dict[str, str]]:
    """Every family of a language: {family: {variant: source}}, t1/t2 derived."""
    comment = LANGUAGES[lang]["comment"]
    families = {}
    for path in sorted((root / lang).glob("*.clone")):
        variants, rename = parse_family(path.read_text())
        variants["t1"] = type1(variants["orig"], comment)
        variants["t2"] = type2(variants["orig"], rename)
        families[path.stem] = variants
    return families


def clone_type(variant: str) -> str:
    return variant.split("-", 1)[0]


def build_pairs(lang: str, families: dict[str, dict[str, str]]) -> list[Pair]:
    pairs = []
    names = sorted(families)
    for family in names:
        v = families[family]
        for variant in POSITIVE_VARIANTS:
            pairs.append(Pair(lang, v["orig"], v[variant], True, clone_type(variant), f"{family}/orig", f"{family}/{variant}"))
        pairs.append(Pair(lang, v["orig"], v["hard"], False, "hard", f"{family}/orig", f"{family}/hard"))
    for i, fa in enumerate(names):
        for fb in names[i + 1 :]:
            for va in NEGATIVE_VARIANTS:
                for vb in NEGATIVE_VARIANTS:
                    pairs.append(Pair(lang, families[fa][va], families[fb][vb], False, "cross", f"{fa}/{va}", f"{fb}/{vb}"))
    return pairs
