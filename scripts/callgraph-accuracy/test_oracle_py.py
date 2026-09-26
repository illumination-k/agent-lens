"""Tests for oracle_py.py on a tiny package covering the call shapes it must handle.

Run with pytest available to the interpreter, e.g.:

    uv run --no-project --with pytest python -m unittest scripts/callgraph-accuracy/test_oracle_py.py
"""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

ORACLE = Path(__file__).resolve().parent / "oracle_py.py"

MOD = textwrap.dedent(
    """\
    import functools
    import threading


    def helper(x):
        return x + 1


    def direct():
        return helper(1)  # @direct-call


    class A:
        def __init__(self):
            self.v = 0

        def run(self):
            return self.step()  # @self-call

        def step(self):
            return 1

        @property
        def prop(self):
            return 2


    def make():
        return A()  # @ctor-call


    def deco(fn):
        @functools.wraps(fn)
        def wrapper(*a):
            return fn(*a)  # @wrapper-call
        return wrapper


    @deco
    @functools.lru_cache(maxsize=None)
    def decorated(x):  # @decorated-def
        return helper(x)


    def uses_decorated():
        return decorated(3)  # @decorated-call


    def outer():
        def inner():  # @inner-def
            return helper(2)  # @inner-call
        return inner()  # @outer-call


    def uses_lambda():
        g = lambda v: helper(v)  # @lambda-line
        return g(1)


    def key(x):
        return -x


    def uses_sorted():
        return sorted([1, 2], key=key)  # @sorted-call


    def gen():
        yield helper(0)
        yield 1


    def consume():
        g = gen()
        first = next(g)  # @gen-first
        second = next(g)  # @gen-second
        return first, second


    def loop():
        out = []
        for v in gen():  # @gen-loop
            out.append(v)
        return out


    class Box:
        def __iter__(self):
            return iter([1])

        def __len__(self):
            return 1


    def protocols():
        b = Box()
        return list(b), len(b)  # @protocol-line


    def comp():
        return [helper(i) for i in range(2)]  # @comp-line


    def uses_prop():
        return A().prop  # @prop-line


    def worker(out):
        out.append(helper(5))  # @worker-call


    def threaded():
        out = []
        t = threading.Thread(target=worker, args=(out,))
        t.start()
        t.join()
        return out


    def uses_map():
        return list(map(key, [1, 2]))  # @map-call


    def apply(fn):
        return fn(1)  # @param-call


    def uses_apply():
        return apply(key)


    class P:
        def __init__(self, v):  # @p-init
            self.v = v


    class Q(P):
        pass


    def map_ctor():
        return list(map(P, [1, 2]))  # @map-ctor


    def cls_param(cls):
        return cls(5)  # @cls-param


    def uses_cls_param():
        return cls_param(P).v


    def subclass_ctor():
        return Q(3).v  # @sub-ctor


    def rate():
        yield 1


    def iterate(v):
        return v


    def rate_consumer(g):
        return iterate(next(g))  # @rate-consumer


    def rate_producer():
        return rate_consumer(rate())
    """
)

TEST = textwrap.dedent(
    """\
    from pkg import mod


    def test_all():
        assert mod.direct() == 2
        assert mod.A().run() == 1
        assert mod.make().v == 0
        assert mod.uses_decorated() == 4
        assert mod.outer() == 3
        assert mod.uses_lambda() == 2
        assert mod.uses_sorted() == [2, 1]
        assert mod.consume() == (1, 1)
        assert mod.loop() == [1, 1]
        assert mod.protocols() == ([1], 1)
        assert mod.comp() == [1, 2]
        assert mod.uses_prop() == 2
        assert mod.threaded() == [6]
        assert mod.uses_map() == [-1, -2]
        assert mod.uses_apply() == -1
        assert [p.v for p in mod.map_ctor()] == [1, 2]
        assert mod.uses_cls_param() == 5
        assert mod.subclass_ctor() == 3
        assert mod.rate_producer() == 1


    def test_fails():
        mod.key(1)  # @test-call
        assert False
    """
)


def line(src: str, marker: str) -> int:
    for number, text in enumerate(src.splitlines(), 1):
        if text.rstrip().endswith("# " + marker) or text.strip() == marker or text.strip().startswith(marker + " "):
            return number
    raise AssertionError(f"marker {marker!r} not found")


def def_line(src: str, name: str) -> int:
    for number, text in enumerate(src.splitlines(), 1):
        if text.lstrip().startswith((f"def {name}(", f"async def {name}(")):
            return number
    raise AssertionError(f"def {name} not found")


M = "pkg/mod.py"


@unittest.skipIf(importlib.util.find_spec("pytest") is None, "pytest is not importable")
class OracleTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        root = Path(cls.tmp.name) / "proj"
        (root / "pkg").mkdir(parents=True)
        (root / "pkg" / "__init__.py").write_text("")
        (root / "pkg" / "mod.py").write_text(MOD)
        (root / "test_mod.py").write_text(TEST)
        out = Path(cls.tmp.name) / "out" / "oracle.json"
        cls.proc = subprocess.run(
            [
                sys.executable,
                str(ORACLE),
                "--root",
                str(root),
                "--out",
                str(out),
                "--",
                "-q",
                "-p",
                "no:cacheprovider",
                str(root),
            ],
            cwd=root,
            capture_output=True,
            text=True,
            check=False,
        )
        cls.root = root
        cls.result = json.loads(out.read_text()) if out.exists() else None

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def edges_to(self, callee_file: str, callee_def: int) -> list[dict]:
        return [
            e for e in self.result["edges"] if (e["callee_file"], e["callee_def_line"]) == (callee_file, callee_def)
        ]

    def assertEdge(
        self,
        caller: str,
        call_marker: str,
        callee: str,
        dispatch: str = "static",
        caller_file: str = M,
        callee_file: str = M,
        callee_def: int | None = None,
    ):
        src = {M: MOD, "test_mod.py": TEST}
        want = {
            "caller_file": caller_file,
            "caller_def_line": def_line(src[caller_file], caller),
            "call_line": line(src[caller_file], call_marker),
            "callee_file": callee_file,
            "callee_def_line": callee_def or def_line(src[callee_file], callee),
            "dispatch": dispatch,
        }
        self.assertIn(want, self.result["edges"])

    def test_exit_code_propagates_and_json_written(self):
        self.assertEqual(self.proc.returncode, 1, self.proc.stderr + self.proc.stdout)
        self.assertIsNotNone(self.result)
        self.assertEqual(
            {k: self.result[k] for k in ("oracle", "language", "kind")},
            {"oracle": "python-setprofile", "language": "python", "kind": "dynamic"},
        )
        self.assertEqual(self.result["root"], str(self.root.resolve()))
        self.assertIn("oracle_py:", self.proc.stderr)

    def test_direct_self_and_constructor(self):
        self.assertEdge("direct", "@direct-call", "helper")
        self.assertEdge("run", "@self-call", "step")
        self.assertEdge("make", "@ctor-call", "__init__")

    def test_edges_from_tests_but_not_from_pytest(self):
        self.assertEdge("test_fails", "@test-call", "key", caller_file="test_mod.py")
        self.assertEqual(self.edges_to("test_mod.py", def_line(TEST, "test_all")), [])

    def test_decorated_callee_uses_def_line(self):
        decorated = line(MOD, "@decorated-def")
        # `decorated(3)` runs the wrapper deco returned, and the wrapper calls
        # its parameter: both are calls through a value bound to another name.
        self.assertEdge("uses_decorated", "@decorated-call", "wrapper", dispatch="dynamic")
        self.assertEdge("wrapper", "@wrapper-call", "decorated", dispatch="dynamic", callee_def=decorated)
        self.assertIn({"file": M, "def_line": decorated}, self.result["executed_functions"])
        self.assertNotIn({"file": M, "def_line": decorated - 2}, self.result["executed_functions"])

    def test_nested_def(self):
        self.assertEdge("outer", "@outer-call", "inner")
        self.assertEdge("inner", "@inner-call", "helper")

    def test_lambda_is_not_a_callee_and_attributes_to_enclosing_def(self):
        self.assertEdge("uses_lambda", "@lambda-line", "helper")
        lam = line(MOD, "@lambda-line")
        self.assertFalse([e for e in self.result["edges"] if e["callee_file"] == M and e["callee_def_line"] == lam])
        self.assertGreater(self.result["stats"].get("no_node_code:<lambda>", 0), 0)

    def test_callback(self):
        self.assertEdge("uses_sorted", "@sorted-call", "key", dispatch="callback")
        # `map` is a C type (no c_call event); the line names `key` without calling it.
        self.assertEdge("uses_map", "@map-call", "key", dispatch="callback")

    def test_call_through_a_parameter_is_dynamic(self):
        self.assertEdge("apply", "@param-call", "key", dispatch="dynamic")

    def test_generator_recorded_once_where_named(self):
        # Resumes are not calls; `next(g)` does not name `gen`, so its creator is unknown.
        self.assertEqual([e["call_line"] for e in self.edges_to(M, def_line(MOD, "gen"))], [line(MOD, "@gen-loop")])
        self.assertEdge("loop", "@gen-loop", "gen")
        self.assertGreater(self.result["stats"].get("call_site_generator_first_advanced_elsewhere", 0), 0)

    def test_constructor_dispatch_follows_the_call_line(self):
        p_init = line(MOD, "@p-init")
        self.assertEdge("subclass_ctor", "@sub-ctor", "__init__", callee_def=p_init)
        self.assertEdge("map_ctor", "@map-ctor", "__init__", dispatch="callback", callee_def=p_init)
        self.assertEdge("cls_param", "@cls-param", "__init__", dispatch="dynamic", callee_def=p_init)

    def test_generator_not_attributed_to_a_line_calling_a_longer_name(self):
        # `iterate(next(g))` contains "rate(" but does not call `rate`.
        self.assertEqual(self.edges_to(M, def_line(MOD, "rate")), [])

    def test_protocol_dunders_from_c_are_dynamic(self):
        self.assertEdge("protocols", "@protocol-line", "__iter__", dispatch="dynamic")
        self.assertEdge("protocols", "@protocol-line", "__len__", dispatch="dynamic")

    def test_comprehension(self):
        self.assertEdge("comp", "@comp-line", "helper")

    def test_property_is_dynamic(self):
        self.assertEdge("uses_prop", "@prop-line", "prop", dispatch="dynamic")

    def test_thread(self):
        self.assertEdge("worker", "@worker-call", "helper")
        # threading.Thread.run (stdlib) calling worker is not an edge.
        self.assertEqual(self.edges_to(M, def_line(MOD, "worker")), [])

    def test_no_module_or_class_body_callees(self):
        for e in self.result["edges"]:
            self.assertNotEqual(e["callee_def_line"], 1)
        self.assertEqual(len(self.result["edges"]), len({json.dumps(e, sort_keys=True) for e in self.result["edges"]}))


if __name__ == "__main__":
    unittest.main()
