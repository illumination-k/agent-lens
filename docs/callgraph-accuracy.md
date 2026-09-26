# Call-graph accuracy

Every call-graph analyzer (`hubs`, `impact`, `unreachable`, `untested`, `risk`,
`layers`, ...) sits on the syntactic resolver in
`crates/agent-lens/src/analyze/call_graph/`. This benchmark measures that
resolver: precision and recall of `agent-lens analyze function-graph` per
`ResolutionMethod`, against mechanical oracles, with humans adjudicating only
the disagreements (issue #579).

```sh
mise run callgraph-accuracy            # every target in targets.toml
mise run callgraph-accuracy pflag      # one or more targets
```

The task builds the release binary and runs
`scripts/callgraph-accuracy/run.py`. It needs network access (git, Go module
proxy, PyPI); mise supplies `go` and `uv` through the task's `tools`. Output
goes to `target/callgraph-accuracy/` and is never committed:

| Path                                         | Content                                                    |
| -------------------------------------------- | ---------------------------------------------------------- |
| `summary.md`, `summary.json`                 | combined table per language and oracle                     |
| `results/<target>/report.md`, `score.json`   | per-target tables and raw counts                           |
| `results/<target>/disagreements.json`        | every unadjudicated disagreement, each with a TOML snippet |
| `results/<target>/graph.json`, `oracle.json` | the two inputs of the scorer                               |
| `repos/<target>`, `venvs/<target>`           | checkout at the pinned commit, Python venv                 |

It is outside `ci` like `bench`: it clones third-party code and runs its test
suite under a profiler, and the result is a report, not a gate. What does run
in `ci`: `ruff check` and the unit tests of the scorer and the Python oracle
(`test:callgraph-accuracy`, under `ci:rust`; `ci_rust.yml` also triggers on
`scripts/callgraph-accuracy/**`) and the metamorphic resolver
tests (cargo tests). The Go oracle's own tests (`go test ./...` in
`scripts/callgraph-accuracy/oracle-go`) are not wired into `ci`.

## Edge definition

An edge is **function → function**: from the innermost agent-lens node
containing the call site to the node of the function that runs. Both oracles
are normalised to this before comparison.

| Case                                                                                      | Rule                                                                                                                                         |
| ----------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| builtins, stdlib, third-party, vendored, generated code                                   | excluded; only functions defined in files under the analysed root are endpoints                                                              |
| constructor                                                                               | Python `A()` → `A.__init__`. Go and Rust have no constructors: `NewX()` / `X::new()` is a plain function edge                                |
| static dispatch                                                                           | a direct call of a named function, or a method on a concrete type: must be a `resolved` edge                                                 |
| `dyn` / interface / virtual dispatch, call through a function value                       | a **candidate set**, scored apart from static dispatch; not expected to be resolved syntactically                                            |
| callback (Python function invoked from C, e.g. `sorted(key=f)`, `map(f, ...)`)            | its own recall row; agent-lens emits no edge for passing a function as a value                                                               |
| call inside a closure, lambda, comprehension or nested function                           | attributed to the innermost named function agent-lens has a node for (see below)                                                             |
| a closure / lambda / nested function as the callee                                        | no node, so unmapped (`callee_nested_or_anonymous`)                                                                                          |
| test code                                                                                 | included (test functions are ordinary nodes with `is_test`)                                                                                  |
| recursion                                                                                 | kept (self edges count)                                                                                                                      |
| decorators                                                                                | the decorated function is the callee; a decorator's wrapper is not an edge of its own (the Python oracle does not implement this: see below) |
| implicit calls (Python `__iter__` / `__next__` in a `for`, operators, `with`, properties) | edges, since the function runs, but there is no call expression; missed pairs of this kind show up under `of which no call site`             |

What agent-lens does today with nested code, measured on small samples (keep
this table in sync when the resolver changes):

| Construct                             | Rust                                                                          | Go                                                             | Python                                   |
| ------------------------------------- | ----------------------------------------------------------------------------- | -------------------------------------------------------------- | ---------------------------------------- |
| calls in a closure / lambda body      | attributed to the enclosing fn                                                | **dropped** (no edge from any node)                            | attributed to the enclosing function     |
| calls in a nested named fn / def      | **dropped**                                                                   | n/a                                                            | attributed to the enclosing function     |
| calling the closure / nested fn       | `unresolved`                                                                  | `unresolved` (named variable) or `anonymous` (`func(){...}()`) | `unresolved`                             |
| `A()` on a class                      | n/a                                                                           | n/a                                                            | `unresolved` (no `__init__` edge)        |
| method on an interface / trait object | `resolved` via `last_segment` if one impl has that name, else a candidate set | same                                                           | same                                     |
| decorated def                         | n/a                                                                           | n/a                                                            | node spans from the first decorator line |

So a Go oracle edge whose call site is inside a func literal is attributed to
the enclosing named function (by the oracle, which keeps the real call line)
and counts as missed by agent-lens, under `of which no call site`.

Where the oracles fall short of the definition:

- Go: calls made by package-level `var x = f()` initialisers have no named
  caller and are dropped; so are the synthetic package `init`. Files a build
  constraint excludes (`//go:build cmp_debug`, another GOOS) are never
  type-checked; the oracle lists the files it did check (`analyzed_files`)
  and the scorer leaves agent-lens edges from other files out of precision.
- Python: a decorator's wrapper is recorded as a function of its own. The
  wrapper is a nested def, so `caller → wrapper` is unmapped
  (`callee_nested_or_anonymous`) and `wrapper → decorated` maps to
  `decorator → decorated` (dispatch `dynamic`), while agent-lens resolves
  `caller → decorated`, which the definition makes the edge: record an
  agent-lens-only `caller → decorated` as `oracle_wrong`. A decorator that is
  a class (toolz `@curry`) shows up as `caller → curry.__call__` instead. A C
  wrapper (`functools.lru_cache`) is transparent.
- Python: a generator's call is seen only when it is first advanced; the
  edge is kept when that line names the generator (`for x in gen():`) and
  dropped otherwise (counted in the oracle's `stats`); an agent-lens edge
  to such a generator is `oracle_wrong`.

## Oracles

| Language | Oracle              | Kind         | Tool                                                                                                             |
| -------- | ------------------- | ------------ | ---------------------------------------------------------------------------------------------------------------- |
| Go       | `go-vta`            | type-checker | `golang.org/x/tools/go/callgraph/vta` over the packages and their tests (`scripts/callgraph-accuracy/oracle-go`) |
| Python   | `python-setprofile` | dynamic      | `sys.setprofile` while the project's pytest suite runs (`scripts/callgraph-accuracy/oracle_py.py`)               |

Static-dispatch edges of a type-checker oracle are near exact, so precision
and recall are both verdicts. A dynamic oracle only sees what the tests run:
an observed edge is certainly real, so **recall** is trustworthy, but an
agent-lens edge that was never observed may still be real, so precision is
replaced by an indicator (below).

### Oracle JSON contract

```json
{
  "oracle": "go-vta | python-setprofile",
  "language": "go | python",
  "kind": "type-checker | dynamic",
  "root": "<absolute path of the analysed tree>",
  "edges": [{
    "caller_file": "<posix path relative to root>",
    "caller_def_line": 12,
    "call_line": 15,
    "callee_file": "<relative>",
    "callee_def_line": 40,
    "dispatch": "static | dynamic | callback"
  }],
  "executed_functions": [{ "file": "<relative>", "def_line": 12 }],
  "analyzed_files": ["<relative>"]
}
```

- Lines are 1-based. A def line is the line of the `def` / `func` keyword
  (Python: after skipping decorator lines).
- Edges are deduplicated on all fields; only functions defined under `root`
  appear.
- `static`: callee fixed at compile time. `dynamic`: interface / virtual
  dispatch or a call through a function value. `callback`: a Python function
  invoked from C code, attributed to the nearest Python frame.
- `executed_functions` (dynamic oracles only): every function observed running.
- `analyzed_files` (optional, type-checker oracles): every file under `root`
  the oracle type-checked. When present, an agent-lens edge whose caller is in
  another file is outside the oracle's view and left out of precision.
- The Python oracle reads dispatch off the call line, since it sees no types:
  `static` when a call expression on that line names the callee (`f(...)`,
  `x.f(...)`), or, for a constructor dunder (`__init__`, `__new__`), the
  class, a subclass inheriting it, or the dunder itself (`A()`, `B()`,
  `super().__init__()`); a constructor reached through a value (`cls(x)`,
  `type(self)(x)`, a local alias) is `dynamic`, one handed to C
  (`map(A, xs)`) `callback`; `callback` when a C function call is open (`sorted(key=f)`) or the line
  names the callee without calling it (`list(map(f, xs))`); `dynamic` for an
  implicit protocol call (property, operator, `__iter__`, `len(x)` →
  `__len__`) and for a call through a value bound to another name (a
  parameter `func(x)`, a local, a `__call__`). Extra keys (`stats`) are
  ignored by the scorer.
- The Go oracle: `static` when the SSA call site has a static callee and the
  call expression names it (an identifier or selector resolving to a declared
  function, or to a method on a concrete receiver); otherwise `dynamic`,
  including a local function value SSA forwards (`f := a.M; f()`) and a method
  called on a type-parameter value in a generic body. Synthetic wrappers (method values, promoted methods,
  generic instances) are followed to the declared function they forward to.
  `call_line` is the line of the call's `(`.

### Mapping to agent-lens nodes

- Caller: the innermost node in `caller_file` whose `[start_line, end_line]`
  contains `call_line` (fallback: `caller_def_line`).
- Callee: the innermost node in `callee_file` containing `callee_def_line`,
  provided that line is the node's own definition (its first line, or reached
  from it through decorator lines only). Otherwise the callee is a nested or
  anonymous function.
- Two nodes of equal span containing the line (several one-line functions on
  one line) are disambiguated by an exact `start_line`, else unmapped.
- A pair reported under several dispatch kinds counts once, as the first of
  static, dynamic, callback.
- Unmappable oracle edges go to the **unmapped** bucket with a reason:
  `caller_no_enclosing_node` (e.g. module-level code), `callee_nested_or_anonymous`,
  `*_file_not_in_graph`, `*_ambiguous_node`.

## Metrics

`O` is the set of mapped oracle pairs (caller node, callee node); `R_m` the
agent-lens `resolved` pairs with resolution method `m` (`lexical`,
`self_method`, `last_segment`, `path_suffix`, `crate_narrowed`).

- **Precision** (type-checker oracle), per `m`: TP = |R_m ∩ O|,
  FP = |R_m \ O|, precision = TP / (TP + FP), over the pairs whose caller is
  in a file the oracle type-checked (the others are counted as excluded). The `overall` row is over the union
  R of all methods; a pair reached by two methods counts in both method rows
  and once overall.
- **Unobserved rate** (dynamic oracle), in place of precision: over the
  agent-lens pairs whose caller node is in `executed_functions`, the share
  never observed. Pairs whose caller never ran are excluded and counted. An
  indicator, **not** a precision verdict.
- **Recall**, per oracle dispatch kind (static, dynamic, callback): found =
  oracle pairs in R, split by the method that found them (a pair found by two
  methods counts in both columns); `only in candidate set` = among an
  ambiguous edge's candidates from that caller but not resolved; `missed` =
  neither. `of which no call site` counts the missed pairs where agent-lens
  recorded no call site at all on the oracle's call line in that caller
  (implicit calls such as `__iter__`, dropped Go closure bodies); the rest of
  `missed` are call sites agent-lens saw but left unresolved.
- **Candidate sets**, per method over `ambiguous` edges whose caller is in the
  oracle's view (same scope as precision; the others are counted as
  excluded): number of sets, mean set size, hit rate (sets with any candidate
  in O from that caller), candidate precision (Σ |cands ∩ O| / Σ |cands|).
- **Unknown / adjudicated**: unmapped oracle edges by reason, adjudications
  applied by verdict, stale adjudications, unadjudicated disagreements.

`run.py` sums the raw counts of every target per (language, oracle) and
recomputes the ratios, so the combined row is micro-averaged.

## Adjudication

Humans look only at disagreements: agent-lens-only pairs (for a dynamic
oracle, only those whose caller ran) and oracle-only pairs. Each is listed in
`results/<target>/disagreements.json` with qualified names, locations,
method or dispatch, and a pre-filled snippet. The verdict goes into
`scripts/callgraph-accuracy/adjudications.toml`, keyed by target and
agent-lens qualified names so it survives line shifts, and is reused on every
run:

| Verdict            | Effect on the numbers                                                                         |
| ------------------ | --------------------------------------------------------------------------------------------- |
| `oracle_wrong`     | the oracle is corrected: an agent-lens-only pair becomes a TP, an oracle-only pair is removed |
| `agent_lens_wrong` | none; counted as a confirmed resolver bug                                                     |
| `definition_gap`   | the pair leaves both sides; counted. Also a prompt to extend the edge definition above        |

A record applies only to pairs the disagreement listing shows, so an
agent-lens-only pair whose caller the oracle cannot see is never adjudicated.
Qualified names are not unique (build-tagged twins, same-named scripts): a
record matching several node pairs applies to all of them, counts once, and
warns on stderr; pin it with optional `caller_file` / `callee_file` (paths
relative to the target root). A verdict that no longer matches a disagreement
(fixed bug, moved code) is reported as stale; delete it. Always fill `note` with the call site and the
reason.

## Targets

`scripts/callgraph-accuracy/targets.toml` pins small, well-tested,
single-language projects by full commit SHA: Go `spf13/pflag`, `google/go-cmp`;
Python `more-itertools`, `toolz`. Add one by resolving a release tag with
`git ls-remote <repo> refs/tags/<tag>` and checking its test suite passes at
that commit.

## Baseline

The number to beat (#578, #586). Measured 2026-09-26 with
`mise run callgraph-accuracy` on agent-lens `bb273cb` (plus the resolver fixes
that landed with this benchmark), with the adjudications committed at the
time. Targets: pflag `0491e57` (v1.0.10), go-cmp `9b12f36` (v0.7.0),
more-itertools `64be96c` (v11.1.0), toolz `568c2b8` (1.1.0); full SHAs in
`targets.toml`. Rerun and compare against this table rather than a single
target.

Per language (targets summed, micro-averaged):

| Language (oracle)   | Precision (type-checker) / unobserved rate (dynamic) | Static recall      | Static pairs only in a candidate set | Dynamic recall | Callback recall | Candidate-set hit rate |
| ------------------- | ---------------------------------------------------- | ------------------ | ------------------------------------ | -------------- | --------------- | ---------------------- |
| Go (go-vta)         | 0.973 (940 / 966)                                    | 0.536 (940 / 1754) | 543                                  | 0 / 1059       | n/a             | 0.920 (623 sets)       |
| Python (setprofile) | 0.239 unobserved (108 / 452)                         | 0.304 (344 / 1132) | 20                                   | 0 / 424        | 0 / 31          | 0.692 (26 sets)        |

Per resolution method:

| Method           | Go precision      | Go static pairs found | Python unobserved rate | Python static pairs found |
| ---------------- | ----------------- | --------------------- | ---------------------- | ------------------------- |
| `lexical`        | 0.986 (436 / 442) | 436                   | 0.121 (35 / 289)       | 254                       |
| `self_method`    | n/a               | n/a                   | 0.070 (3 / 43)         | 40                        |
| `last_segment`   | 0.952 (393 / 413) | 393                   | 0.590 (69 / 117)       | 48                        |
| `path_suffix`    | 1.000 (111 / 111) | 111                   | 0.000 (0 / 2)          | 2                         |
| `crate_narrowed` | resolves nothing  | n/a                   | 1.000 (1 / 1)          | 0                         |

Per target: Go precision pflag 0.989, go-cmp 0.945; Go static recall pflag
0.470, go-cmp 0.724. Python static recall more-itertools 0.129, toolz 0.827;
unobserved rate more-itertools 0.374, toolz 0.155.

Unknown / adjudicated: Go 12 `agent_lens_wrong`, 1887 unadjudicated
disagreements, nothing unmapped, 2 resolved edges excluded (caller in a file
behind a build constraint). Python 4 `oracle_wrong`, 6 `agent_lens_wrong`,
1345 unadjudicated; 183 oracle edges unmapped (179 nested or anonymous
callees, 1 callee and 3 callers with no enclosing node).

What the misses are, from reading the disagreements:

- Go static misses without a call site (84 of 271; every one sampled) are calls inside func
  literals, which agent-lens drops; the rest are method calls on a typed
  package variable or local (`CommandLine.StringP(...)`, `err.Unwrap()`) left
  unresolved.
- Go false positives: a method on a call expression's result read as the
  package-level function of that name (`(expr).String()`,
  `s.statelessCompare(step).Equal()`), and `last_segment` binding a stdlib
  method (`reflect.Type.Name`, `time.Time.IsZero`, `reflect.Value.IsNil`) to
  the only in-repo method with that name.
- Python static misses are almost all `mi.foo(...)` in more-itertools' tests:
  `import more_itertools as mi` over an `__init__.py` that re-exports with
  `from .more import *` (the barrel gap pinned in the metamorphic tests).
- Python unobserved `last_segment` edges are mostly shadowed names: a local, a
  parameter or an `itertools` import bound to a same-named in-repo function.

## Metamorphic checks

Transformations that must not change the graph (renaming functions, aliasing
imports, splitting or moving modules, re-exporting through a barrel file) need
no oracle: any difference is a resolver bug. They run as ordinary cargo tests
in `ci` (`crates/agent-lens/src/analyze/call_graph/metamorphic_tests.rs`), for
Rust, TypeScript, Python and Go. Differences the resolver does not handle yet
(re-exports through a barrel file, relative imports inside a directory index)
are pinned exactly in its `KNOWN_GAPS` table; deleting a row records the fix.

## Existing suites and literature

The PyCG micro-benchmark (112 cases, 16 categories) and SWARM-JS (126 cases,
18 categories) are smoke tests at most; borrow their category taxonomy, not a
headline number. Hand-labelled suites miss edges no tool finds, and their
edge definitions (constructors, builtins, decorators, dispatch, unexecuted
paths) differ from each other more than their annotation errors do.

| Tool                                                                   | Language | Precision | Recall |
| ---------------------------------------------------------------------- | -------- | --------- | ------ |
| [PyCG](https://arxiv.org/abs/2103.00587)                               | Python   | 99%       | 70%    |
| [HeaderGen](https://github.com/secure-software-engineering/HeaderGen)  | Python   | 95%       | 95%    |
| npm-callgraph ([2405.07206](https://arxiv.org/abs/2405.07206))         | JS       | 91%       | 68%    |
| Closure name-matching ([2405.07206](https://arxiv.org/abs/2405.07206)) | JS       | 81%       | 89%    |

These use other edge definitions and oracles, so they are orientation, not a
target to compare against directly.
