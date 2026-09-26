# oracle-go

The Go type-checker oracle of the call-graph accuracy benchmark (#579). It loads
one Go module with `go/packages` (tests included), builds SSA, computes the call
graph with `go/callgraph/vta` seeded by `go/callgraph/cha`, and writes the edges
between functions declared under the root in the shared oracle contract
(`"oracle": "go-vta"`, `"kind": "type-checker"`, no `executed_functions`).

```sh
go run . -root <module dir> -out <file.json> [-patterns ./...]
```

Needs Go 1.25 or newer. Logs and the drop counts go to stderr.

## Edge conventions

- **Root is one Go module.** `./...` does not cross into a nested module, so a
  nested module's functions are never loaded: their edges are missing, not
  wrong. A `go.work` root is not supported. Ill-typed packages are left out of
  the SSA program with a warning.
- **Caller** is the enclosing _named_ function. A call inside a function literal
  is attributed to the literal's outermost named parent (`caller_def_line`),
  while `call_line` stays the real call site. The stderr line reports how many
  edges come from inside a literal; agent-lens attributes them the same way.
- **Callee** is a named function or method. Generic instances map to their
  generic declaration. Synthetic callees (method-value `$bound`, `$thunk`,
  promoted-method wrappers, instantiation wrappers) are collapsed into the named
  functions they forward to. Anonymous function literal callees are dropped and
  counted on stderr. Package initializers are neither callers nor callees; an
  explicit `func init` is an ordinary function.
- **Lines.** `caller_def_line` / `callee_def_line` are the line of the `func`
  keyword, never the doc comment. `call_line` is the line of the call's `(` (of
  the `go` / `defer` keyword for those statements), as `go/ssa` records it.
- **Dispatch** is `static` when the call expression names its callee (its
  `Fun` is an identifier or selector resolving to a declared function, or to a
  method on a concrete receiver) and SSA agrees
  (`CallCommon.StaticCallee() != nil`). Otherwise it is `dynamic`: interface
  invokes, calls through a function value (a parameter, a field, or a local
  that SSA forwards to its callee, as in `f := Plain; f()` or
  `f := a.String; f()`), a method called on a type-parameter value inside a
  generic body (its callee depends on the instantiation), and a static call
  whose collapsed wrapper chain itself dispatches dynamically.
- **Excluded**: endpoints outside the root, under a `vendor/` directory, or in a
  file with a `// Code generated ... DO NOT EDIT.` header; the synthetic test
  main (it lives in the build cache, outside the root).

Output is sorted and deduplicated on every field.
