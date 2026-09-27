"""Tests for oracle_pyright.py.

    python3 -m unittest scripts/callgraph-accuracy/test_oracle_pyright.py

The end-to-end tests need pyright's language server: `pyright-langserver` on
PATH, or PYRIGHT_LANGSERVER naming the command; they skip without it.
"""

from __future__ import annotations

import os
import shlex
import shutil
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import oracle_pyright  # noqa: E402

MOD = textwrap.dedent(
    """\
    from abc import ABC, abstractmethod
    from typing import overload


    def helper(x):
        return x + 1


    def direct():
        return helper(1)  # @direct-call


    class A:
        def __init__(self):  # @a-init
            self.v = 0

        def run(self):
            return self.step()  # @self-call

        def step(self):
            return 1

        @property
        def prop(self):
            return 2


    class B(A):
        pass


    class C(A):
        def __init__(self):  # @c-init
            super().__init__()  # @super-call


    def make():
        return A(), B(), C()  # @ctor-line


    def deco(fn):
        def wrapper(*a):
            return fn(*a)  # @wrapper-call
        return wrapper


    @deco
    def decorated(x):  # @decorated-def
        return x


    def uses_decorated():
        return decorated(3)  # @decorated-call


    def apply(fn):
        return fn(1)  # @param-call


    alias = helper


    def uses_alias():
        return alias(2)  # @alias-call


    class Base(ABC):
        @abstractmethod
        def go(self): ...


    class Impl(Base):
        def go(self):
            return 1


    class Other(Base):
        def go(self):
            return 2


    def dyn(b: Base):
        return b.go()  # @abstract-call


    def untyped(b):
        return b.run()  # @untyped-call


    def uses_prop():
        a = A()
        return a.prop  # @prop-read


    def nested():
        f = lambda v: helper(v)  # @lambda-line
        return [helper(i) for i in range(2)], f(1)  # @comp-line


    @overload
    def over(x: int) -> int: ...
    @overload
    def over(x: str) -> str: ...
    def over(x):  # @over-impl
        return x


    def uses_over():
        return over(1)  # @over-call


    helper(0)  # @module-call
    """
)

TEST = textwrap.dedent(
    """\
    import pkg


    def test_barrel():
        assert pkg.helper(1) == 2  # @barrel-call
    """
)

M = "pkg/mod.py"


def line(src: str, marker: str) -> int:
    for number, text in enumerate(src.splitlines(), 1):
        if text.rstrip().endswith("# " + marker):
            return number
    raise AssertionError(f"marker {marker!r} not found")


def def_line(src: str, name: str) -> int:
    for number, text in enumerate(src.splitlines(), 1):
        if text.lstrip().startswith((f"def {name}(", f"async def {name}(")):
            return number
    raise AssertionError(f"def {name} not found")


def langserver() -> list[str] | None:
    cmd = shlex.split(os.environ.get("PYRIGHT_LANGSERVER", "pyright-langserver"))
    return cmd if cmd and shutil.which(cmd[0]) else None


class ModuleTest(unittest.TestCase):
    """ast facts, no pyright needed."""

    def module(self, src: str) -> oracle_pyright.Module:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        path = Path(tmp.name) / "m.py"
        path.write_text(src, encoding="utf-8")
        return oracle_pyright.Module(path, "m.py")

    def test_positions_are_utf16_columns_of_the_name(self):
        mod = self.module("def f():\n    s = '𝔘'; g.h(1)\n    k()\n")
        sites = {(s.name, s.line, s.col) for s in mod.calls}
        # '𝔘' is 4 UTF-8 bytes but 2 UTF-16 code units: `h` is at byte 18, column 16.
        self.assertEqual(sites, {("h", 2, 16), ("k", 3, 4)})
        self.assertEqual(mod.functions[1].col, 4)

    def test_callers_are_the_lexically_enclosing_def(self):
        src = textwrap.dedent(
            """\
            @deco(a())
            def f(x=b()):
                g = lambda: c()
                def inner():
                    return d()
                return [e() for _ in g()]
            class K:
                z = h()
            """
        )
        callers = {s.name: s.caller for s in self.module(src).calls}
        self.assertEqual(callers, {"deco": None, "a": None, "b": None, "c": 2, "d": 4, "e": 2, "g": 2, "h": None})

    def test_abstract_methods(self):
        src = textwrap.dedent(
            """\
            class P(Protocol):
                def a(self): return 1
            class Q:
                @abstractmethod
                def b(self): return 1
                def c(self):
                    "doc"
                    raise NotImplementedError("x")
                def d(self): ...
                def e(self): pass
                def f(self):
                    if x:
                        raise NotImplementedError
            def g(): ...
            """
        )
        fns = self.module(src).functions
        abstract = {fn.name for fn in fns.values() if fn.abstract}
        self.assertEqual(abstract, {"a", "b", "c", "d"})

    def test_decorators_and_aliases(self):
        src = "class A:\n    @x.setter\n    def x(self, v): pass\n    @functools.cached_property\n    def y(self): pass\nf = g\nh = m.k\nn = call()\n"
        mod = self.module(src)
        self.assertIn("property", mod.functions[3].decorators)
        self.assertIn("cached_property", mod.functions[5].decorators)
        self.assertEqual(mod.aliases, {(6, 0): (5, 4), (7, 0): (6, 6)})
        self.assertEqual(mod.classes[1].methods, {"x": 3, "y": 5})

    def test_syntax_error_is_not_parsed(self):
        self.assertFalse(self.module("def f(:\n").parsed)


@unittest.skipIf(langserver() is None, "pyright-langserver is not installed")
class OracleTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        root = Path(cls.tmp.name) / "proj"
        (root / "pkg").mkdir(parents=True)
        (root / "pkg" / "__init__.py").write_text("from .mod import *\n")
        (root / "pkg" / "mod.py").write_text(MOD)
        (root / "test_mod.py").write_text(TEST)
        (root / ".venv").mkdir()
        (root / ".venv" / "skipped.py").write_text("def skipped():\n    pass\n")
        cls.doc, cls.stats = oracle_pyright.run(root, langserver())
        cls.root = root

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def edges_at(self, call_line: int, file: str = M) -> set[tuple[int, str]]:
        return {
            (e["callee_def_line"], e["dispatch"])
            for e in self.doc["edges"]
            if (e["caller_file"], e["call_line"]) == (file, call_line)
        }

    def assertEdge(self, caller: str | int, marker: str, callee_def: int, dispatch: str = "static", src: str = MOD, file: str = M):
        want = {
            "caller_file": file,
            "caller_def_line": caller if isinstance(caller, int) else def_line(src, caller),
            "call_line": line(src, marker),
            "callee_file": M,
            "callee_def_line": callee_def,
            "dispatch": dispatch,
        }
        self.assertIn(want, self.doc["edges"])

    def test_document_shape(self):
        self.assertEqual(
            {k: self.doc[k] for k in ("oracle", "language", "kind")},
            {"oracle": "python-pyright", "language": "python", "kind": "type-checker"},
        )
        self.assertEqual(self.doc["root"], str(self.root.resolve()))
        self.assertEqual(self.doc["analyzed_files"], ["pkg/__init__.py", "pkg/mod.py", "test_mod.py"])

    def test_direct_self_and_barrel(self):
        self.assertEdge("direct", "@direct-call", def_line(MOD, "helper"))
        self.assertEdge("run", "@self-call", def_line(MOD, "step"))
        self.assertEdge("test_barrel", "@barrel-call", def_line(MOD, "helper"), src=TEST, file="test_mod.py")

    def test_constructors_follow_the_mro(self):
        a_init, c_init = line(MOD, "@a-init"), line(MOD, "@c-init")
        self.assertEqual(self.edges_at(line(MOD, "@ctor-line")), {(a_init, "static"), (c_init, "static")})
        self.assertEdge(c_init, "@super-call", a_init)

    def test_decorated_function_is_the_callee(self):
        self.assertEdge("uses_decorated", "@decorated-call", line(MOD, "@decorated-def"))
        self.assertEqual(self.edges_at(line(MOD, "@wrapper-call")), set())

    def test_call_through_a_parameter_has_no_edge(self):
        # `fn` is declared on the `def apply(fn)` line: the column keeps it from reading as `apply`.
        self.assertEqual(self.edges_at(line(MOD, "@param-call")), set())

    def test_alias_is_followed(self):
        self.assertEdge("uses_alias", "@alias-call", def_line(MOD, "helper"))

    def test_abstract_method_fans_out_to_overrides(self):
        at = line(MOD, "@abstract-call")
        go = [n for n, text in enumerate(MOD.splitlines(), 1) if text.strip().startswith("def go(")]
        self.assertEqual(self.edges_at(at), {(n, "dynamic") for n in go})

    def test_unknown_receiver_is_an_unresolved_call_site(self):
        self.assertEqual(self.edges_at(line(MOD, "@untyped-call")), set())
        self.assertIn({"file": M, "line": line(MOD, "@untyped-call"), "name": "run"}, self.doc["unresolved_call_sites"])

    def test_property_read_is_dynamic(self):
        self.assertEdge("uses_prop", "@prop-read", def_line(MOD, "prop"), "dynamic")

    def test_lambda_and_comprehension_belong_to_the_enclosing_def(self):
        self.assertEdge("nested", "@lambda-line", def_line(MOD, "helper"))
        self.assertEdge("nested", "@comp-line", def_line(MOD, "helper"))

    def test_overloads_keep_the_implementation(self):
        self.assertEqual(self.edges_at(line(MOD, "@over-call")), {(line(MOD, "@over-impl"), "static")})

    def test_module_level_calls_are_counted_not_emitted(self):
        self.assertEqual(self.edges_at(line(MOD, "@module-call")), set())
        self.assertGreater(self.stats["call_site_caller_module_or_class"], 0)


if __name__ == "__main__":
    unittest.main()
