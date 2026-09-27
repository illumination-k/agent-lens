#!/usr/bin/env python3
"""Pyright type-checker oracle for Python: resolve every call site under root with pyright.

    oracle_pyright.py --root <dir> --out <json> [--python <interpreter>] [--pyright-langserver CMD]

Drives ``pyright-langserver --stdio`` over LSP (stdlib only, Python >= 3.11).
Call sites come from ``ast``; each callee is resolved with
``textDocument/definition`` at the callee's name token, so the type checker
decides what runs (imports and ``__init__`` re-exports followed, method
receivers typed by annotation or inference), in the oracle contract of
docs/callgraph-accuracy.md (oracle ``python-pyright``, kind ``type-checker``).
``--python`` points pyright at the interpreter (a venv) whose packages it
should see; without it pyright picks the one on PATH.

Edge conventions:

- Call sites: every ``ast.Call`` whose callee is a name (``f(x)``) or an
  attribute (``x.f(x)``, ``super().f()``, ``m.f()``); ``call_line`` is the line
  of the callee's name. Calls of any other shape (``f()()``, ``fs[0]()``) are
  counted (``call_site_other_shape``), not resolved.
- Callers: the innermost ``def`` lexically around the call (a call in a lambda
  or comprehension belongs to the enclosing def; decorators and default values
  belong to the scope around the def they decorate). Calls in module or class
  bodies have no caller and are counted (``call_site_caller_module_or_class``).
- Callees: a ``def`` under root (``callee_def_line`` is the ``def`` line,
  decorators skipped). The declaration is the edge, whatever a decorator wraps
  it in: ``@deco def f`` called as ``f()`` is ``caller -> f``. A definition that
  lands on a simple alias (``g = f``, ``g = mod.f``) is followed to what the
  alias names. Calls resolving outside root (stdlib, dependencies) or to a
  value that is not a function (a parameter, a local, an attribute) have no edge.
- Constructors: a call resolving to a class under root is an edge to the
  ``__init__`` and the ``__new__`` the class's MRO reaches first, when that
  class is under root (bases resolved with pyright, C3 order; a base outside
  root ends the walk). ``super().__init__()`` resolves like any method.
- Overloads: when pyright lists ``@overload`` stubs next to the
  implementation, only the implementation is kept.
- Dispatch: ``static`` for a single resolved function or constructor.
  ``dynamic`` when pyright lists several targets (a union receiver,
  conditional definitions), for an abstract method (``@abstractmethod``, a
  ``Protocol`` member, a body that only raises ``NotImplementedError`` or is
  ``...``), which also gets a ``dynamic`` edge to every override in a
  subclass under root, and for a property read (``x.p`` where ``p`` is a
  ``@property`` / ``cached_property`` getter under root), which runs a function
  without a call expression. Operator and protocol dunders (``len(x)``,
  ``for``, ``with``) and functions passed as values (``sorted(key=f)``) have
  no edge.
- ``unresolved_call_sites``: call sites where pyright found no declaration at
  all (the receiver's type is unknown: an unannotated parameter, an untyped
  dependency) and whose name is also the name of a function or class under
  root. The scorer leaves an agent-lens edge out of precision when every call
  site of it is one of these: the oracle cannot say where it goes.
- ``analyzed_files``: every ``.py`` file under root (``.venv`` / ``venv`` /
  ``.tox`` / ``.nox``, ``node_modules``, ``__pycache__``, ``site-packages`` and
  dot-directories skipped).

Logs and the drop counts go to stderr.
"""

from __future__ import annotations

import argparse
import ast
import json
import os
import re
import shlex
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from pathlib import Path

from lsp import LspClient, position, targets, uri_to_path

SKIP_DIRS = {".venv", "venv", ".tox", ".nox", "node_modules", "__pycache__", "site-packages", "dist-packages"}
PROPERTY_DECORATORS = {"property", "cached_property", "abstractproperty"}
ABSTRACT_DECORATORS = {"abstractmethod", "abstractproperty"}
CONSTRUCTORS = ("__new__", "__init__")
# How many `g = f` hops a definition is followed through.
ALIAS_DEPTH = 4
DEF_KEYWORD = re.compile(rb"(?:async\s+)?(?:def|class)\s+")

Key = tuple[str, int]  # (relative posix path, 1-based def line)


def log(msg: str) -> None:
    print(f"oracle_pyright: {msg}", file=sys.stderr, flush=True)


# ---------------------------------------------------------------- per-file facts


@dataclass
class Function:
    name: str
    col: int | None  # utf-16 column of the name on the def line
    decorators: set[str]
    owner: int | None  # def line of the class whose body defines it
    abstract: bool = False


@dataclass
class Class:
    name: str
    col: int | None
    bases: list[tuple[int, int]]  # 0-based (line, utf-16 column) of each base's name
    protocol: bool
    methods: dict[str, int] = field(default_factory=dict)  # name -> def line


@dataclass
class Site:
    caller: int | None  # def line of the enclosing def
    line: int  # 1-based line of the callee's name
    col: int  # utf-16 column of the callee's name
    name: str


class Module:
    """ast facts of one file: functions, classes, simple aliases, call sites, attribute reads."""

    def __init__(self, path: Path, rel: str):
        self.path, self.rel = path, rel
        self.functions: dict[int, Function] = {}
        self.classes: dict[int, Class] = {}
        # target (line, utf-16 column) -> (0-based line, utf-16 column) of the aliased name
        self.aliases: dict[tuple[int, int], tuple[int, int]] = {}
        self.calls: list[Site] = []
        self.reads: list[Site] = []  # attribute loads that are not callees
        self.other_calls = 0
        data = path.read_bytes()
        self.lines = data.splitlines()
        try:
            tree = ast.parse(data, filename=str(path))
        except (SyntaxError, ValueError):
            self.parsed = False
            return
        self.parsed = True
        _Collector(self).visit(tree)

    def col16(self, line: int, byte_col: int) -> int:
        """UTF-16 column (LSP) of a UTF-8 byte offset (ast) in 1-based `line`."""
        text = self.lines[line - 1][:byte_col] if 0 < line <= len(self.lines) else b""
        return len(text.decode("utf-8", "replace").encode("utf-16-le")) // 2

    def def_name_col(self, node: ast.FunctionDef | ast.AsyncFunctionDef | ast.ClassDef) -> int | None:
        """UTF-16 column of a def's / class's name, which is on its `def` / `class` line."""
        m = DEF_KEYWORD.match(self.lines[node.lineno - 1], node.col_offset) if node.lineno <= len(self.lines) else None
        return self.col16(node.lineno, m.end()) if m else None

    def name_pos(self, node: ast.expr) -> tuple[int, int, str] | None:
        """(1-based line, utf-16 column, name) of the name a Name / Attribute ends in."""
        if isinstance(node, ast.Name):
            return node.lineno, self.col16(node.lineno, node.col_offset), node.id
        if isinstance(node, ast.Attribute) and node.end_lineno is not None and node.end_col_offset is not None:
            start = node.end_col_offset - len(node.attr.encode())
            return node.end_lineno, self.col16(node.end_lineno, start), node.attr
        return None


def _decorator_names(node: ast.FunctionDef | ast.AsyncFunctionDef | ast.ClassDef) -> set[str]:
    out = set()
    for d in node.decorator_list:
        d = d.func if isinstance(d, ast.Call) else d
        if isinstance(d, ast.Name):
            out.add(d.id)
        elif isinstance(d, ast.Attribute):
            out.add(d.attr)
            if isinstance(d.value, ast.Name) and d.attr in {"getter", "setter", "deleter"}:
                out.add("property")
    return out


def _only_raises_or_ellipsis(body: list[ast.stmt]) -> bool:
    """A body that is only a docstring and `...`, or ends in `raise NotImplementedError`."""
    stmts = body[1:] if body and _is_docstring(body[0]) else body
    if not stmts:
        return False
    last = stmts[-1]
    if len(stmts) == 1 and isinstance(last, ast.Expr) and isinstance(last.value, ast.Constant) and last.value.value is Ellipsis:
        return True
    if isinstance(last, ast.Raise) and last.exc is not None:
        exc = last.exc.func if isinstance(last.exc, ast.Call) else last.exc
        return isinstance(exc, ast.Name) and exc.id == "NotImplementedError" and len(stmts) == 1
    return False


def _is_docstring(stmt: ast.stmt) -> bool:
    return isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Constant) and isinstance(stmt.value.value, str)


class _Collector(ast.NodeVisitor):
    def __init__(self, mod: Module):
        self.mod = mod
        self.func: int | None = None  # def line of the innermost enclosing def
        self.cls: Class | None = None  # class whose body is being visited directly
        self.cls_line: int | None = None
        self.callees: set[int] = set()  # id() of Attribute nodes that are a call's callee

    def visit_FunctionDef(self, node):
        # Decorators, defaults and annotations run in the enclosing scope.
        for d in node.decorator_list:
            self.visit(d)
        self.visit(node.args)
        if node.returns is not None:
            self.visit(node.returns)
        decorators = _decorator_names(node)
        fn = Function(node.name, self.mod.def_name_col(node), decorators, self.cls_line)
        if self.cls is not None:
            self.cls.methods.setdefault(node.name, node.lineno)
            fn.abstract = bool(decorators & ABSTRACT_DECORATORS) or self.cls.protocol or _only_raises_or_ellipsis(node.body)
        self.mod.functions[node.lineno] = fn
        saved = self.func, self.cls, self.cls_line
        self.func, self.cls, self.cls_line = node.lineno, None, None
        for stmt in node.body:
            self.visit(stmt)
        self.func, self.cls, self.cls_line = saved

    visit_AsyncFunctionDef = visit_FunctionDef

    def visit_ClassDef(self, node):
        for d in node.decorator_list:
            self.visit(d)
        bases, protocol = [], False
        for b in node.bases:
            self.visit(b)
            target = b.value if isinstance(b, ast.Subscript) else b
            pos = self.mod.name_pos(target)
            if pos is not None:
                bases.append((pos[0] - 1, pos[1]))
                protocol |= pos[2] == "Protocol"
        for k in node.keywords:
            self.visit(k)
        cls = Class(node.name, self.mod.def_name_col(node), bases, protocol)
        self.mod.classes[node.lineno] = cls
        saved = self.cls, self.cls_line
        self.cls, self.cls_line = cls, node.lineno
        for stmt in node.body:
            self.visit(stmt)
        self.cls, self.cls_line = saved

    def visit_Assign(self, node):
        target = node.targets[0]
        if len(node.targets) == 1 and isinstance(target, ast.Name):
            pos = self.mod.name_pos(node.value)
            if pos is not None:
                at = (target.lineno, self.mod.col16(target.lineno, target.col_offset))
                self.mod.aliases[at] = (pos[0] - 1, pos[1])
        self.generic_visit(node)

    def visit_Call(self, node):
        pos = self.mod.name_pos(node.func)
        if pos is None:
            self.mod.other_calls += 1
        else:
            self.mod.calls.append(Site(self.func, *pos))
            self.callees.add(id(node.func))
        self.generic_visit(node)

    def visit_Attribute(self, node):
        if isinstance(node.ctx, ast.Load) and id(node) not in self.callees:
            pos = self.mod.name_pos(node)
            if pos is not None:
                self.mod.reads.append(Site(self.func, *pos))
        self.generic_visit(node)


def python_files(root: Path) -> list[Path]:
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS and not d.startswith("."))
        out += [Path(dirpath) / f for f in sorted(filenames) if f.endswith(".py")]
    return out


# ---------------------------------------------------------------- oracle


class PyrightOracle:
    """One pyright session over one tree; `run` returns the oracle document."""

    def __init__(self, root: Path, client: LspClient):
        self.root = root
        self.client = client
        self.stats: Counter = Counter()
        self.edges: set[tuple] = set()
        self.unresolved: set[tuple[str, int, str]] = set()
        self.modules: dict[str, Module] = {}
        self.names: set[str] = set()  # every function and class name under root
        self.property_names: set[str] = set()
        self.cache: dict[tuple[str, int, int], list | None] = {}
        self.bases: dict[Key, list[Key]] = {}
        self.mros: dict[Key, list[Key]] = {}
        self.subclasses: dict[Key, set[Key]] | None = None

    def run(self) -> dict:
        for f in python_files(self.root):
            rel = f.relative_to(self.root).as_posix()
            mod = Module(f, rel)
            if not mod.parsed:
                self.stats["file_syntax_error"] += 1
                continue
            self.modules[rel] = mod
            self.names |= {fn.name for fn in mod.functions.values()} | {c.name for c in mod.classes.values()}
            self.property_names |= {fn.name for fn in mod.functions.values() if fn.decorators & PROPERTY_DECORATORS}
            text = f.read_text(encoding="utf-8", errors="replace")
            self.client.notify(
                "textDocument/didOpen",
                {"textDocument": {"uri": f.as_uri(), "languageId": "python", "version": 1, "text": text}},
            )
        log(f"{sum(len(m.functions) for m in self.modules.values())} functions in {len(self.modules)} files")
        for mod in self.modules.values():
            self.stats["call_site_other_shape"] += mod.other_calls
            for site in mod.calls:
                self.call(mod, site)
            for site in mod.reads:
                if site.name in self.property_names and site.caller is not None:
                    self.property_read(mod, site)
        keys = ["caller_file", "caller_def_line", "call_line", "callee_file", "callee_def_line", "dispatch"]
        return {
            "oracle": "python-pyright",
            "language": "python",
            "kind": "type-checker",
            "root": str(self.root),
            "edges": [dict(zip(keys, e)) for e in sorted(self.edges)],
            "analyzed_files": sorted(self.modules),
            "unresolved_call_sites": [{"file": f, "line": ln, "name": n} for f, ln, n in sorted(self.unresolved)],
        }

    # --- resolution

    def definitions(self, rel: str, line0: int, col: int) -> list | None:
        """What `textDocument/definition` names at a position, aliases followed.

        None when pyright finds no declaration; otherwise a list of targets,
        each ("func", key), ("class", key) or None (outside root, or not a
        function or class: a parameter, a local, an attribute).
        """
        k = (rel, line0, col)
        if k not in self.cache:
            self.cache[k] = None  # guards alias cycles
            self.cache[k] = self._definitions(rel, line0, col, 0)
        return self.cache[k]

    def _definitions(self, rel: str, line0: int, col: int, depth: int) -> list | None:
        found = targets(self.client.request("textDocument/definition", position(self.root / rel, line0, col)))
        if not found:
            return None
        out: list = []
        for uri, target0, char in found:
            try:
                trel = uri_to_path(uri).resolve().relative_to(self.root).as_posix()
            except ValueError:
                out.append(None)
                continue
            mod = self.modules.get(trel)
            line = target0 + 1
            # The column tells a def's name from a parameter on its line.
            fn, cls = (mod.functions.get(line), mod.classes.get(line)) if mod else (None, None)
            if fn is not None and fn.col in (None, char):
                out.append(("func", (trel, line)))
            elif cls is not None and cls.col in (None, char):
                out.append(("class", (trel, line)))
            elif mod is not None and (line, char) in mod.aliases and depth < ALIAS_DEPTH:
                sub = self._definitions(trel, *mod.aliases[(line, char)], depth + 1)
                out.extend(sub or [None])
            else:
                out.append(None)
        # `@overload` stubs next to their implementation: keep the implementation.
        funcs = [t for t in out if t and t[0] == "func"]
        impl = [t for t in funcs if "overload" not in self.function(t[1]).decorators]
        if impl and len(impl) < len(funcs):
            out = [t for t in out if not (t and t[0] == "func") or t in impl]
        return list(dict.fromkeys(out))

    def function(self, key: Key) -> Function:
        return self.modules[key[0]].functions[key[1]]

    def call(self, mod: Module, site: Site) -> None:
        if site.caller is None:
            self.stats["call_site_caller_module_or_class"] += 1
            return
        targets = self.definitions(mod.rel, site.line - 1, site.col)
        if targets is None:
            self.stats["call_site_unresolved"] += 1
            if site.name in self.names:
                self.unresolved.add((mod.rel, site.line, site.name))
            return
        callees: list[Key] = []
        for t in targets:
            if t is None:
                continue
            if t[0] == "func":
                callees.append(t[1])
            else:
                ctors = self.constructors(t[1])
                if not ctors:
                    self.stats["constructor_without_init_under_root"] += 1
                callees += ctors
        if not callees:
            self.stats["callee_outside_root_or_not_a_function"] += 1
            return
        several = len({t[1] for t in targets if t}) > 1
        for key in callees:
            self.add(mod, site, key, "dynamic" if several else "static")

    def property_read(self, mod: Module, site: Site) -> None:
        for t in self.definitions(mod.rel, site.line - 1, site.col) or []:
            if t and t[0] == "func" and self.function(t[1]).decorators & PROPERTY_DECORATORS:
                self.add(mod, site, t[1], "dynamic")
                self.stats["property_read_edges"] += 1

    def add(self, mod: Module, site: Site, callee: Key, dispatch: str) -> None:
        caller = (mod.rel, site.caller, site.line)
        fn = self.function(callee)
        if fn.abstract and fn.owner is not None:
            dispatch = "dynamic"
            for override in self.overrides((callee[0], fn.owner), fn.name):
                self.edges.add((*caller, *override, "dynamic"))
        self.edges.add((*caller, *callee, dispatch))

    # --- classes

    def class_bases(self, key: Key) -> list[Key]:
        """The bases of a class under root that are classes under root, in order."""
        if key not in self.bases:
            self.bases[key] = []
            out = []
            for line0, col in self.modules[key[0]].classes[key[1]].bases:
                for t in self.definitions(key[0], line0, col) or []:
                    if t and t[0] == "class" and t[1] != key:
                        out.append(t[1])
            self.bases[key] = list(dict.fromkeys(out))
        return self.bases[key]

    def mro(self, key: Key) -> list[Key]:
        """C3 linearisation over the bases under root (a depth-first order if C3 fails)."""
        if key in self.mros:
            return self.mros[key]
        self.mros[key] = [key]  # guards inheritance cycles
        seqs = [list(self.mro(b)) for b in self.class_bases(key)] + [list(self.class_bases(key))]
        out = [key]
        while any(seqs):
            for seq in seqs:
                head = seq[0] if seq else None
                if head is not None and not any(head in s[1:] for s in seqs):
                    break
            else:
                # Inconsistent: fall back to depth-first, first occurrence wins.
                rest = [c for s in seqs for c in s]
                out += [c for c in dict.fromkeys(rest) if c not in out]
                break
            out.append(head)
            seqs = [[c for c in s if c != head] for s in seqs]
        self.mros[key] = out
        return out

    def constructors(self, key: Key) -> list[Key]:
        out = []
        for name in CONSTRUCTORS:
            for c in self.mro(key):
                line = self.modules[c[0]].classes[c[1]].methods.get(name)
                if line is not None:
                    out.append((c[0], line))
                    break
        return out

    def overrides(self, owner: Key, name: str) -> list[Key]:
        if self.subclasses is None:
            self.subclasses = defaultdict(set)
            for rel, mod in self.modules.items():
                for line in mod.classes:
                    for base in self.class_bases((rel, line)):
                        self.subclasses[base].add((rel, line))
        out, stack, seen = [], [owner], {owner}
        while stack:
            for sub in sorted(self.subclasses.get(stack.pop(), ())):
                if sub in seen:
                    continue
                seen.add(sub)
                stack.append(sub)
                line = self.modules[sub[0]].classes[sub[1]].methods.get(name)
                if line is not None:
                    out.append((sub[0], line))
        return out


# ---------------------------------------------------------------- session


def run(root: Path, langserver: list[str], python: Path | None = None) -> tuple[dict, Counter]:
    root = root.resolve()
    settings = {
        "python": {"pythonPath": str(python)} if python else {},
        "python.analysis": {"autoSearchPaths": True, "useLibraryCodeForTypes": True, "diagnosticMode": "openFilesOnly"},
    }

    def answer(msg: dict):
        if msg["method"] == "workspace/configuration":
            return [settings.get(item.get("section"), {}) for item in msg["params"]["items"]]
        return None

    client = LspClient([*langserver, "--stdio"], cwd=root, answer=answer)
    try:
        client.request(
            "initialize",
            {
                "processId": os.getpid(),
                "rootUri": root.as_uri(),
                "workspaceFolders": [{"uri": root.as_uri(), "name": root.name}],
                "capabilities": {
                    "workspace": {"configuration": True},
                    "textDocument": {"definition": {"linkSupport": True}},
                },
            },
        )
        client.notify("initialized", {})
        oracle = PyrightOracle(root, client)
        return oracle.run(), oracle.stats
    finally:
        client.close()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", required=True, type=Path)
    ap.add_argument("--out", type=Path)
    ap.add_argument("--python", type=Path, help="interpreter whose environment pyright resolves imports in")
    ap.add_argument(
        "--pyright-langserver",
        default=os.environ.get("PYRIGHT_LANGSERVER", "pyright-langserver"),
        help="command that starts pyright's language server (without --stdio)",
    )
    args = ap.parse_args()
    doc, stats = run(args.root, shlex.split(args.pyright_langserver), args.python)
    text = json.dumps(doc, indent=1) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text)
    else:
        sys.stdout.write(text)
    dropped = ", ".join(f"{n} {k}" for k, n in sorted(stats.items())) or "nothing"
    log(f"{len(doc['edges'])} edges from {len(doc['analyzed_files'])} files; {dropped}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
