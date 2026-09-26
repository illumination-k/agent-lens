"""Tests for oracle_rs.py.

The lexer and symbol helpers run everywhere; the end-to-end test drives
rust-analyzer over a tiny cargo project and is skipped when rust-analyzer is
not installed (`rustup component add rust-analyzer`):

    python3 -m unittest scripts/callgraph-accuracy/test_oracle_rs.py
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import oracle_rs  # noqa: E402


def calls_in_macros(src: str) -> list[str]:
    """The identifiers macro_call_tokens reports, by text."""
    out = []
    for offset in oracle_rs.macro_call_tokens(src):
        end = offset
        while end < len(src) and (src[end].isalnum() or src[end] == "_"):
            end += 1
        out.append(src[offset:end])
    return out


class MacroCallTokensTest(unittest.TestCase):
    def test_call_shapes_inside_macro_arguments(self):
        src = 'fn t() { assert_eq!(f(1), a.g(2)); vec![h::<u8>(3), m::k(4)]; }'
        self.assertEqual(calls_in_macros(src), ["f", "g", "h", "k"])

    def test_calls_outside_macros_are_left_to_the_call_hierarchy(self):
        self.assertEqual(calls_in_macros("fn t() { f(1); a.g(2); }"), [])

    def test_nested_macro_names_and_declared_fns_are_not_calls(self):
        src = "fn t() { assert!(format!(\"{}\", f()).is_empty()); m! { fn helper() {} } }"
        self.assertEqual(calls_in_macros(src), ["f", "is_empty"])

    def test_macro_rules_bodies_are_skipped(self):
        src = "macro_rules! m { ($e:expr) => { f($e) }; }\nfn t() { m!(g(1)); }"
        self.assertEqual(calls_in_macros(src), ["g"])

    def test_strings_chars_lifetimes_and_comments_do_not_confuse_the_lexer(self):
        src = textwrap.dedent(
            """\
            fn t<'a>(x: &'a str) {
                // f(1) in a comment
                /* nested /* g(2) */ still comment */
                println!("h(3) in a string {}", real(x));
                let c = '(';
                let r = r#"raw k(4) "quoted" "#;
                debug_assert!(b'x' == other(c));
            }
            """
        )
        self.assertEqual(calls_in_macros(src), ["real", "other"])


class HelpersTest(unittest.TestCase):
    def test_line_col_counts_utf16_columns(self):
        src = "let s = \"é😀\"; f()"
        line, col = oracle_rs.line_col(src, src.index("f()"))
        self.assertEqual((line, col), (0, len("let s = \"é") + 2 + len("\"; ")))
        self.assertEqual(oracle_rs.line_col("a\nbc", 3), (1, 1))

    def test_function_symbols_marks_trait_members(self):
        def sym(kind, line, children=()):
            pos = {"line": line, "character": 4}
            return {"kind": kind, "selectionRange": {"start": pos}, "range": {"start": pos, "end": {"line": line + 2, "character": 0}}, "children": list(children)}

        symbols = [sym(12, 0), sym(11, 3, [sym(6, 4)]), sym(19, 8, [sym(6, 9, [sym(12, 10)])])]
        self.assertEqual(
            list(oracle_rs.function_symbols(symbols)),
            [(0, 4, 0, 2, False), (4, 4, 4, 6, True), (9, 4, 9, 11, False), (10, 4, 10, 12, False)],
        )


def rust_analyzer() -> str | None:
    ra = os.environ.get("RUST_ANALYZER") or shutil.which("rust-analyzer")
    if ra is None:
        return None
    # The rustup proxy exists even when the component is not installed.
    ok = subprocess.run([ra, "--version"], capture_output=True).returncode == 0
    return ra if ok else None


LIB = textwrap.dedent(
    """\
    pub fn helper(x: u32) -> u32 {
        x + 1
    }

    pub trait Shape {
        fn area(&self) -> u32;
    }

    pub struct Sq(pub u32);

    impl Shape for Sq {
        fn area(&self) -> u32 {
            helper(self.0)
        }
    }

    impl std::str::FromStr for Sq {
        type Err = ();
        fn from_str(s: &str) -> Result<Self, ()> {
            Ok(Sq(s.len() as u32))
        }
    }

    pub fn run(s: &dyn Shape) -> u32 {
        let sq: Sq = "abc".parse().unwrap();
        let direct = <Sq as std::str::FromStr>::from_str("x").unwrap();
        assert_eq!(helper(1), 2);
        s.area() + sq.area() + direct.area()
    }

    #[cfg(feature = "never")]
    pub fn inactive() -> u32 {
        helper(0)
    }
    """
)


@unittest.skipIf(rust_analyzer() is None, "rust-analyzer not installed")
class EndToEndTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        root = Path(cls.tmp.name)
        (root / "src").mkdir()
        (root / "Cargo.toml").write_text('[package]\nname = "sample"\nversion = "0.1.0"\nedition = "2021"\n\n[features]\nnever = []\n\n[workspace]\n')
        (root / "src" / "lib.rs").write_text(LIB)
        # No feature enabled: `inactive` is cfg'd out.
        cls.doc, cls.stats = oracle_rs.run(root, rust_analyzer(), features=[])

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def edges(self) -> set[tuple[int, int, int, str]]:
        return {(e["caller_def_line"], e["call_line"], e["callee_def_line"], e["dispatch"]) for e in self.doc["edges"]}

    def test_static_calls(self):
        self.assertIn((12, 13, 1, "static"), self.edges())  # Sq::area -> helper
        self.assertIn((24, 28, 12, "static"), self.edges())  # sq.area() on a concrete Sq

    def test_std_trait_method_resolves_to_the_impl(self):
        self.assertIn((24, 26, 19, "static"), self.edges())  # <Sq as FromStr>::from_str

    def test_call_inside_macro_arguments(self):
        self.assertIn((24, 27, 1, "static"), self.edges())  # assert_eq!(helper(1), 2)

    def test_dyn_call_is_dynamic_to_the_trait_method_and_its_impls(self):
        self.assertIn((24, 28, 6, "dynamic"), self.edges())
        self.assertIn((24, 28, 12, "dynamic"), self.edges())

    def test_cfg_inactive_function_is_reported_unanalyzed(self):
        self.assertIn({"file": "src/lib.rs", "def_line": 32}, self.doc["unanalyzed_functions"])
        self.assertNotIn(32, {e["caller_def_line"] for e in self.doc["edges"]})
        self.assertEqual(self.doc["analyzed_files"], ["src/lib.rs"])


if __name__ == "__main__":
    unittest.main()
