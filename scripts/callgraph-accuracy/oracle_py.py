#!/usr/bin/env python3
"""Dynamic call-graph oracle for Python: record caller -> callee edges while pytest runs.

Usage (inside a venv where the target project and pytest are installed):

    python oracle_py.py --root <project> --out <oracle.json> [--] [pytest args...]

pytest runs in-process (``pytest.main``) under ``sys.setprofile`` and
``threading.setprofile`` (``threading.setprofile_all_threads`` where it exists).
Every Python function call whose callee *and* immediate caller are defined in a
file under ``--root`` becomes an edge of the shared oracle contract (oracle
``python-setprofile``, kind ``dynamic``). Stdlib only, Python >= 3.11.

Conventions (see also the stats block of the output):

- Scope: a file is under root when its realpath is inside ``--root`` and not in
  ``sys.prefix`` / ``sys.base_prefix``, a ``site-packages`` / ``dist-packages``
  directory, or ``<root>/{.venv,venv,.tox,.nox}``. A call made by library code
  (pytest invoking a test, functools running a wrapped function) is not an edge:
  the immediate caller frame must be under root.
- Callers: the immediate under-root frame gives ``call_line``. When that frame is
  a lambda or a comprehension / generator expression, ``caller_def_line`` is the
  *lexically* enclosing ``def`` (found with ``ast``), not the next frame up the
  stack: a generator expression may be consumed far from where it was written.
  Calls made from a module or class body have no function to attribute them to
  and are counted per call site (``call_site_caller_module_or_class``), not emitted.
- Callees: ``callee_def_line`` is the ``def`` line, decorators skipped (``ast``
  maps a code object's ``co_firstlineno``, the first decorator line, to
  ``FunctionDef.lineno``). Module bodies, class bodies, lambdas, comprehensions
  and other ``<...>`` code objects have no agent-lens node; they are never
  emitted, and each distinct one that ran is counted under ``no_node_code:<name>``.
- Generators / coroutines: every resume fires ``call`` again. Only the first
  resume counts, detected by the frame's ``f_lasti`` not being past the code's
  first ``RESUME`` instruction. CPython fires no ``call`` when a generator object
  is created, so the first resume happens wherever it is first advanced (often
  a C consumer such as ``sum`` in another function). The edge is kept only when
  that line calls the generator function by name (``ast``, as in ``for x in gen():``);
  otherwise it is counted (``call_site_generator_first_advanced_elsewhere``) and
  dropped, since the creating call site is unknown.
- Dispatch: a dynamic oracle cannot see static types, so the call line decides
  (rules in order):
  ``dynamic`` if the callee is a dunder other than ``__init__`` / ``__new__`` /
  ``__call__`` / ``__post_init__`` / ``__init_subclass__`` that the call line
  does not name (``deque(x)`` -> ``__iter__``, ``len(x)`` -> ``__len__``);
  ``callback`` if the caller frame had a C function call open (``c_call``
  without its ``c_return``), e.g. ``sorted(xs, key=f)``;
  ``dynamic`` if the caller's current instruction is not a call instruction
  (property getter, operator dunder, ``__iter__``, ``__enter__`` on <= 3.13);
  for a constructor dunder (``__init__``, ``__new__``, ``__post_init__``,
  ``__init_subclass__``): ``static`` if the line calls the class, a subclass
  inheriting the dunder, or the dunder itself (``A()``, ``B()``,
  ``super().__init__()``), ``callback`` if it only names the class
  (``map(A, xs)``), ``dynamic`` otherwise (``cls(x)``, ``type(self)(x)``, a
  local alias);
  ``static`` if a call expression spanning the call line names the callee
  (``f(...)``, ``x.f(...)``, ``super().f(...)``);
  ``callback`` if the line names the callee without calling it: it was passed
  as a value and invoked by code outside root, e.g. ``list(map(f, xs))``
  (``map`` is a C *type*, which raises no ``c_call``);
  ``dynamic`` otherwise: a call through a value bound to another name (a
  parameter ``func(x)``, a local, a curried object's ``__call__``, the
  wrapper a decorator returned calling the decorated function).
  Kept generator and coroutine first resumes are always ``static``.
- ``executed_functions``: every under-root function code seen running.
- Overhead: a call site (callee code, caller code, caller instruction, callback)
  is resolved once; later hits cost a set lookup. Expect roughly 5-10x the
  suite's plain runtime (a no-op profile function alone costs ~4x).

Exit status is pytest's. The JSON is written whenever tests ran (exit codes 0
and 1, or any edge observed): failing tests do not invalidate observed edges.
"""

from __future__ import annotations

import argparse
import ast
import dis
import json
import linecache
import os
import re
import sys
import threading
from collections import Counter, defaultdict
from pathlib import Path

CO_OPTIMIZED = 0x0001
CO_GENERATOR_LIKE = 0x0020 | 0x0080 | 0x0200  # generator, coroutine, async generator
CALL_OPS = frozenset(
    {"CALL", "CALL_KW", "CALL_FUNCTION_EX", "PRECALL", "CALL_FUNCTION", "CALL_FUNCTION_KW", "CALL_METHOD"}
)
# Dunders a plain call reaches (`A()`, `obj()`); any other dunder not named on
# the call line is an implicit protocol call, even from inside a C callable.
EXPLICIT_DUNDERS = frozenset({"__init__", "__new__", "__call__", "__post_init__", "__init_subclass__"})
# Of those, the ones a call naming the class reaches: static without their
# own name on the call line. `__call__` is not among them: `obj(x)` calls
# through a value.
CONSTRUCTOR_DUNDERS = frozenset({"__init__", "__new__", "__post_init__", "__init_subclass__"})
WORD = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
COMPREHENSIONS = frozenset({"<listcomp>", "<dictcomp>", "<setcomp>", "<genexpr>"})
EXCLUDED_ROOT_DIRS = (".venv", "venv", ".tox", ".nox")

# Code kinds.
FUNC, ANON, SKIP = "func", "anon", "skip"


class FileIndex:
    """Per-file ast facts: decorator line -> def line, function body ranges, called names."""

    def __init__(self, path: str):
        self.def_line: dict[int, int] = {}
        self.bodies: list[tuple[int, int, int]] = []  # (body_start, end, def_line)
        self.called: dict[int, set[str]] = defaultdict(set)  # line -> names called on it
        try:
            tree = ast.parse(Path(path).read_bytes(), filename=path)
        except (OSError, SyntaxError, ValueError):
            return
        for node in ast.walk(tree):
            if isinstance(node, ast.Call):
                name = _called_name(node.func)
                if name is not None:
                    for line in range(node.lineno, (node.end_lineno or node.lineno) + 1):
                        self.called[line].add(name)
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                first = min([node.lineno] + [d.lineno for d in node.decorator_list])
                self.def_line[first] = node.lineno
                self.def_line[node.lineno] = node.lineno
                body_start = node.body[0].lineno if node.body else node.lineno
                self.bodies.append(
                    (min(body_start, node.end_lineno or node.lineno), node.end_lineno or node.lineno, node.lineno)
                )

    def names_call(self, line: int, name: str) -> bool:
        """Whether a call expression spanning `line` names `name` (`name(...)`, `x.name(...)`)."""
        return name in self.called.get(line, ())

    def enclosing_def(self, line: int) -> int | None:
        """Def line of the innermost function whose body contains `line`."""
        best = None
        for start, end, def_line in self.bodies:
            if start <= line <= end and (best is None or end - start < best[0]):
                best = (end - start, def_line)
        return best[1] if best else None


def _called_name(func: ast.expr) -> str | None:
    if isinstance(func, ast.Name):
        return func.id
    if isinstance(func, ast.Attribute):
        return func.attr
    return None


class Oracle:
    def __init__(self, root: Path, exclude_files: set[str]):
        self.root = str(root)
        self.prefix = self.root + os.sep
        excluded = {os.path.realpath(p) for p in (sys.prefix, sys.base_prefix, sys.exec_prefix)}
        excluded |= {os.path.join(self.root, d) for d in EXCLUDED_ROOT_DIRS}
        self.excluded = tuple(p + os.sep for p in excluded if p != self.root)
        self.exclude_files = exclude_files
        self.files: dict[str, str | None] = {}  # co_filename -> relative posix path or None
        self.indexes: dict[str, FileIndex] = {}
        # id(code) -> None (not under root) or (kind, rel, def_line, is_generator, first_resume[, name])
        # Keyed by id(code): hashing a code object hashes its contents. `pinned`
        # keeps every classified code alive so its id is never reused.
        self.codes: dict = {}
        self.pinned: list = []
        self.call_ops: dict = {}  # code -> list of opnames per code unit
        self.edges: set[tuple] = set()
        self.executed: set[tuple[str, int]] = set()
        self.stats: Counter = Counter()

    # --- classification (cached per file / code object) ---

    def rel(self, filename: str) -> str | None:
        if filename in self.files:
            return self.files[filename]
        rel = None
        if not filename.startswith("<"):
            real = os.path.realpath(filename)
            if (
                real.startswith(self.prefix)
                and not real.startswith(self.excluded)
                and real not in self.exclude_files
                and os.sep + "site-packages" + os.sep not in real
                and os.sep + "dist-packages" + os.sep not in real
            ):
                rel = Path(os.path.relpath(real, self.root)).as_posix()
                self.indexes[rel] = FileIndex(real)
        self.files[filename] = rel
        return rel

    def classify(self, code):
        rel = self.rel(code.co_filename)
        info = None
        if rel is not None:
            name = code.co_name
            if name in COMPREHENSIONS or name == "<lambda>":
                info = (ANON, rel, None, False, 0)
            elif name.startswith("<") or not code.co_flags & CO_OPTIMIZED:
                info = (SKIP, rel, None, False, 0)
            if info is not None:
                self.stats["no_node_code:" + (name if name.startswith("<") else "<class body>")] += 1
            else:
                def_line = self.indexes[rel].def_line.get(code.co_firstlineno, code.co_firstlineno)
                gen = bool(code.co_flags & CO_GENERATOR_LIKE)
                first_resume = 0
                if gen:
                    first_resume = next((i.offset for i in dis.get_instructions(code) if i.opname == "RESUME"), 0)
                info = (FUNC, rel, def_line, gen, first_resume, code.co_name)
                self.executed.add((rel, def_line))
        self.codes[id(code)] = info
        self.pinned.append(code)
        return info

    def opname_at(self, code, lasti: int) -> str:
        ops = self.call_ops.get(code)
        if ops is None:
            # Inline CACHE entries belong to the instruction before them.
            ops = []
            for ins in dis.get_instructions(code):
                ops.extend([ops[-1]] * (ins.offset // 2 - len(ops)) if ops else ())
                ops.append(ins.opname)
            ops.extend([ops[-1]] * (len(code.co_code) // 2 - len(ops)) if ops else ())
            self.call_ops[code] = ops
        unit = lasti // 2
        return ops[unit] if 0 <= unit < len(ops) else ""

    # --- the profile function ---

    def make_profile(self):
        """The profile function: a closure, since it runs for every call and C call."""
        codes_get = self.codes.get
        classify = self.classify
        seen: set = set()  # (callee code, caller code, caller f_lasti, callback) already handled
        record = self.record

        class Local(threading.local):
            def __init__(self):
                self.c = []  # frames with a C call open, innermost last

        local = Local()

        def profile(frame, event, arg):
            if event == "call":
                code = frame.f_code
                info = codes_get(id(code), 0)
                if info == 0:
                    info = classify(code)
                if info is None or info[0] != FUNC:
                    return
                if info[3] and frame.f_lasti > info[4]:
                    return  # a generator / coroutine resumed, not called
                caller = frame.f_back
                if caller is None:
                    return
                stack = local.c
                callback = bool(stack) and stack[-1] is caller
                key = (id(code), id(caller.f_code), caller.f_lasti, callback)
                if key not in seen:
                    seen.add(key)
                    record(info, frame, caller, callback)
            elif event == "c_call":
                local.c.append(frame)
            elif event == "c_return" or event == "c_exception":
                stack = local.c
                if stack and stack[-1] is frame:
                    stack.pop()

        return profile

    def record(self, info, frame, caller, callback: bool):
        """Record the edge of a call site seen for the first time."""
        _, callee_file, callee_def, gen, _, name = info
        cinfo = self.codes.get(id(caller.f_code), 0)
        if cinfo == 0:
            cinfo = self.classify(caller.f_code)
        if cinfo is None:
            self.stats["call_site_caller_outside_root"] += 1
            return
        call_line = caller.f_lineno
        ckind, caller_file, caller_def = cinfo[0], cinfo[1], cinfo[2]
        if ckind == ANON:
            caller_def = self.indexes[caller_file].enclosing_def(call_line)
        if caller_def is None:
            self.stats["call_site_caller_module_or_class"] += 1
            return
        line = linecache.getline(caller.f_code.co_filename, call_line)
        index = self.indexes[caller_file]
        if gen:
            if not index.names_call(call_line, name):
                self.stats["call_site_generator_first_advanced_elsewhere"] += 1
                return
            dispatch = "static"
        elif name.startswith("__") and name.endswith("__") and name not in EXPLICIT_DUNDERS and name not in line:
            dispatch = "dynamic"
        elif callback:
            dispatch = "callback"
        elif self.opname_at(caller.f_code, caller.f_lasti) not in CALL_OPS:
            dispatch = "dynamic"
        elif name in CONSTRUCTOR_DUNDERS:
            # Static only when the line calls the class (or a subclass
            # inheriting the dunder) by name, or names the dunder itself
            # (`super().__init__()`). `cls(x)`, `type(self)(x)` and a local
            # alias call through a value; `map(A, xs)` hands the class to C.
            classes = constructed_classes(frame)
            words = set(WORD.findall(line))
            if index.names_call(call_line, name) or any(index.names_call(call_line, c) for c in classes):
                dispatch = "static"
            elif words & classes:
                dispatch = "callback"
            else:
                dispatch = "dynamic"
        elif index.names_call(call_line, name):
            dispatch = "static"
        elif name in WORD.findall(line):
            # Named on the line but not called there: passed as a value and
            # invoked by code that is not under root (`list(map(f, xs))`).
            dispatch = "callback"
        else:
            # A call through a value bound to another name (a parameter, a
            # local, a curried object's `__call__`).
            dispatch = "dynamic"
        self.edges.add((caller_file, caller_def, call_line, callee_file, callee_def, dispatch))

    def result(self) -> dict:
        keys = ("caller_file", "caller_def_line", "call_line", "callee_file", "callee_def_line", "dispatch")
        return {
            "oracle": "python-setprofile",
            "language": "python",
            "kind": "dynamic",
            "root": self.root,
            "edges": [dict(zip(keys, e)) for e in sorted(self.edges)],
            "executed_functions": [{"file": f, "def_line": d} for f, d in sorted(self.executed)],
            "stats": dict(sorted(self.stats.items())),
        }


def constructed_classes(frame) -> set[str]:
    """Names of the classes a constructor dunder's frame builds, up to the defining one.

    The first argument is the instance (`__init__`, `__post_init__`) or the
    class (`__new__`, `__init_subclass__`); its MRO, cut at the class whose
    body defines the running code (`co_qualname` on 3.11+), gives every class
    name a call reaching this dunder may spell.
    """
    code = frame.f_code
    owner = getattr(code, "co_qualname", "").rpartition(".")[0].rpartition(".")[2]
    names = {owner} if owner else set()
    if not code.co_argcount:
        return names
    obj = frame.f_locals.get(code.co_varnames[0])
    cls = obj if isinstance(obj, type) else type(obj)
    for c in getattr(cls, "__mro__", ()):
        names.add(c.__name__)
        if c.__name__ == owner:
            break
    else:
        return {owner} if owner else set()
    return names


def install(profile):
    sys.setprofile(profile)
    if hasattr(threading, "setprofile_all_threads"):
        threading.setprofile_all_threads(profile)
    else:
        threading.setprofile(profile)


def uninstall():
    if hasattr(threading, "setprofile_all_threads"):
        threading.setprofile_all_threads(None)
    else:
        threading.setprofile(None)
    sys.setprofile(None)


def parse_args(argv: list[str]) -> tuple[argparse.Namespace, list[str]]:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", required=True, type=Path, help="tree whose functions are recorded")
    parser.add_argument("--out", required=True, type=Path, help="oracle JSON to write")
    if "--" in argv:
        at = argv.index("--")
        return parser.parse_args(argv[:at]), argv[at + 1 :]
    return parser.parse_known_args(argv)


def main(argv: list[str] | None = None) -> int:
    args, pytest_args = parse_args(sys.argv[1:] if argv is None else argv)
    root = Path(os.path.realpath(args.root))
    import pytest  # noqa: PLC0415 - imported before profiling so its import is not traced

    oracle = Oracle(root, {os.path.realpath(__file__)})
    install(oracle.make_profile())
    try:
        code = pytest.main(pytest_args or [str(root)])
    finally:
        uninstall()
    code = int(code)
    if code in (0, 1) or oracle.edges:
        result = oracle.result()
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(result, indent=1) + "\n")
        print(
            f"oracle_py: {len(result['edges'])} edges, {len(result['executed_functions'])} functions executed, "
            f"pytest exit {code} -> {args.out}",
            file=sys.stderr,
        )
    else:
        print(f"oracle_py: pytest exit {code}, no tests ran; {args.out} not written", file=sys.stderr)
    return code


if __name__ == "__main__":
    sys.exit(main())
