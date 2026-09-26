//! Corpus writers and runners shared by the Criterion benchmarks.
//!
//! Each bench target compiles this module on its own and uses a subset of
//! it, so the unused remainder is expected per target.
#![allow(dead_code, reason = "each bench target uses a different subset")]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Materialize a corpus in a fresh tempdir, panicking on I/O failure:
/// a benchmark without its corpus has nothing to measure.
pub fn corpus(write: impl FnOnce(&Path) -> std::io::Result<()>) -> TempDir {
    let dir = tempfile::tempdir().unwrap_or_else(|err| {
        panic!("failed to create benchmark tempdir: {err}");
    });
    write(dir.path()).unwrap_or_else(|err| {
        panic!("failed to write benchmark corpus: {err}");
    });
    dir
}

/// Unwrap an analyzer report and keep its length alive, so a failing
/// analyzer aborts the run instead of timing its error path.
pub fn consume<E: std::fmt::Display>(report: Result<String, E>) {
    match report {
        Ok(report) => {
            std::hint::black_box(report.len());
        }
        Err(err) => panic!("benchmark analyzer failed: {err}"),
    }
}

/// A Rust crate of `file_count` modules with `functions_per_file`
/// functions each, calling across modules through every resolver path
/// that matters at scale: lexical hits, `crate::`-prefixed paths,
/// receiver-method fallbacks, duplicate names (ambiguity), and external
/// calls (unresolved).
pub fn write_rust_graph_corpus(
    root: &Path,
    file_count: usize,
    functions_per_file: usize,
) -> std::io::Result<()> {
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"graph-bench\"\n",
    )?;
    let mut lib = String::new();
    for file_idx in 0..file_count {
        let _ = writeln!(lib, "pub mod module_{file_idx:02};");
    }
    std::fs::write(root.join("src/lib.rs"), lib)?;

    for file_idx in 0..file_count {
        let path: PathBuf = root.join(format!("src/module_{file_idx:02}.rs"));
        let mut src = String::new();
        let neighbor = (file_idx + 1) % file_count;
        let _ = writeln!(src, "use crate::module_{neighbor:02};");
        let _ = writeln!(src, "pub struct Widget_{file_idx:02};");
        let _ = writeln!(
            src,
            "impl Widget_{file_idx:02} {{ pub fn refresh(&self) {{}} }}"
        );
        for fn_idx in 0..functions_per_file {
            let next = (fn_idx + 1) % functions_per_file;
            let _ = writeln!(
                src,
                r#"
pub fn generated_{file_idx:02}_{fn_idx:03}(widget: &Widget_{file_idx:02}) -> i64 {{
    // Lexical same-module call plus a cross-module path call.
    let local = generated_{file_idx:02}_{next:03};
    std::hint::black_box(&local);
    crate::module_{neighbor:02}::generated_{neighbor:02}_{fn_idx:03}(widget_of_{neighbor:02}());
    module_{neighbor:02}::shared_helper();
    // Receiver method (last-segment fallback) and an ambiguous name.
    widget.refresh();
    shared_helper();
    // External call stays unresolved.
    external_dependency_call({fn_idx});
    {fn_idx}
}}
"#,
            );
        }
        let _ = writeln!(src, "pub fn shared_helper() {{}}");
        let _ = writeln!(
            src,
            "pub fn widget_of_{neighbor:02}() -> &'static Widget_{neighbor:02} {{ unimplemented!() }}"
        );
        std::fs::write(&path, src)?;
    }
    Ok(())
}

/// The graph corpus plus one `impl` per module whose methods touch
/// disjoint field subsets, so cohesion has LCOM components to split and
/// the wrapper and delegation analyzers have forwarders to find.
pub fn write_rust_crate_corpus(
    root: &Path,
    file_count: usize,
    functions_per_file: usize,
) -> std::io::Result<()> {
    write_rust_graph_corpus(root, file_count, functions_per_file)?;
    let mut lib = std::fs::read_to_string(root.join("src/lib.rs"))?;
    for file_idx in 0..file_count {
        let _ = writeln!(lib, "pub mod model_{file_idx:02};");
        let neighbor = (file_idx + 1) % file_count;
        let src = format!(
            r#"
pub struct Model{file_idx:02} {{
    count: u64,
    total: i64,
    name: String,
    tags: Vec<String>,
}}

impl Model{file_idx:02} {{
    pub fn bump(&mut self, by: u64) -> u64 {{
        self.count += by;
        if self.count > 100 {{ self.count = 0; }}
        self.count
    }}
    pub fn add(&mut self, value: i64) -> i64 {{
        for step in 0..value.rem_euclid(7) {{
            if step % 2 == 0 {{ self.total += step; }} else {{ self.total -= 1; }}
        }}
        self.total
    }}
    pub fn rename(&mut self, name: &str) {{
        self.name = name.to_owned();
        self.tags.push(name.to_owned());
    }}
    pub fn label(&self) -> String {{
        self.describe()
    }}
    fn describe(&self) -> String {{
        format!("{{}}:{{}}", self.name, self.tags.len())
    }}
    pub fn forward(&mut self, by: u64) -> u64 {{
        self.bump(by)
    }}
    pub fn relay(&self, value: i64) -> i64 {{
        crate::model_{neighbor:02}::scale(value)
    }}
}}

pub fn scale(value: i64) -> i64 {{
    match value.rem_euclid(4) {{
        0 => value * 2,
        1 if value > 10 => value / 2,
        2 => value - 1,
        _ => value + 1,
    }}
}}
"#
        );
        std::fs::write(root.join(format!("src/model_{file_idx:02}.rs")), src)?;
    }
    std::fs::write(root.join("src/lib.rs"), lib)
}

/// The corpus of issue #562: Go functions that share one error-handling
/// skeleton (call / err-check / call / err-check / loop / return) with
/// every identifier and literal unique, so there is no copy-paste at all.
/// Value-blind candidate generation makes `--target blocks` score every
/// pair of idioms here, which is the quadratic path that regressed.
pub fn write_go_idiom_corpus(root: &Path, file_count: usize) -> std::io::Result<()> {
    for f in 0..file_count {
        let mut src = format!("package p{f}\n\nimport \"errors\"\n");
        for k in 0..8 {
            let u = format!("{f}_{k}");
            let _ = write!(
                src,
                r#"
func Handle{u}(in{u} string) (string, error) {{
	a{u}, err := load{u}(in{u}, "key-{u}-a")
	if err != nil {{
		return "", errors.New("load {u} failed")
	}}
	b{u}, err := check{u}(a{u}, {lit})
	if err != nil {{
		return "", errors.New("check {u} failed")
	}}
	for i{u} := 0; i{u} < {bound}; i{u}++ {{
		b{u} = merge{u}(b{u}, i{u}, "sep-{u}")
	}}
	return render{u}(b{u}, "fmt-{u}"), nil
}}
"#,
                lit = f * 100 + k,
                bound = k + 3,
            );
        }
        std::fs::write(root.join(format!("f{f}.go")), src)?;
    }
    Ok(())
}

/// A Go module of `package_count` packages, each importing its neighbor,
/// with a method set on one struct per package so graph, cohesion and
/// coupling analyzers all have Go edges to resolve.
pub fn write_go_module_corpus(
    root: &Path,
    package_count: usize,
    functions_per_package: usize,
) -> std::io::Result<()> {
    std::fs::write(root.join("go.mod"), "module example.com/bench\n\ngo 1.22\n")?;
    for p in 0..package_count {
        let neighbor = (p + 1) % package_count;
        let dir = root.join(format!("pkg{p:02}"));
        std::fs::create_dir_all(&dir)?;
        let mut src = format!(
            "package pkg{p:02}\n\nimport (\n\t\"errors\"\n\t\"example.com/bench/pkg{neighbor:02}\"\n)\n\n\
             type Store struct {{\n\tcount int\n\tname  string\n\titems []string\n}}\n\n\
             func (s *Store) Bump(by int) int {{\n\ts.count += by\n\tif s.count > 100 {{\n\t\ts.count = 0\n\t}}\n\treturn s.count\n}}\n\n\
             func (s *Store) Rename(name string) {{\n\ts.name = name\n\ts.items = append(s.items, name)\n}}\n\n\
             func (s *Store) Forward(by int) int {{\n\treturn s.Bump(by)\n}}\n"
        );
        for k in 0..functions_per_package {
            let next = (k + 1) % functions_per_package;
            let _ = write!(
                src,
                r#"
func Step{k:03}(in int) (int, error) {{
	if in < 0 {{
		return 0, errors.New("negative {p}_{k}")
	}}
	acc := in + {salt}
	for i := 0; i < {bound}; i++ {{
		switch {{
		case acc%{modulus} == 0:
			acc += i
		case acc > {limit}:
			acc /= 2
		default:
			acc -= 1
		}}
	}}
	_ = Step{next:03}
	v, err := pkg{neighbor:02}.Step{k:03}(acc)
	if err != nil {{
		return 0, err
	}}
	return v + acc, nil
}}
"#,
                salt = (p * 7 + k) % 13,
                bound = 3 + k % 5,
                modulus = 2 + k % 4,
                limit = 40 + k,
            );
        }
        std::fs::write(dir.join(format!("pkg{p:02}.go")), src)?;
    }
    Ok(())
}

/// A TypeScript tree of `file_count` modules importing their neighbor,
/// each with a class, free functions, arrow functions and branching.
pub fn write_ts_corpus(
    root: &Path,
    file_count: usize,
    functions_per_file: usize,
) -> std::io::Result<()> {
    std::fs::write(
        root.join("package.json"),
        "{\"name\":\"ts-bench\",\"private\":true}\n",
    )?;
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir)?;
    for f in 0..file_count {
        let neighbor = (f + 1) % file_count;
        let mut src = format!(
            "import {{ step000 as neighborStep }} from \"./mod{neighbor:02}\";\n\n\
             export class Store{f:02} {{\n  private count = 0;\n  private name = \"\";\n  private items: string[] = [];\n\n  \
             bump(by: number): number {{\n    this.count += by;\n    if (this.count > 100) {{\n      this.count = 0;\n    }}\n    return this.count;\n  }}\n\n  \
             rename(name: string): void {{\n    this.name = name;\n    this.items.push(name);\n  }}\n\n  \
             forward(by: number): number {{\n    return this.bump(by);\n  }}\n}}\n"
        );
        for k in 0..functions_per_file {
            let next = (k + 1) % functions_per_file;
            let _ = write!(
                src,
                r#"
export function step{k:03}(input: number): number {{
  let acc = input + {salt};
  for (let i = 0; i < {bound}; i++) {{
    if (acc % {modulus} === 0) {{
      acc += i * {factor};
    }} else if (acc > {limit}) {{
      acc = Math.floor(acc / 2);
    }} else {{
      acc -= 1;
    }}
  }}
  const pick = [acc, input].filter((v) => v > {salt}).map((v) => v * 2);
  void step{next:03};
  return pick.length > 0 ? neighborStep(pick[0] ?? 0) : acc;
}}
"#,
                salt = (f * 7 + k) % 13,
                bound = 3 + k % 5,
                modulus = 2 + k % 4,
                factor = 1 + k % 3,
                limit = 40 + k,
            );
        }
        std::fs::write(src_dir.join(format!("mod{f:02}.ts")), src)?;
    }
    Ok(())
}

/// A Python package of `file_count` modules importing their neighbor,
/// each with a class and branching free functions.
pub fn write_py_corpus(
    root: &Path,
    file_count: usize,
    functions_per_file: usize,
) -> std::io::Result<()> {
    let pkg = root.join("benchpkg");
    std::fs::create_dir_all(&pkg)?;
    std::fs::write(pkg.join("__init__.py"), "")?;
    for f in 0..file_count {
        let neighbor = (f + 1) % file_count;
        let mut src = format!(
            "from benchpkg import mod{neighbor:02}\n\n\n\
             class Store{f:02}:\n    def __init__(self):\n        self.count = 0\n        self.name = \"\"\n        self.items = []\n\n    \
             def bump(self, by):\n        self.count += by\n        if self.count > 100:\n            self.count = 0\n        return self.count\n\n    \
             def rename(self, name):\n        self.name = name\n        self.items.append(name)\n\n    \
             def forward(self, by):\n        return self.bump(by)\n"
        );
        for k in 0..functions_per_file {
            let next = (k + 1) % functions_per_file;
            let _ = write!(
                src,
                r#"

def step{k:03}(value):
    acc = value + {salt}
    for i in range({bound}):
        if acc % {modulus} == 0:
            acc += i * {factor}
        elif acc > {limit}:
            acc //= 2
        else:
            acc -= 1
    picked = [v * 2 for v in (acc, value) if v > {salt}]
    _ = step{next:03}
    return mod{neighbor:02}.step000(picked[0]) if picked else acc
"#,
                salt = (f * 7 + k) % 13,
                bound = 3 + k % 5,
                modulus = 2 + k % 4,
                factor = 1 + k % 3,
                limit = 40 + k,
            );
        }
        std::fs::write(pkg.join(format!("mod{f:02}.py")), src)?;
    }
    Ok(())
}

/// Turn `root` into a git repository with `commits` commits, each editing
/// a rotating subset of the `*.rs` files under `src/`, so churn and
/// co-change have a history to walk.
pub fn write_git_history(root: &Path, commits: usize) -> std::io::Result<()> {
    git(root, &["init", "-q", "-b", "main"])?;
    git(root, &["add", "-A"])?;
    git(root, &["commit", "-q", "-m", "initial"])?;
    let files = rust_sources(root)?;
    for commit in 0..commits {
        for (idx, file) in files.iter().enumerate() {
            if (idx + commit) % 4 == 0 {
                append(file, &format!("// churn {commit}\n"))?;
            }
        }
        git(root, &["commit", "-q", "-am", &format!("change {commit}")])?;
    }
    Ok(())
}

/// Leave an uncommitted edit on every fourth `*.rs` file under `src/`:
/// a new function per file, the shape a pre-commit `--diff-only` run sees.
pub fn write_pending_edit(root: &Path) -> std::io::Result<()> {
    for (idx, file) in rust_sources(root)?.iter().enumerate().step_by(4) {
        append(
            file,
            &format!(
                "\npub fn pending_{idx}(input: i64) -> i64 {{\n    if input > {idx} {{ input - 1 }} else {{ input + 1 }}\n}}\n"
            ),
        )?;
    }
    Ok(())
}

fn rust_sources(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(root.join("src"))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    files.sort();
    Ok(files)
}

fn append(path: &Path, text: &str) -> std::io::Result<()> {
    let mut content = std::fs::read_to_string(path)?;
    content.push_str(text);
    std::fs::write(path, content)
}

fn git(root: &Path, args: &[&str]) -> std::io::Result<()> {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=bench",
            "-c",
            "user.email=bench@example.com",
        ])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(root)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "git {args:?} failed: {status}"
        )))
    }
}
