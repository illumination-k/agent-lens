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
proxy, PyPI, crates.io, npm); mise supplies `go`, `uv` and `node` through the
task's `tools`, and the task adds the `rust-analyzer` rustup component. Output
goes to `target/callgraph-accuracy/` and is never committed:

| Path                                         | Content                                                    |
| -------------------------------------------- | ---------------------------------------------------------- |
| `summary.md`, `summary.json`                 | combined table per language and oracle                     |
| `results/<target>/report.md`, `score.json`   | per-target tables and raw counts                           |
| `results/<target>/disagreements.json`        | every unadjudicated disagreement, each with a TOML snippet |
| `results/<target>/graph.json`, `oracle.json` | the two inputs of the scorer                               |
| `repos/<target>`, `venvs/<target>`           | checkout at the pinned commit, Python venv                 |

Rust checkouts sit under this repository's `target/`, which the root
`Cargo.toml` excludes from its workspace so that cargo treats each as its own.

It is outside `ci` like `bench`: it clones third-party code and runs its test
suite under a profiler, and the result is a report, not a gate. What does run
in `ci`: `ruff check` and the unit tests of the scorer and the Python oracle
(`test:callgraph-accuracy`, under `ci:rust`; `ci_rust.yml` also triggers on
`scripts/callgraph-accuracy/**`) and the metamorphic resolver
tests (cargo tests). The Rust oracle's tests are discovered with the others; its
end-to-end test skips unless rust-analyzer is installed. The Go oracle's own
tests (`go test ./...` in `scripts/callgraph-accuracy/oracle-go`) and the
TypeScript oracle's (`npm ci && npm test` in
`scripts/callgraph-accuracy/oracle-ts`) are not wired into `ci`.

## Edge definition

An edge is **function → function**: from the innermost agent-lens node
containing the call site to the node of the function that runs. Both oracles
are normalised to this before comparison.

| Case                                                                                      | Rule                                                                                                                                                                      |
| ----------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| builtins, stdlib, third-party, vendored, generated code                                   | excluded; only functions defined in files under the analysed root are endpoints                                                                                           |
| constructor                                                                               | Python `A()` → `A.__init__`, TypeScript `new A()` → `A`'s `constructor` (when declared). Go and Rust have no constructors: `NewX()` / `X::new()` is a plain function edge |
| static dispatch                                                                           | a direct call of a named function, or a method on a concrete type: must be a `resolved` edge                                                                              |
| `dyn` / interface / virtual dispatch, call through a function value                       | a **candidate set**, scored apart from static dispatch; not expected to be resolved syntactically                                                                         |
| callback (Python function invoked from C, e.g. `sorted(key=f)`, `map(f, ...)`)            | its own recall row; agent-lens emits no edge for passing a function as a value                                                                                            |
| call inside a closure, lambda, comprehension or nested function                           | attributed to the innermost named function agent-lens has a node for (see below)                                                                                          |
| a closure / lambda / nested function as the callee                                        | no node, so unmapped (`callee_nested_or_anonymous`)                                                                                                                       |
| test code                                                                                 | included (test functions are ordinary nodes with `is_test`)                                                                                                               |
| recursion                                                                                 | kept (self edges count)                                                                                                                                                   |
| decorators                                                                                | the decorated function is the callee; a decorator's wrapper is not an edge of its own (the Python oracle does not implement this: see below)                              |
| implicit calls (Python `__iter__` / `__next__` in a `for`, operators, `with`, properties) | edges, since the function runs, but there is no call expression; missed pairs of this kind show up under `of which no call site`                                          |

What agent-lens does today with nested code, measured on small samples (keep
this table in sync when the resolver changes):

| Construct                             | Rust                                                                          | Go                                                             | Python                                   | TypeScript                                         |
| ------------------------------------- | ----------------------------------------------------------------------------- | -------------------------------------------------------------- | ---------------------------------------- | -------------------------------------------------- |
| calls in a closure / lambda body      | attributed to the enclosing fn                                                | attributed to the enclosing function                           | attributed to the enclosing function     | attributed to the closure's own `::closure#N` node |
| calls in a nested named fn / def      | **dropped**                                                                   | n/a                                                            | attributed to the enclosing function     | attributed to the nested function's own node       |
| calling the closure / nested fn       | `unresolved`                                                                  | `unresolved` (named variable) or `anonymous` (`func(){...}()`) | `unresolved`                             | `unresolved`                                       |
| `A()` on a class                      | n/a                                                                           | n/a                                                            | `unresolved` (no `__init__` edge)        | `new A()`: no call site at all                     |
| calls inside a macro invocation       | **dropped** (`assert_eq!(f(), 1)` records no call site)                       | n/a                                                            | n/a                                      | n/a                                                |
| method on an interface / trait object | `resolved` via `last_segment` if one impl has that name, else a candidate set | same                                                           | same                                     | same                                               |
| decorated def                         | n/a                                                                           | n/a                                                            | node spans from the first decorator line | node spans from the first decorator line           |

A Go oracle edge whose call site is inside a func literal is attributed to
the enclosing named function by both sides (the oracle keeps the real call
line).

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
- Rust: rust-analyzer resolves with one feature set (all features unless the
  target names some), so a fn behind an inactive `cfg` is unresolved: the
  oracle lists it in `unanalyzed_functions` and the scorer leaves agent-lens
  edges from it out of precision. A `#[cfg]` on a block or statement inside
  an analysed fn is not seen that way, so its calls are neither edges nor
  excluded: pick the target's features to keep such code active.
- Rust: a call through a trait (`dyn`, a generic `T: Trait`, a default
  method) is `dynamic` to the trait method and to every in-root impl
  (`textDocument/implementation`): a CHA-style set, far larger than the VTA
  sets of the Go oracle. A trait method declared without a body has no
  agent-lens node, so those edges are unmapped (`callee_no_enclosing_node`).
- TypeScript: interface / abstract method calls fan out only to classes that
  name the type in an `extends` / `implements` clause (transitively); an
  object literal or a structurally compatible class without the clause is not
  a candidate. Implicit calls (getters, JSX elements, tagged templates,
  decorators, iterators) have no edge, and neither do calls whose callee type
  comes from an uninstalled dependency (the checkout has no `node_modules`,
  so such a value is `any`).
- Python: a generator's call is seen only when it is first advanced; the
  edge is kept when that line names the generator (`for x in gen():`) and
  dropped otherwise (counted in the oracle's `stats`); an agent-lens edge
  to such a generator is `oracle_wrong`.

## Oracles

| Language   | Oracle               | Kind         | Tool                                                                                                                          |
| ---------- | -------------------- | ------------ | ----------------------------------------------------------------------------------------------------------------------------- |
| Go         | `go-vta`             | type-checker | `golang.org/x/tools/go/callgraph/vta` over the packages and their tests (`scripts/callgraph-accuracy/oracle-go`)              |
| Python     | `python-setprofile`  | dynamic      | `sys.setprofile` while the project's pytest suite runs (`scripts/callgraph-accuracy/oracle_py.py`)                            |
| Rust       | `rust-analyzer`      | type-checker | rust-analyzer's LSP call hierarchy, plus `textDocument/definition` inside macros and at trait calls (`oracle_rs.py`)          |
| TypeScript | `typescript-checker` | type-checker | the TypeScript compiler API (`checker.getResolvedSignature`) over every TS / JS file (`scripts/callgraph-accuracy/oracle-ts`) |

Static-dispatch edges of a type-checker oracle are near exact, so precision
and recall are both verdicts. A dynamic oracle only sees what the tests run:
an observed edge is certainly real, so **recall** is trustworthy, but an
agent-lens edge that was never observed may still be real, so precision is
replaced by an indicator (below).

### Oracle JSON contract

```json
{
  "oracle": "go-vta | python-setprofile | rust-analyzer | typescript-checker",
  "language": "go | python | rust | typescript",
  "kind": "type-checker | dynamic",
  "root": "<absolute path of the analysed tree>",
  "caller_is_innermost_function": false,
  "edges": [{
    "caller_file": "<posix path relative to root>",
    "caller_def_line": 12,
    "call_line": 15,
    "callee_file": "<relative>",
    "callee_def_line": 40,
    "dispatch": "static | dynamic | callback"
  }],
  "executed_functions": [{ "file": "<relative>", "def_line": 12 }],
  "analyzed_files": ["<relative>"],
  "unanalyzed_functions": [{ "file": "<relative>", "def_line": 12 }]
}
```

- Lines are 1-based. A def line is the line of the `def` / `func` / `fn`
  keyword (Python: after skipping decorator lines); TypeScript: the first line
  of the declaration, decorators and modifiers included, and for a function or
  arrow that initialises a variable, property or object key, the line of that
  declaration.
- Edges are deduplicated on all fields; only functions defined under `root`
  appear.
- `static`: callee fixed at compile time. `dynamic`: interface / virtual
  dispatch or a call through a function value. `callback`: a Python function
  invoked from C code, attributed to the nearest Python frame.
- `executed_functions` (dynamic oracles only): every function observed running.
- `analyzed_files` (optional, type-checker oracles): every file under `root`
  the oracle type-checked. When present, an agent-lens edge whose caller is in
  another file is outside the oracle's view and left out of precision.
- `unanalyzed_functions` (optional, type-checker oracles): functions in an
  analysed file the oracle could not resolve (Rust: cfg-inactive). An
  agent-lens edge whose caller maps to one is left out of precision, as for
  `analyzed_files`.
- `caller_is_innermost_function` (optional, default false): when true,
  `caller_def_line` is the innermost function around the call, anonymous ones
  included, and the scorer maps the caller from it (see below).
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
- The Rust oracle: `static` unless the callee is declared in a `trait` block;
  then `dynamic`, plus a `dynamic` edge to every in-root impl of that method.
  Calls rust-analyzer's call hierarchy lists come with the callee pinned by
  the type checker; for a trait method (std traits included) the call site is
  re-resolved with `textDocument/definition`, which gives the impl when the
  receiver type is known (`Version::from_str(s)`). Calls inside macro
  arguments, which the call hierarchy skips, are found by lexing each macro
  invocation's token tree for `name(` / `.name(` / `name::<..>(` and asking
  `textDocument/definition` at each. `call_line` is the line of the callee's
  name.
- The TypeScript oracle: `static` when the callee expression names the callee
  (the identifier or property resolves, through import aliases, to the
  function, the method, or a `const` / `readonly` binding initialised with
  it), and for `new C()` / `super()`; `dynamic` for a call through any other
  value, for an interface / abstract method (to every class implementing it
  by heritage clause) and for a method on a union-typed receiver (to every
  member's method). `call_line` is the line of the callee's name.

### Mapping to agent-lens nodes

- Caller: the innermost node in `caller_file` whose `[start_line, end_line]`
  contains `call_line` (fallback: `caller_def_line`). With
  `caller_is_innermost_function`, the node defined at `caller_def_line`, else
  the innermost node containing it: mapping by `call_line` would give a call
  to the callback that opens on its own line (`safeTry(function* () {`), which
  TypeScript nodes (one per closure) make common.
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
`self_method`, `last_segment`, `path_suffix`, `crate_narrowed`, and for
TypeScript `binding`). A resolved edge with no caller node (a call in
module-level code: a Rust `const` initialiser) is outside every set and only
counted (`resolved edge excluded`).

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
  (implicit calls such as `__iter__`, calls in Rust macro arguments); the rest of
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
Python `more-itertools`, `toolz`; Rust `dtolnay/semver`, `rust-lang/log`;
TypeScript `supermacro/neverthrow`, `pmndrs/zustand`. A Rust target may
name the cargo `features` rust-analyzer enables (default: all). Add one by resolving a release tag with
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

- Go static misses without a call site (84 of 271; every one sampled) were calls inside func
  literals, which agent-lens dropped (fixed since: see "Go" below); the rest are method calls on a typed
  package variable or local (`CommandLine.StringP(...)`, `err.Unwrap()`) left
  unresolved.
- Go false positives: a method on a call expression's result read as the
  package-level function of that name (`(expr).String()`,
  `s.statelessCompare(step).Equal()`), and `last_segment` binding a stdlib
  method (`reflect.Type.Name`, `time.Time.IsZero`, `reflect.Value.IsNil`) to
  the only in-repo method with that name. All but `reflect.Type.Name` are
  fixed since (see "Go" below).
- Python static misses are almost all `mi.foo(...)` in more-itertools' tests:
  `import more_itertools as mi` over an `__init__.py` that re-exports with
  `from .more import *` (the barrel gap pinned in the metamorphic tests).
- Python unobserved `last_segment` edges are mostly shadowed names: a local, a
  parameter or an `itertools` import bound to a same-named in-repo function.

### Go

Measured 2026-09-26 on pflag and go-cmp after four Go fixes, found by running
the oracle over google/uuid `0f11ee6` (v1.6.0), BurntSushi/toml `d97def5`
(v1.5.0), samber/lo `203faca` (v1.51.0), gorilla/mux `b4617d0` (v1.8.1) and
spf13/cast `40e8e07` (v1.9.2):

- calls inside a func literal are attributed to the enclosing function
  instead of dropped;
- a method on a value that is no plain path (`NewDecoder(r).Decode(v)`,
  `net.IP(b).String()`) is a receiver call, not a bare call to the
  package-level function named like it;
- the name fallback keeps a bare call to free functions (`Domain(b)` is a
  conversion or a function, never `UUID.Domain`) and a receiver call to
  methods (`wg.Add(1)` is never the package-level `tag.Add`);
- an explicit single type argument (`Empty[K]()`) names the generic
  function instead of reading as an index;

plus `IsNil` / `IsZero` in Go's ubiquitous method names.

Per repository, after the fixes (the five discovery targets are not in
`targets.toml`; their precision counts every agent-lens-only pair as a false
positive, since none is adjudicated):

| Repository                  | Commit    | Precision           | Static recall       | Static pairs only in a candidate set | Dynamic recall |
| --------------------------- | --------- | ------------------- | ------------------- | ------------------------------------ | -------------- |
| spf13/pflag v1.0.10         | `0491e57` | 0.985 (1098 / 1115) | 0.841 (1094 / 1301) | 30                                   | 4 / 837        |
| google/go-cmp v0.7.0        | `9b12f36` | 1.000 (376 / 376)   | 0.830 (376 / 453)   | 63                                   | 0 / 222        |
| google/uuid v1.6.0          | `0f11ee6` | 1.000 (159 / 159)   | 0.828 (159 / 192)   | 8                                    | 0 / 5          |
| BurntSushi/toml v1.5.0      | `d97def5` | 0.972 (692 / 712)   | 0.889 (690 / 776)   | 65                                   | 2 / 87         |
| samber/lo v1.51.0           | `203faca` | 0.993 (144 / 145)   | 0.706 (144 / 204)   | 7                                    | 0 / 3          |
| gorilla/mux v1.8.1          | `b4617d0` | 0.981 (358 / 365)   | 0.669 (358 / 535)   | 177                                  | 0 / 66         |
| spf13/cast v1.9.2           | `40e8e07` | 0.887 (94 / 106)    | 0.803 (94 / 117)    | 0                                    | 0 / 37         |
| pflag + go-cmp (micro-avg.) |           | 0.989 (1474 / 1491) | 0.838 (1470 / 1754) | 93                                   | 4 / 1059       |

Against the baseline above, pflag + go-cmp went from precision 0.973 and
static recall 0.536. On the five discovery targets, the fixes raised
precision from 0.968 to 0.973 and static recall from 0.678 to 0.792. What is
left there: `reflect.Value.Type` bound to an in-repo `Type` method by
`crate_narrowed` (toml), `Get` on an `http.Header` (mux), edges to
`zz_generated.go`, which the oracle excludes as generated (cast, all 12 of
its false positives), and lo's self-edges on generic functions
(`Contains → Contains`), which come from the oracle collapsing instantiation
wrappers.

### Rust and TypeScript

Measured 2026-09-26 with `mise run callgraph-accuracy semver log neverthrow
zustand` on agent-lens `ec8e182`, with no adjudications yet (every
disagreement is still unadjudicated, so these precisions count every
agent-lens-only pair as a false positive). Targets: semver `5368cdf` (1.0.28),
log `8034743` (0.4.34, features `kv_serde`, `kv_sval`), neverthrow `1b7a959`
(v8.2.0), zustand `2115efb` (v5.0.15).

| Language (oracle)               | Precision         | Static recall      | Static pairs only in a candidate set | Dynamic recall | Candidate-set hit rate |
| ------------------------------- | ----------------- | ------------------ | ------------------------------------ | -------------- | ---------------------- |
| Rust (rust-analyzer)            | 0.924 (269 / 291) | 0.550 (260 / 473)  | 86                                   | 9 / 606        | 0.932 (118 sets)       |
| TypeScript (typescript-checker) | 0.995 (765 / 769) | 0.704 (764 / 1085) | 148                                  | 1 / 392        | 1.000 (315 sets)       |

| Method           | Rust precision    | Rust static pairs found | TypeScript precision | TypeScript static pairs found |
| ---------------- | ----------------- | ----------------------- | -------------------- | ----------------------------- |
| `lexical`        | 0.942 (162 / 172) | 162                     | n/a                  | n/a                           |
| `self_method`    | 1.000 (20 / 20)   | 18                      | 1.000 (2 / 2)        | 2                             |
| `last_segment`   | 0.792 (38 / 48)   | 37                      | 0.993 (419 / 422)    | 418                           |
| `path_suffix`    | 0.961 (49 / 51)   | 43                      | 1.000 (72 / 72)      | 72                            |
| `binding`        | n/a               | n/a                     | 0.996 (272 / 273)    | 272                           |
| `crate_narrowed` | resolves nothing  | n/a                     | resolves nothing     | n/a                           |

Per target: precision semver 1.000, log 0.847, neverthrow 1.000, zustand
0.981; static recall semver 0.714, log 0.423, neverthrow 0.651, zustand 0.910.

Unknown: Rust 95 oracle edges unmapped (63 to trait methods without a body,
32 to closures / nested fns), 10 resolved edges excluded as their caller is
cfg-inactive, 2 as module-level; TypeScript 8 unmapped (5 callers with no
enclosing node, 3 ambiguous: a `const f = (a) => (b) => {...}` curried arrow
gives two nodes of the same span).

What the misses are, from reading the disagreements:

- Rust static misses without a call site are mostly calls inside macro
  arguments (`assert_eq!(f(x), ...)`, `write!(f, "{}", g())`): 59 of 72 in
  semver. agent-lens records no call site in a macro invocation.
- Rust: semver's `display.rs` has two `Version::fmt` (the `Display` and the
  `Debug` impl); the first gets no edges at all, not even to `pad` or
  `digits`, which are called outside any closure.
- Rust false positives: `last_segment` binding a call to the cfg-selected shim
  (`log::AtomicUsize::load` / `store`, compiled only without native atomics);
  `BTreeMap::get(self, ..)` inside `impl Source for BTreeMap` resolved
  `lexical` to the enclosing method itself.
- TypeScript static misses: calls to functions of a `namespace`
  (`Result.combine(...)`, 66 of neverthrow's 153), `new C()` (35; no call
  site is recorded for a `new` expression), and calls to local closures
  (`const parse = (s) => ...; parse(x)`, all 20 of zustand's).
- TypeScript false positives: `last_segment` binding `useStore()` in the
  zustand examples, where `useStore` is the local result of `create(...)`, to
  `src/react.ts`'s `useStore`.

## Metamorphic checks

Transformations that must not change the graph (renaming functions, aliasing
imports, splitting or moving modules, re-exporting through a barrel file) need
no oracle: any difference is a resolver bug. They run as ordinary cargo tests
in `ci` (`crates/agent-lens/src/analyze/call_graph/metamorphic_tests.rs`), for
Rust, TypeScript, Python and Go. Differences the resolver does not handle yet
(re-exports through a barrel file)
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
