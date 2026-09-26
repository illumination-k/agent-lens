//! Metamorphic tests for the call-graph resolver (#579).
//!
//! Each language has one small fixture project — a few modules, imports,
//! a class or receiver type with methods, free functions, and a `decoy`
//! module that defines the same names as the real callees so that a call
//! only resolves when the resolver actually follows the import. Every
//! test applies a semantics-preserving rewrite to that project and
//! requires the normalised edge set to come out identical:
//!
//! * **rename** — every function gets a generated name (proptest, fixed
//!   seed). Nothing but spelling changes, so target, resolution kind *and*
//!   [`ResolutionMethod`] must all match.
//! * **alias** — imports are renamed at the import site (`use a::f as g`,
//!   `import { f as g }`, `from m import f as g`, `import n "path"`). The
//!   import is still the evidence, so target, resolution kind and method
//!   must all match.
//! * **split** — a function moves to a new module and every caller's
//!   import follows it. Target and resolution kind must match; the method
//!   may change (a same-module lexical call becomes an imported one).
//! * **barrel** — callers import through a re-exporting module (`pub use`,
//!   `export … from`, a package `__init__.py`). Go has no re-exports.
//!   Target and resolution kind must match; the method may change.
//!
//! An edge is normalised to its caller, its target (node, sorted
//! candidate set, or the bare callee name when unresolved), its
//! resolution kind, and its call count. Node names are mapped back
//! through the rewrite (a renamed function to its original name, a moved
//! one to its original module) before comparing. Call lines are not
//! compared: every rewrite shifts them.
//!
//! A difference the resolver cannot close cheaply is pinned in
//! [`KNOWN_GAPS`] rather than ignored, so the test starts failing — and
//! the table must be trimmed — the moment the gap closes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use rstest::rstest;

use super::model::{GraphLanguage, Resolution, ResolutionMethod};
use super::{CallGraph, CallGraphBuilder};
use crate::analyze::AnalyzeRoots;
use crate::test_support::write_file;

/// One fixture file: path relative to the project root and its source.
/// Function names are written `@name@` so the rename rewrite can
/// substitute them; every other rewrite renders them verbatim.
type FixtureFile = (&'static str, &'static str);

struct Project {
    language: GraphLanguage,
    files: &'static [FixtureFile],
}

const RUST: Project = Project {
    language: GraphLanguage::Rust,
    files: &[
        (
            "src/lib.rs",
            "pub mod decoy;\n\
             pub mod geo;\n\
             pub mod text;\n\
             \n\
             use crate::geo::@area@;\n\
             use crate::text::{@shout@, Greeter};\n\
             \n\
             pub fn @run@() -> usize {\n\
             \x20   let g = Greeter::new();\n\
             \x20   let s = g.@greet@();\n\
             \x20   @area@(2) + @shout@(&s).len() + geo::@perimeter@(3)\n\
             }\n",
        ),
        (
            "src/geo.rs",
            "pub fn @area@(x: usize) -> usize {\n\
             \x20   @square@(x)\n\
             }\n\
             \n\
             fn @square@(x: usize) -> usize {\n\
             \x20   x * x\n\
             }\n\
             \n\
             pub fn @perimeter@(x: usize) -> usize {\n\
             \x20   super::text::@count@(x) * 4\n\
             }\n",
        ),
        (
            "src/text.rs",
            "pub struct Greeter;\n\
             \n\
             impl Greeter {\n\
             \x20   pub fn new() -> Self {\n\
             \x20       Greeter\n\
             \x20   }\n\
             \n\
             \x20   pub fn @greet@(&self) -> String {\n\
             \x20       self.@salute@()\n\
             \x20   }\n\
             \n\
             \x20   fn @salute@(&self) -> String {\n\
             \x20       @shout@(\"hi\")\n\
             \x20   }\n\
             }\n\
             \n\
             pub fn @shout@(s: &str) -> String {\n\
             \x20   s.to_uppercase()\n\
             }\n\
             \n\
             pub fn @count@(x: usize) -> usize {\n\
             \x20   x\n\
             }\n",
        ),
        (
            "src/decoy.rs",
            "pub fn @area@(x: usize) -> usize {\n\
             \x20   x\n\
             }\n\
             \n\
             pub fn @shout@(s: &str) -> String {\n\
             \x20   s.to_owned()\n\
             }\n\
             \n\
             pub fn @perimeter@(x: usize) -> usize {\n\
             \x20   x\n\
             }\n\
             \n\
             pub fn @greet@() -> String {\n\
             \x20   String::new()\n\
             }\n",
        ),
    ],
};

const TYPESCRIPT: Project = Project {
    language: GraphLanguage::TypeScript,
    files: &[
        (
            "src/main.ts",
            "import { @area@ } from './geo';\n\
             import * as geo from './geo';\n\
             import { @shout@, Greeter } from './text';\n\
             \n\
             export function @run@(): number {\n\
             \x20 const g = new Greeter();\n\
             \x20 const s = g.@greet@();\n\
             \x20 return @area@(2) + @shout@(s).length + geo.@perimeter@(3);\n\
             }\n",
        ),
        (
            "src/geo.ts",
            "import { @count@ } from './text';\n\
             \n\
             export function @area@(x: number): number {\n\
             \x20 return @square@(x);\n\
             }\n\
             \n\
             function @square@(x: number): number {\n\
             \x20 return x * x;\n\
             }\n\
             \n\
             export function @perimeter@(x: number): number {\n\
             \x20 return @count@(x) * 4;\n\
             }\n",
        ),
        (
            "src/text.ts",
            "export class Greeter {\n\
             \x20 @greet@(): string {\n\
             \x20   return this.@salute@();\n\
             \x20 }\n\
             \n\
             \x20 @salute@(): string {\n\
             \x20   return @shout@('hi');\n\
             \x20 }\n\
             }\n\
             \n\
             export function @shout@(s: string): string {\n\
             \x20 return s.toUpperCase();\n\
             }\n\
             \n\
             export function @count@(x: number): number {\n\
             \x20 return x;\n\
             }\n",
        ),
        (
            "src/decoy.ts",
            "export function @area@(x: number): number {\n\
             \x20 return x;\n\
             }\n\
             \n\
             export function @shout@(s: string): string {\n\
             \x20 return s;\n\
             }\n\
             \n\
             export function @perimeter@(x: number): number {\n\
             \x20 return x;\n\
             }\n\
             \n\
             export function @greet@(): string {\n\
             \x20 return '';\n\
             }\n",
        ),
    ],
};

const PYTHON: Project = Project {
    language: GraphLanguage::Python,
    files: &[
        ("app/__init__.py", ""),
        (
            "app/main.py",
            "from app import geo\n\
             from app.geo import @area@\n\
             from app.text import @shout@\n\
             \n\
             \n\
             def @run@(g):\n\
             \x20   s = g.@greet@()\n\
             \x20   return @area@(2) + len(@shout@(s)) + geo.@perimeter@(3)\n",
        ),
        (
            "app/geo.py",
            "from app.text import @count@\n\
             \n\
             \n\
             def @area@(x):\n\
             \x20   return @square@(x)\n\
             \n\
             \n\
             def @square@(x):\n\
             \x20   return x * x\n\
             \n\
             \n\
             def @perimeter@(x):\n\
             \x20   return @count@(x) * 4\n",
        ),
        (
            "app/text.py",
            "class Greeter:\n\
             \x20   def @greet@(self):\n\
             \x20       return self.@salute@()\n\
             \n\
             \x20   def @salute@(self):\n\
             \x20       return @shout@(\"hi\")\n\
             \n\
             \n\
             def @shout@(s):\n\
             \x20   return s.upper()\n\
             \n\
             \n\
             def @count@(x):\n\
             \x20   return x\n",
        ),
        (
            "app/decoy.py",
            "def @area@(x):\n\
             \x20   return x\n\
             \n\
             \n\
             def @shout@(s):\n\
             \x20   return s\n\
             \n\
             \n\
             def @perimeter@(x):\n\
             \x20   return x\n\
             \n\
             \n\
             def @greet@():\n\
             \x20   return \"\"\n",
        ),
    ],
};

const GO: Project = Project {
    language: GraphLanguage::Go,
    files: &[
        ("go.mod", "module example.com/shapes\n\ngo 1.22\n"),
        (
            "app/app.go",
            "package app\n\
             \n\
             import (\n\
             \t\"example.com/shapes/geo\"\n\
             \t\"example.com/shapes/text\"\n\
             )\n\
             \n\
             func @Run@(g *text.Greeter) int {\n\
             \ts := g.@Greet@()\n\
             \treturn geo.@Area@(2) + len(text.@Shout@(s)) + geo.@Perimeter@(3)\n\
             }\n",
        ),
        (
            "geo/geo.go",
            "package geo\n\
             \n\
             import \"example.com/shapes/text\"\n\
             \n\
             func @Area@(x int) int { return @square@(x) }\n\
             \n\
             func @square@(x int) int { return x * x }\n\
             \n\
             func @Perimeter@(x int) int { return text.@Count@(x) * 4 }\n",
        ),
        (
            "text/text.go",
            "package text\n\
             \n\
             import \"strings\"\n\
             \n\
             type Greeter struct{}\n\
             \n\
             func (g *Greeter) @Greet@() string { return g.@salute@() }\n\
             \n\
             func (g *Greeter) @salute@() string { return @Shout@(\"hi\") }\n\
             \n\
             func @Shout@(s string) string { return strings.ToUpper(s) }\n\
             \n\
             func @Count@(x int) int { return x }\n",
        ),
        (
            "decoy/decoy.go",
            "package decoy\n\
             \n\
             func @Area@(x int) int { return x }\n\
             \n\
             func @Shout@(s string) string { return s }\n\
             \n\
             func @Perimeter@(x int) int { return x }\n\
             \n\
             func @Greet@() string { return \"\" }\n",
        ),
    ],
};

/// A semantics-preserving rewrite of one [`Project`].
struct Rewrite {
    project: &'static Project,
    /// Files replaced or added (by path).
    files: &'static [FixtureFile],
    /// Files deleted.
    removed: &'static [&'static str],
    /// `(new qualified name, original qualified name)` of every function
    /// the rewrite moved to another module.
    moved: &'static [(&'static str, &'static str)],
    /// Whether the [`ResolutionMethod`] of every edge must survive too,
    /// not only its target and resolution kind.
    same_method: bool,
}

const RUST_ALIAS: Rewrite = Rewrite {
    project: &RUST,
    files: &[(
        "src/lib.rs",
        "pub mod decoy;\n\
         pub mod geo;\n\
         pub mod text;\n\
         \n\
         use crate::geo as shapes;\n\
         use crate::geo::@area@ as surface;\n\
         use crate::text::{@shout@ as yell, Greeter as Hello};\n\
         \n\
         pub fn @run@() -> usize {\n\
         \x20   let g = Hello::new();\n\
         \x20   let s = g.@greet@();\n\
         \x20   surface(2) + yell(&s).len() + shapes::@perimeter@(3)\n\
         }\n",
    )],
    removed: &[],
    moved: &[],
    same_method: true,
};

/// 2018-edition uniform paths: a `use` naming a child module without
/// `crate::` / `self::` is relative to the current module.
const RUST_RELATIVE_ALIAS: Rewrite = Rewrite {
    project: &RUST,
    files: &[(
        "src/lib.rs",
        "pub mod decoy;\n\
         pub mod geo;\n\
         pub mod text;\n\
         \n\
         use self::geo as shapes;\n\
         use geo::@area@ as surface;\n\
         use text::{@shout@ as yell, Greeter as Hello};\n\
         \n\
         pub fn @run@() -> usize {\n\
         \x20   let g = Hello::new();\n\
         \x20   let s = g.@greet@();\n\
         \x20   surface(2) + yell(&s).len() + shapes::@perimeter@(3)\n\
         }\n",
    )],
    removed: &[],
    moved: &[],
    same_method: true,
};

const RUST_SPLIT: Rewrite = Rewrite {
    project: &RUST,
    files: &[
        (
            "src/lib.rs",
            "pub mod decoy;\n\
             pub mod geo;\n\
             pub mod loud;\n\
             pub mod measure;\n\
             pub mod text;\n\
             \n\
             use crate::geo::@area@;\n\
             use crate::loud::@shout@;\n\
             use crate::text::Greeter;\n\
             \n\
             pub fn @run@() -> usize {\n\
             \x20   let g = Greeter::new();\n\
             \x20   let s = g.@greet@();\n\
             \x20   @area@(2) + @shout@(&s).len() + measure::@perimeter@(3)\n\
             }\n",
        ),
        (
            "src/geo.rs",
            "pub fn @area@(x: usize) -> usize {\n\
             \x20   @square@(x)\n\
             }\n\
             \n\
             fn @square@(x: usize) -> usize {\n\
             \x20   x * x\n\
             }\n",
        ),
        (
            "src/measure.rs",
            "pub fn @perimeter@(x: usize) -> usize {\n\
             \x20   super::text::@count@(x) * 4\n\
             }\n",
        ),
        (
            "src/loud.rs",
            "pub fn @shout@(s: &str) -> String {\n\
             \x20   s.to_uppercase()\n\
             }\n",
        ),
        (
            "src/text.rs",
            "use crate::loud::@shout@;\n\
             \n\
             pub struct Greeter;\n\
             \n\
             impl Greeter {\n\
             \x20   pub fn new() -> Self {\n\
             \x20       Greeter\n\
             \x20   }\n\
             \n\
             \x20   pub fn @greet@(&self) -> String {\n\
             \x20       self.@salute@()\n\
             \x20   }\n\
             \n\
             \x20   fn @salute@(&self) -> String {\n\
             \x20       @shout@(\"hi\")\n\
             \x20   }\n\
             }\n\
             \n\
             pub fn @count@(x: usize) -> usize {\n\
             \x20   x\n\
             }\n",
        ),
    ],
    removed: &[],
    moved: &[
        ("crate::measure::perimeter", "crate::geo::perimeter"),
        ("crate::loud::shout", "crate::text::shout"),
    ],
    same_method: false,
};

const RUST_BARREL: Rewrite = Rewrite {
    project: &RUST,
    files: &[
        (
            "src/lib.rs",
            "pub mod decoy;\n\
             pub mod geo;\n\
             pub mod prelude;\n\
             pub mod text;\n\
             \n\
             use crate::prelude::{@area@, @shout@, Greeter};\n\
             \n\
             pub fn @run@() -> usize {\n\
             \x20   let g = Greeter::new();\n\
             \x20   let s = g.@greet@();\n\
             \x20   @area@(2) + @shout@(&s).len() + prelude::@perimeter@(3)\n\
             }\n",
        ),
        (
            "src/prelude.rs",
            "pub use crate::geo::{@area@, @perimeter@};\n\
             pub use crate::text::*;\n",
        ),
    ],
    removed: &[],
    moved: &[],
    same_method: false,
};

const TYPESCRIPT_ALIAS: Rewrite = Rewrite {
    project: &TYPESCRIPT,
    files: &[(
        "src/main.ts",
        "import { @area@ as surface } from './geo';\n\
         import * as shapes from './geo';\n\
         import { @shout@ as yell, Greeter as Hello } from './text';\n\
         \n\
         export function @run@(): number {\n\
         \x20 const g = new Hello();\n\
         \x20 const s = g.@greet@();\n\
         \x20 return surface(2) + yell(s).length + shapes.@perimeter@(3);\n\
         }\n",
    )],
    removed: &[],
    moved: &[],
    same_method: true,
};

const TYPESCRIPT_SPLIT: Rewrite = Rewrite {
    project: &TYPESCRIPT,
    files: &[
        (
            "src/main.ts",
            "import { @area@ } from './geo';\n\
             import * as measure from './measure';\n\
             import { @shout@ } from './loud';\n\
             import { Greeter } from './text';\n\
             \n\
             export function @run@(): number {\n\
             \x20 const g = new Greeter();\n\
             \x20 const s = g.@greet@();\n\
             \x20 return @area@(2) + @shout@(s).length + measure.@perimeter@(3);\n\
             }\n",
        ),
        (
            "src/geo.ts",
            "export function @area@(x: number): number {\n\
             \x20 return @square@(x);\n\
             }\n\
             \n\
             function @square@(x: number): number {\n\
             \x20 return x * x;\n\
             }\n",
        ),
        (
            "src/measure.ts",
            "import { @count@ } from './text';\n\
             \n\
             export function @perimeter@(x: number): number {\n\
             \x20 return @count@(x) * 4;\n\
             }\n",
        ),
        (
            "src/loud.ts",
            "export function @shout@(s: string): string {\n\
             \x20 return s.toUpperCase();\n\
             }\n",
        ),
        (
            "src/text.ts",
            "import { @shout@ } from './loud';\n\
             \n\
             export class Greeter {\n\
             \x20 @greet@(): string {\n\
             \x20   return this.@salute@();\n\
             \x20 }\n\
             \n\
             \x20 @salute@(): string {\n\
             \x20   return @shout@('hi');\n\
             \x20 }\n\
             }\n\
             \n\
             export function @count@(x: number): number {\n\
             \x20 return x;\n\
             }\n",
        ),
    ],
    removed: &[],
    moved: &[
        ("src::measure::perimeter", "src::geo::perimeter"),
        ("src::loud::shout", "src::text::shout"),
    ],
    same_method: false,
};

/// `./geo` and `./geo/index` name the same module once `geo.ts` becomes
/// the directory's `index.ts`.
const TYPESCRIPT_INDEX_MODULE: Rewrite = Rewrite {
    project: &TYPESCRIPT,
    files: &[
        (
            "src/main.ts",
            "import { @area@ } from './geo/index';\n\
             import * as geo from './geo/index.ts';\n\
             import { @shout@, Greeter } from './text';\n\
             \n\
             export function @run@(): number {\n\
             \x20 const g = new Greeter();\n\
             \x20 const s = g.@greet@();\n\
             \x20 return @area@(2) + @shout@(s).length + geo.@perimeter@(3);\n\
             }\n",
        ),
        (
            "src/geo/index.ts",
            "import { @count@ } from '../text';\n\
             \n\
             export function @area@(x: number): number {\n\
             \x20 return @square@(x);\n\
             }\n\
             \n\
             function @square@(x: number): number {\n\
             \x20 return x * x;\n\
             }\n\
             \n\
             export function @perimeter@(x: number): number {\n\
             \x20 return @count@(x) * 4;\n\
             }\n",
        ),
    ],
    removed: &["src/geo.ts"],
    moved: &[],
    same_method: true,
};

const TYPESCRIPT_BARREL: Rewrite = Rewrite {
    project: &TYPESCRIPT,
    files: &[
        (
            "src/main.ts",
            "import { @area@, @shout@, Greeter } from './index';\n\
             import * as lib from './index';\n\
             \n\
             export function @run@(): number {\n\
             \x20 const g = new Greeter();\n\
             \x20 const s = g.@greet@();\n\
             \x20 return @area@(2) + @shout@(s).length + lib.@perimeter@(3);\n\
             }\n",
        ),
        (
            "src/index.ts",
            "export { @area@, @perimeter@ } from './geo';\n\
             export * from './text';\n",
        ),
    ],
    removed: &[],
    moved: &[],
    same_method: false,
};

const PYTHON_ALIAS: Rewrite = Rewrite {
    project: &PYTHON,
    files: &[(
        "app/main.py",
        "import app.geo as shapes\n\
         from app.geo import @area@ as surface\n\
         from app.text import @shout@ as yell\n\
         \n\
         \n\
         def @run@(g):\n\
         \x20   s = g.@greet@()\n\
         \x20   return surface(2) + len(yell(s)) + shapes.@perimeter@(3)\n",
    )],
    removed: &[],
    moved: &[],
    same_method: true,
};

const PYTHON_RELATIVE_ALIAS: Rewrite = Rewrite {
    project: &PYTHON,
    files: &[(
        "app/main.py",
        "from . import geo as shapes\n\
         from .geo import @area@ as surface\n\
         from .text import @shout@ as yell\n\
         \n\
         \n\
         def @run@(g):\n\
         \x20   s = g.@greet@()\n\
         \x20   return surface(2) + len(yell(s)) + shapes.@perimeter@(3)\n",
    )],
    removed: &[],
    moved: &[],
    same_method: true,
};

const PYTHON_SPLIT: Rewrite = Rewrite {
    project: &PYTHON,
    files: &[
        (
            "app/main.py",
            "from app import measure\n\
             from app.geo import @area@\n\
             from app.loud import @shout@\n\
             \n\
             \n\
             def @run@(g):\n\
             \x20   s = g.@greet@()\n\
             \x20   return @area@(2) + len(@shout@(s)) + measure.@perimeter@(3)\n",
        ),
        (
            "app/geo.py",
            "def @area@(x):\n\
             \x20   return @square@(x)\n\
             \n\
             \n\
             def @square@(x):\n\
             \x20   return x * x\n",
        ),
        (
            "app/measure.py",
            "from app.text import @count@\n\
             \n\
             \n\
             def @perimeter@(x):\n\
             \x20   return @count@(x) * 4\n",
        ),
        (
            "app/loud.py",
            "def @shout@(s):\n\
             \x20   return s.upper()\n",
        ),
        (
            "app/text.py",
            "from app.loud import @shout@\n\
             \n\
             \n\
             class Greeter:\n\
             \x20   def @greet@(self):\n\
             \x20       return self.@salute@()\n\
             \n\
             \x20   def @salute@(self):\n\
             \x20       return @shout@(\"hi\")\n\
             \n\
             \n\
             def @count@(x):\n\
             \x20   return x\n",
        ),
    ],
    removed: &[],
    moved: &[
        ("app::measure::perimeter", "app::geo::perimeter"),
        ("app::loud::shout", "app::text::shout"),
    ],
    same_method: false,
};

const PYTHON_BARREL: Rewrite = Rewrite {
    project: &PYTHON,
    files: &[
        (
            "app/__init__.py",
            "from app.geo import @area@, @perimeter@\n\
             from app.text import @shout@\n",
        ),
        (
            "app/main.py",
            "import app\n\
             from app import @area@, @shout@\n\
             \n\
             \n\
             def @run@(g):\n\
             \x20   s = g.@greet@()\n\
             \x20   return @area@(2) + len(@shout@(s)) + app.@perimeter@(3)\n",
        ),
    ],
    removed: &[],
    moved: &[],
    same_method: false,
};

const GO_ALIAS: Rewrite = Rewrite {
    project: &GO,
    files: &[(
        "app/app.go",
        "package app\n\
         \n\
         import (\n\
         \tshapes \"example.com/shapes/geo\"\n\
         \twords \"example.com/shapes/text\"\n\
         )\n\
         \n\
         func @Run@(g *words.Greeter) int {\n\
         \ts := g.@Greet@()\n\
         \treturn shapes.@Area@(2) + len(words.@Shout@(s)) + shapes.@Perimeter@(3)\n\
         }\n",
    )],
    removed: &[],
    moved: &[],
    same_method: true,
};

const GO_SPLIT: Rewrite = Rewrite {
    project: &GO,
    files: &[
        (
            "app/app.go",
            "package app\n\
             \n\
             import (\n\
             \t\"example.com/shapes/geo\"\n\
             \t\"example.com/shapes/loud\"\n\
             \t\"example.com/shapes/measure\"\n\
             \t\"example.com/shapes/text\"\n\
             )\n\
             \n\
             func @Run@(g *text.Greeter) int {\n\
             \ts := g.@Greet@()\n\
             \treturn geo.@Area@(2) + len(loud.@Shout@(s)) + measure.@Perimeter@(3)\n\
             }\n",
        ),
        (
            "geo/geo.go",
            "package geo\n\
             \n\
             func @Area@(x int) int { return @square@(x) }\n\
             \n\
             func @square@(x int) int { return x * x }\n",
        ),
        (
            "measure/measure.go",
            "package measure\n\
             \n\
             import \"example.com/shapes/text\"\n\
             \n\
             func @Perimeter@(x int) int { return text.@Count@(x) * 4 }\n",
        ),
        (
            "loud/loud.go",
            "package loud\n\
             \n\
             import \"strings\"\n\
             \n\
             func @Shout@(s string) string { return strings.ToUpper(s) }\n",
        ),
        (
            "text/text.go",
            "package text\n\
             \n\
             import \"example.com/shapes/loud\"\n\
             \n\
             type Greeter struct{}\n\
             \n\
             func (g *Greeter) @Greet@() string { return g.@salute@() }\n\
             \n\
             func (g *Greeter) @salute@() string { return loud.@Shout@(\"hi\") }\n\
             \n\
             func @Count@(x int) int { return x }\n",
        ),
    ],
    removed: &[],
    moved: &[
        ("measure::Perimeter", "geo::Perimeter"),
        ("loud::Shout", "text::Shout"),
    ],
    same_method: false,
};

/// A difference between a project's graph and its rewrite's that the
/// resolver does not close yet (#579). `missing` are edges of the
/// original graph the rewrite lost, `extra` are edges only the rewrite
/// has, both in [`NormEdge`]'s display form.
struct KnownGap {
    rewrite: &'static str,
    missing: &'static [&'static str],
    extra: &'static [&'static str],
}

/// Differences pinned rather than fixed (#579). Deleting a row is the
/// expected way to record a fix: the test compares the diff exactly, so
/// a closed gap fails until its row goes.
///
/// * **Barrels (Rust, TypeScript, Python).** Nothing follows a
///   re-export: an import through `prelude` / `index.ts` /
///   `__init__.py` names a path no node carries, so the call drops to
///   the bare-name fallback — ambiguous against the decoy, or
///   unresolved for a path call the suffix match cannot place. Closing
///   it needs a per-module re-export table from every adapter (a
///   re-exporting module has no functions, so its imports never reach
///   the graph today), which is more than a resolver fix.
/// * **Relative imports inside a directory index.** `src/geo/index.ts`
///   has the module path `src::geo`, so the adapter resolves its
///   `'../text'` one directory too high (`text`, not `src::text`) and
///   the call falls back to the bare name. The adapter only sees the
///   module path, not whether the file is an index; the same holds
///   for relative imports in a Python package's `__init__.py`.
const KNOWN_GAPS: &[KnownGap] = &[
    KnownGap {
        rewrite: "rust_barrel",
        missing: &[
            "crate::run -> crate::geo::area [Resolved] x1",
            "crate::run -> crate::geo::perimeter [Resolved] x1",
            "crate::run -> crate::text::shout [Resolved] x1",
        ],
        extra: &[
            "crate::run -> {crate::decoy::area, crate::geo::area} [Ambiguous] x1",
            "crate::run -> {crate::decoy::shout, crate::text::shout} [Ambiguous] x1",
            "crate::run -> ?perimeter [Unresolved] x1",
        ],
    },
    KnownGap {
        rewrite: "typescript_barrel",
        missing: &[
            "src::main::run -> src::geo::area [Resolved] x1",
            "src::main::run -> src::geo::perimeter [Resolved] x1",
            "src::main::run -> src::text::shout [Resolved] x1",
        ],
        extra: &[
            "src::main::run -> {src::decoy::area, src::geo::area} [Ambiguous] x1",
            "src::main::run -> {src::decoy::shout, src::text::shout} [Ambiguous] x1",
            "src::main::run -> ?perimeter [Unresolved] x1",
        ],
    },
    KnownGap {
        rewrite: "python_barrel",
        missing: &[
            "app::main::run -> app::geo::area [Resolved] x1",
            "app::main::run -> app::geo::perimeter [Resolved] x1",
            "app::main::run -> app::text::shout [Resolved] x1",
        ],
        extra: &[
            "app::main::run -> {app::decoy::area, app::geo::area} [Ambiguous] x1",
            "app::main::run -> {app::decoy::shout, app::text::shout} [Ambiguous] x1",
            "app::main::run -> ?perimeter [Unresolved] x1",
        ],
    },
];

/// Render every fixture file with the placeholders replaced by `names`
/// (original name → new name); unmapped placeholders render as written.
fn render(files: &[(String, String)], names: &HashMap<String, String>) -> Vec<(String, String)> {
    files
        .iter()
        .map(|(path, source)| {
            let mut out = String::with_capacity(source.len());
            let mut rest = source.as_str();
            while let Some(start) = rest.find('@') {
                out.push_str(&rest[..start]);
                let after = &rest[start + 1..];
                let end = after.find('@').expect("unterminated placeholder");
                let name = &after[..end];
                out.push_str(names.get(name).map_or(name, String::as_str));
                rest = &after[end + 1..];
            }
            out.push_str(rest);
            (path.clone(), out)
        })
        .collect()
}

fn project_files(project: &Project) -> Vec<(String, String)> {
    project
        .files
        .iter()
        .map(|(path, source)| ((*path).to_owned(), (*source).to_owned()))
        .collect()
}

fn rewritten_files(rewrite: &Rewrite) -> Vec<(String, String)> {
    let mut files: BTreeMap<String, String> = project_files(rewrite.project).into_iter().collect();
    for path in rewrite.removed {
        files.remove(*path);
    }
    for (path, source) in rewrite.files {
        files.insert((*path).to_owned(), (*source).to_owned());
    }
    files.into_iter().collect()
}

fn build(files: &[(String, String)]) -> CallGraph {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in files {
        write_file(dir.path(), path, source);
    }
    let graph = CallGraphBuilder::new()
        .build(&AnalyzeRoots::from(dir.path()))
        .unwrap();
    CallGraph::clone(&graph)
}

/// Where an edge points, with node names already mapped back to the
/// original project's.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    Node(String),
    Candidates(Vec<String>),
    Name(String),
    Anonymous,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct NormEdge {
    from: String,
    target: Target,
    resolution: Resolution,
    method: Option<ResolutionMethod>,
    call_count: usize,
}

impl fmt::Display for NormEdge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let target = match &self.target {
            Target::Node(name) => name.clone(),
            Target::Candidates(names) => format!("{{{}}}", names.join(", ")),
            Target::Name(name) => format!("?{name}"),
            Target::Anonymous => "<anonymous>".to_owned(),
        };
        let method = self
            .method
            .map_or_else(String::new, |method| format!("/{method:?}"));
        write!(
            f,
            "{} -> {target} [{:?}{method}] x{}",
            self.from, self.resolution, self.call_count
        )
    }
}

/// Maps a rewritten graph's names back to the original project's.
struct Canon<'a> {
    /// New function name → original name (rename rewrite).
    names: HashMap<String, String>,
    /// New qualified name → original qualified name (moves).
    moved: &'a [(&'a str, &'a str)],
    keep_method: bool,
}

impl Canon<'_> {
    fn identity(keep_method: bool) -> Self {
        Self {
            names: HashMap::new(),
            moved: &[],
            keep_method,
        }
    }

    fn name(&self, name: &str) -> String {
        self.names
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_owned())
    }

    fn qualified(&self, qualified: &str) -> String {
        let (prefix, last) = qualified.rsplit_once("::").unwrap_or(("", qualified));
        let renamed = if prefix.is_empty() {
            self.name(last)
        } else {
            format!("{prefix}::{}", self.name(last))
        };
        self.moved
            .iter()
            .find(|(new, _)| *new == renamed)
            .map_or(renamed, |(_, old)| (*old).to_owned())
    }

    fn edges(&self, graph: &CallGraph) -> BTreeSet<NormEdge> {
        let qualified: HashMap<&str, String> = graph
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), self.qualified(&node.qualified_name)))
            .collect();
        let label = |id: &str| qualified.get(id).cloned().unwrap_or_else(|| id.to_owned());
        graph
            .edges
            .iter()
            .map(|edge| {
                let target = match (edge.resolution, edge.to.as_deref()) {
                    (Resolution::Resolved, Some(to)) => Target::Node(label(to)),
                    (Resolution::Ambiguous, _) => {
                        let mut names: Vec<String> =
                            edge.candidates.iter().map(|id| label(id)).collect();
                        names.sort();
                        Target::Candidates(names)
                    }
                    _ => edge
                        .callee_name
                        .as_deref()
                        .map_or(Target::Anonymous, |name| Target::Name(self.name(name))),
                };
                NormEdge {
                    from: edge
                        .from
                        .as_deref()
                        .map_or_else(|| "<top>".to_owned(), label),
                    target,
                    resolution: edge.resolution,
                    method: edge.resolution_method.filter(|_| self.keep_method),
                    call_count: edge.call_count,
                }
            })
            .collect()
    }
}

/// `(missing, extra)`: edges only `before` has, edges only `after` has.
fn diff(before: &BTreeSet<NormEdge>, after: &BTreeSet<NormEdge>) -> (Vec<String>, Vec<String>) {
    let render = |edges: std::collections::btree_set::Difference<'_, NormEdge>| {
        edges.map(ToString::to_string).collect::<Vec<_>>()
    };
    (
        render(before.difference(after)),
        render(after.difference(before)),
    )
}

/// The fixtures exercise every strategy the resolver has, so a rewrite
/// that preserves them is saying something about each — and a fixture
/// that silently stopped resolving would make every rewrite vacuous.
#[rstest]
#[case::rust(&RUST)]
#[case::typescript(&TYPESCRIPT)]
#[case::python(&PYTHON)]
#[case::go(&GO)]
fn fixture_projects_resolve_their_workspace_calls(#[case] project: &Project) {
    let graph = build(&render(&project_files(project), &HashMap::new()));
    let edges = Canon::identity(true).edges(&graph);
    let resolved = edges
        .iter()
        .filter(|edge| edge.resolution == Resolution::Resolved)
        .count();
    let unresolved_workspace: Vec<String> = edges
        .iter()
        .filter(|edge| {
            matches!(&edge.target, Target::Name(name)
                if ["area", "Area", "shout", "Shout", "perimeter", "Perimeter"].contains(&name.as_str()))
                || matches!(&edge.target, Target::Candidates(names)
                    if !names.iter().any(|n| n.ends_with("greet") || n.ends_with("Greet")))
        })
        .map(ToString::to_string)
        .collect();
    assert!(
        unresolved_workspace.is_empty(),
        "{:?}: calls into the workspace that did not resolve: {unresolved_workspace:#?}",
        project.language,
    );
    assert!(
        resolved >= 7,
        "{:?}: expected the fixture to resolve its workspace calls, got {edges:#?}",
        project.language,
    );
}

#[rstest]
#[case::rust_alias("rust_alias", &RUST_ALIAS)]
#[case::rust_relative_alias("rust_relative_alias", &RUST_RELATIVE_ALIAS)]
#[case::rust_split("rust_split", &RUST_SPLIT)]
#[case::rust_barrel("rust_barrel", &RUST_BARREL)]
#[case::typescript_alias("typescript_alias", &TYPESCRIPT_ALIAS)]
#[case::typescript_split("typescript_split", &TYPESCRIPT_SPLIT)]
#[case::typescript_index_module("typescript_index_module", &TYPESCRIPT_INDEX_MODULE)]
#[case::typescript_barrel("typescript_barrel", &TYPESCRIPT_BARREL)]
#[case::python_alias("python_alias", &PYTHON_ALIAS)]
#[case::python_relative_alias("python_relative_alias", &PYTHON_RELATIVE_ALIAS)]
#[case::python_split("python_split", &PYTHON_SPLIT)]
#[case::python_barrel("python_barrel", &PYTHON_BARREL)]
#[case::go_alias("go_alias", &GO_ALIAS)]
#[case::go_split("go_split", &GO_SPLIT)]
fn semantics_preserving_rewrites_keep_the_call_graph(
    #[case] name: &str,
    #[case] rewrite: &Rewrite,
) {
    let no_rename = HashMap::new();
    let before = Canon::identity(rewrite.same_method)
        .edges(&build(&render(&project_files(rewrite.project), &no_rename)));
    let canon = Canon {
        names: HashMap::new(),
        moved: rewrite.moved,
        keep_method: rewrite.same_method,
    };
    let after = canon.edges(&build(&render(&rewritten_files(rewrite), &no_rename)));

    let (missing, extra) = diff(&before, &after);
    let (expected_missing, expected_extra) = KNOWN_GAPS
        .iter()
        .find(|gap| gap.rewrite == name)
        .map_or((Vec::new(), Vec::new()), |gap| {
            (
                gap.missing.iter().map(|s| (*s).to_owned()).collect(),
                gap.extra.iter().map(|s| (*s).to_owned()).collect(),
            )
        });
    assert_eq!(
        (missing, extra),
        (expected_missing, expected_extra),
        "{name}: (missing, extra) edges differ from KNOWN_GAPS — a rewrite that preserves \
         semantics must preserve the graph; if a gap closed, delete its KNOWN_GAPS row",
    );
}

/// Words no generated name may take: keywords and predeclared names of
/// the four languages, plus every other identifier the fixtures spell.
const RESERVED: &[&str] = &[
    // Rust
    "abstract",
    "async",
    "await",
    "become",
    "box",
    "break",
    "const",
    "continue",
    "crate",
    "dyn",
    "else",
    "enum",
    "extern",
    "false",
    "final",
    "for",
    "gen",
    "impl",
    "let",
    "loop",
    "macro",
    "match",
    "mod",
    "move",
    "mut",
    "override",
    "priv",
    "pub",
    "ref",
    "return",
    "self",
    "static",
    "struct",
    "super",
    "trait",
    "true",
    "try",
    "type",
    "typeof",
    "union",
    "unsafe",
    "unsized",
    "use",
    "virtual",
    "where",
    "while",
    "yield",
    // TypeScript
    "any",
    "boolean",
    "case",
    "catch",
    "class",
    "constructor",
    "debugger",
    "declare",
    "default",
    "delete",
    "do",
    "export",
    "extends",
    "finally",
    "from",
    "function",
    "get",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "module",
    "namespace",
    "never",
    "new",
    "null",
    "number",
    "object",
    "of",
    "package",
    "private",
    "protected",
    "public",
    "require",
    "set",
    "string",
    "switch",
    "symbol",
    "this",
    "throw",
    "undefined",
    "unknown",
    "var",
    "void",
    "with",
    // Python
    "and",
    "assert",
    "def",
    "del",
    "elif",
    "except",
    "global",
    "is",
    "lambda",
    "none",
    "nonlocal",
    "not",
    "or",
    "pass",
    "print",
    "raise",
    // Go
    "append",
    "bool",
    "byte",
    "cap",
    "chan",
    "clear",
    "close",
    "complex",
    "copy",
    "defer",
    "error",
    "fallthrough",
    "func",
    "goto",
    "imag",
    "int",
    "iota",
    "len",
    "make",
    "map",
    "max",
    "min",
    "nil",
    "panic",
    "println",
    "range",
    "real",
    "recover",
    "rune",
    "select",
    "uint",
    // Fixture identifiers
    "app",
    "decoy",
    "geo",
    "greeter",
    "hello",
    "index",
    "lib",
    "loud",
    "main",
    "measure",
    "prelude",
    "shapes",
    "strings",
    "surface",
    "text",
    "words",
    "yell",
];

/// Function placeholders of `project`, in a stable order.
fn placeholders(project: &Project) -> Vec<String> {
    let names: BTreeSet<String> = project
        .files
        .iter()
        .flat_map(|(_, source)| source.split('@').skip(1).step_by(2).map(ToOwned::to_owned))
        .collect();
    names.into_iter().collect()
}

/// Give `name` the case of `original`'s first letter, so a Go function
/// keeps its export status and nothing else about it changes.
fn with_case_of(original: &str, name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if original.starts_with(|c: char| c.is_ascii_uppercase()) => {
            first.to_ascii_uppercase().to_string() + chars.as_str()
        }
        _ => name.to_owned(),
    }
}

fn usable_name(language: GraphLanguage, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let capital = with_case_of("X", name);
    !RESERVED.contains(&lower.as_str())
        && [lower.as_str(), capital.as_str()].iter().all(|candidate| {
            !language.ubiquitous_method_names().contains(candidate)
                && !language.builtin_function_names().contains(candidate)
        })
}

/// Distinct identifiers, one per placeholder.
fn fresh_names(language: GraphLanguage, count: usize) -> impl Strategy<Value = Vec<String>> {
    proptest::collection::btree_set(
        "[a-z][a-z0-9_]{1,9}".prop_filter("reserved or table-listed name", move |name| {
            usable_name(language, name)
        }),
        count,
    )
    .prop_map(|names| names.into_iter().collect())
    .prop_shuffle()
}

/// Renaming every function changes nothing but spelling, so the graph —
/// method included — must map back onto the original edge for edge.
#[rstest]
#[case::rust(&RUST)]
#[case::typescript(&TYPESCRIPT)]
#[case::python(&PYTHON)]
#[case::go(&GO)]
fn renaming_functions_keeps_the_call_graph(#[case] project: &Project) {
    let files = project_files(project);
    let before = Canon::identity(true).edges(&build(&render(&files, &HashMap::new())));
    let keys = placeholders(project);
    let config = Config {
        cases: 12,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(579),
        ..Config::default()
    };
    let mut runner = TestRunner::new(config);
    let outcome = runner.run(&fresh_names(project.language, keys.len()), |fresh| {
        let renames: HashMap<String, String> = keys
            .iter()
            .zip(&fresh)
            .map(|(key, name)| (key.clone(), with_case_of(key, name)))
            .collect();
        let canon = Canon {
            names: renames
                .iter()
                .map(|(old, new)| (new.clone(), old.clone()))
                .collect(),
            moved: &[],
            keep_method: true,
        };
        let after = canon.edges(&build(&render(&files, &renames)));
        let (missing, extra) = diff(&before, &after);
        prop_assert!(
            missing.is_empty() && extra.is_empty(),
            "{:?} renamed {renames:?}: missing {missing:#?}, extra {extra:#?}",
            project.language,
        );
        Ok(())
    });
    if let Err(err) = outcome {
        panic!("{err}");
    }
}
