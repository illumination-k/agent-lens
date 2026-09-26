// Command oracle-go is the Go type-checker oracle of the call-graph accuracy
// benchmark (issue #579). It builds the SSA form of one Go module, computes
// its call graph with go/callgraph/vta (seeded by CHA), and writes the edges
// between functions declared under -root in the shared oracle contract:
//
//	go run . -root <module dir> -out <file.json> [-patterns ./...]
//
// -root must be a single Go module: `./...` does not cross into nested
// modules, so their functions are never loaded and their edges are missing
// (not wrong). A go.work root is not supported.
//
// Edge conventions (see README.md next to this file):
//   - caller: the enclosing *named* function of the call site; a closure's
//     call is attributed to the function literal's outermost named parent,
//     but call_line stays the real call site.
//   - callee: a named function or method; generic instances map to their
//     origin. Synthetic callees (method-value `$bound`, `$thunk`, promoted
//     method and instantiation wrappers) are collapsed into the named
//     functions they forward to. Anonymous closure callees are dropped and
//     counted on stderr.
//   - def lines are the line of the `func` keyword (never the doc comment).
//   - call_line is the line of the call's `(` (the `go` / `defer` keyword for
//     those statements), as go/ssa records it.
//   - dispatch "static" when the call expression names its callee: its Fun
//     is an identifier or selector resolving (types.Info.Uses) to a declared
//     function or to a method on a concrete (non-type-parameter) receiver, and
//     SSA agrees (CallCommon.StaticCallee != nil). "dynamic" for interface
//     invokes, calls through a function value (a parameter, a field, or a
//     local SSA forwards: `f := Plain; f()`, `f := a.M; f()`), a method call
//     on a type-parameter value in a generic body, and a static call whose
//     collapsed wrapper chain itself dispatches dynamically.
//
// Logs go to stderr; the JSON goes to -out (or stdout when -out is empty).
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"go/ast"
	"go/token"
	"go/types"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"golang.org/x/tools/go/callgraph"
	"golang.org/x/tools/go/callgraph/cha"
	"golang.org/x/tools/go/callgraph/vta"
	"golang.org/x/tools/go/packages"
	"golang.org/x/tools/go/ssa"
	"golang.org/x/tools/go/ssa/ssautil"
)

// Edge is one caller -> callee pair of the shared oracle contract.
type Edge struct {
	CallerFile    string `json:"caller_file"`
	CallerDefLine int    `json:"caller_def_line"`
	CallLine      int    `json:"call_line"`
	CalleeFile    string `json:"callee_file"`
	CalleeDefLine int    `json:"callee_def_line"`
	Dispatch      string `json:"dispatch"`
}

// Output is the shared oracle contract document.
type Output struct {
	Oracle   string `json:"oracle"`
	Language string `json:"language"`
	Kind     string `json:"kind"`
	Root     string `json:"root"`
	Edges    []Edge `json:"edges"`
	// AnalyzedFiles lists every file under root the oracle type-checked
	// (relative posix paths). A file left out by build constraints
	// (`//go:build cmp_debug`, another GOOS) is absent, so the scorer does
	// not count agent-lens edges from it as false positives.
	AnalyzedFiles []string `json:"analyzed_files"`
}

// Stats counts what was dropped, for the stderr summary.
type Stats struct {
	SiteEdges        int // call-graph edges with a call site
	SyntheticCaller  int // caller has no named function (package initializer, wrapper body)
	ClosureCallee    int // callee is a function literal
	OutsideRoot      int // an endpoint outside root, in vendor/, or in a generated file
	NoPosition       int // an endpoint with no source position
	CallLineFallback int // call site with no position; call_line = caller_def_line
	InClosure        int // emitted edges whose call site is inside a function literal
}

func main() {
	root := flag.String("root", "", "directory of the Go module to analyze (required)")
	out := flag.String("out", "", "output JSON file (default stdout)")
	patterns := flag.String("patterns", "./...", "package patterns, separated by spaces or commas")
	flag.Parse()
	if *root == "" {
		fmt.Fprintln(os.Stderr, "oracle-go: -root is required")
		os.Exit(2)
	}
	res, stats, err := Run(*root, splitPatterns(*patterns))
	if err != nil {
		fmt.Fprintln(os.Stderr, "oracle-go:", err)
		os.Exit(1)
	}
	fmt.Fprintf(os.Stderr, "oracle-go: %d edges from %d call-graph site edges; dropped: %d synthetic caller, %d closure callee, %d outside root/vendor/generated, %d no position; %d call lines fell back to the caller def line; %d edges have their call site inside a function literal\n",
		len(res.Edges), stats.SiteEdges, stats.SyntheticCaller, stats.ClosureCallee, stats.OutsideRoot, stats.NoPosition, stats.CallLineFallback, stats.InClosure)
	if err := write(*out, res); err != nil {
		fmt.Fprintln(os.Stderr, "oracle-go:", err)
		os.Exit(1)
	}
}

func splitPatterns(s string) []string {
	return strings.FieldsFunc(s, func(r rune) bool { return r == ',' || r == ' ' || r == '\t' })
}

func write(path string, res *Output) error {
	var w io.Writer = os.Stdout
	if path != "" {
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			return err
		}
		f, err := os.Create(path)
		if err != nil {
			return err
		}
		defer f.Close()
		w = f
	}
	enc := json.NewEncoder(w)
	enc.SetIndent("", "  ")
	return enc.Encode(res)
}

// Run loads the module at root and returns its oracle edges.
func Run(root string, patterns []string) (*Output, *Stats, error) {
	absRoot, err := filepath.Abs(root)
	if err != nil {
		return nil, nil, err
	}
	if real, err := filepath.EvalSymlinks(absRoot); err == nil {
		absRoot = real
	}
	cfg := &packages.Config{
		Mode:  packages.LoadAllSyntax | packages.NeedModule,
		Tests: true,
		Dir:   absRoot,
	}
	pkgs, err := packages.Load(cfg, patterns...)
	if err != nil {
		return nil, nil, fmt.Errorf("load: %w", err)
	}
	if len(pkgs) == 0 {
		return nil, nil, fmt.Errorf("no packages matched %v under %s", patterns, absRoot)
	}
	// Ill-typed packages are left out of the SSA program by ssautil, so
	// their edges go missing; say so rather than fail the whole run.
	packages.Visit(pkgs, nil, func(p *packages.Package) {
		for _, e := range p.Errors {
			fmt.Fprintf(os.Stderr, "oracle-go: warning: %s: %v\n", p.ID, e)
		}
	})

	prog, _ := ssautil.AllPackages(pkgs, ssa.InstantiateGenerics)
	prog.Build()
	funcs := ssautil.AllFunctions(prog)
	cg := vta.CallGraph(funcs, cha.CallGraph(prog))

	e := &extractor{
		fset:      prog.Fset,
		root:      absRoot,
		generated: generatedFiles(pkgs),
		relCache:  map[string]string{},
		stats:     &Stats{},
		seen:      map[Edge]bool{},
		named:     namedCallSites(pkgs),
	}
	for _, n := range cg.Nodes {
		for _, out := range n.Out {
			e.edge(out)
		}
	}
	sort.Slice(e.edges, func(i, j int) bool { return less(e.edges[i], e.edges[j]) })
	return &Output{
		Oracle:        "go-vta",
		Language:      "go",
		Kind:          "type-checker",
		Root:          absRoot,
		Edges:         e.edges,
		AnalyzedFiles: e.analyzedFiles(pkgs),
	}, e.stats, nil
}

// analyzedFiles returns the files under root (not vendored, not generated)
// of every loaded package, sorted.
func (e *extractor) analyzedFiles(pkgs []*packages.Package) []string {
	set := map[string]bool{}
	packages.Visit(pkgs, nil, func(p *packages.Package) {
		for _, f := range p.Syntax {
			if rel := e.rel(p.Fset.Position(f.Package).Filename); rel != "" {
				set[rel] = true
			}
		}
	})
	files := make([]string, 0, len(set))
	for f := range set {
		files = append(files, f)
	}
	sort.Strings(files)
	return files
}

func less(a, b Edge) bool {
	if a.CallerFile != b.CallerFile {
		return a.CallerFile < b.CallerFile
	}
	if a.CallerDefLine != b.CallerDefLine {
		return a.CallerDefLine < b.CallerDefLine
	}
	if a.CallLine != b.CallLine {
		return a.CallLine < b.CallLine
	}
	if a.CalleeFile != b.CalleeFile {
		return a.CalleeFile < b.CalleeFile
	}
	if a.CalleeDefLine != b.CalleeDefLine {
		return a.CalleeDefLine < b.CalleeDefLine
	}
	return a.Dispatch < b.Dispatch
}

// generatedFiles returns the files carrying a "Code generated ... DO NOT
// EDIT." header, across every loaded package.
func generatedFiles(pkgs []*packages.Package) map[string]bool {
	gen := map[string]bool{}
	packages.Visit(pkgs, nil, func(p *packages.Package) {
		for _, f := range p.Syntax {
			if ast.IsGenerated(f) {
				gen[p.Fset.Position(f.Package).Filename] = true
			}
		}
	})
	return gen
}

type extractor struct {
	fset      *token.FileSet
	root      string
	generated map[string]bool
	relCache  map[string]string // absolute file -> relative path, "" when excluded
	stats     *Stats
	seen      map[Edge]bool
	edges     []Edge
	// named maps the position go/ssa gives a call site (the call's `(`, or
	// the `go` / `defer` keyword) to whether its call expression names the
	// callee statically.
	named map[token.Pos]bool
}

// namedCallSites records, for every call expression of every loaded
// package, whether it names its callee statically (see namesCallee).
func namedCallSites(pkgs []*packages.Package) map[token.Pos]bool {
	sites := map[token.Pos]bool{}
	packages.Visit(pkgs, nil, func(p *packages.Package) {
		if p.TypesInfo == nil {
			return
		}
		for _, f := range p.Syntax {
			ast.Inspect(f, func(n ast.Node) bool {
				switch n := n.(type) {
				case *ast.CallExpr:
					sites[n.Lparen] = namesCallee(p.TypesInfo, n)
				case *ast.GoStmt:
					sites[n.Go] = namesCallee(p.TypesInfo, n.Call)
				case *ast.DeferStmt:
					sites[n.Defer] = namesCallee(p.TypesInfo, n.Call)
				}
				return true
			})
		}
	})
	return sites
}

// namesCallee reports whether call's Fun is an identifier or selector that
// resolves to a declared function or method, not a variable, and does not
// select a method through a type parameter (whose callee depends on the
// instantiation, like an interface call).
func namesCallee(info *types.Info, call *ast.CallExpr) bool {
	fun := ast.Unparen(call.Fun)
	switch f := fun.(type) {
	case *ast.IndexExpr: // explicit instantiation: F[int](x)
		fun = ast.Unparen(f.X)
	case *ast.IndexListExpr:
		fun = ast.Unparen(f.X)
	}
	var id *ast.Ident
	switch f := fun.(type) {
	case *ast.Ident:
		id = f
	case *ast.SelectorExpr:
		if sel, ok := info.Selections[f]; ok && isTypeParam(sel.Recv()) {
			return false
		}
		id = f.Sel
	default:
		return false
	}
	_, ok := info.Uses[id].(*types.Func)
	return ok
}

func isTypeParam(t types.Type) bool {
	if p, ok := t.(*types.Pointer); ok {
		t = p.Elem()
	}
	_, ok := types.Unalias(t).(*types.TypeParam)
	return ok
}

// site is a named function's position under root.
type site struct {
	file string
	line int
}

func (e *extractor) edge(out *callgraph.Edge) {
	if out.Site == nil {
		return
	}
	e.stats.SiteEdges++
	caller := named(out.Caller.Func)
	if caller == nil || caller.Synthetic != "" {
		e.stats.SyntheticCaller++
		return
	}
	from, ok := e.position(caller)
	if !ok {
		return
	}
	callLine := from.line
	if pos := out.Site.Pos(); pos.IsValid() {
		callLine = e.fset.Position(pos).Line
	} else {
		e.stats.CallLineFallback++
	}
	inClosure := origin(out.Caller.Func).Parent() != nil
	dynamic := out.Site.Common().StaticCallee() == nil
	if named, ok := e.named[out.Site.Pos()]; ok && !named {
		// SSA forwards a local function value to its static callee; the
		// source still calls through a value.
		dynamic = true
	}
	for _, t := range e.targets(out.Callee, dynamic, map[*callgraph.Node]bool{}) {
		to, ok := e.position(t.fn)
		if !ok {
			continue
		}
		dispatch := "static"
		if t.dynamic {
			dispatch = "dynamic"
		}
		edge := Edge{
			CallerFile:    from.file,
			CallerDefLine: from.line,
			CallLine:      callLine,
			CalleeFile:    to.file,
			CalleeDefLine: to.line,
			Dispatch:      dispatch,
		}
		if !e.seen[edge] {
			e.seen[edge] = true
			e.edges = append(e.edges, edge)
			if inClosure {
				e.stats.InClosure++
			}
		}
	}
}

type target struct {
	fn      *ssa.Function
	dynamic bool
}

// targets resolves a callee node to the named functions it stands for:
// itself, or — for a synthetic wrapper — whatever the wrapper calls.
func (e *extractor) targets(n *callgraph.Node, dynamic bool, visiting map[*callgraph.Node]bool) []target {
	fn := origin(n.Func)
	if fn.Parent() != nil {
		e.stats.ClosureCallee++
		return nil
	}
	if fn.Synthetic == "" {
		return []target{{fn, dynamic}}
	}
	if visiting[n] {
		return nil
	}
	visiting[n] = true
	var ts []target
	for _, out := range n.Out {
		d := dynamic
		if out.Site != nil && out.Site.Common().StaticCallee() == nil {
			d = true
		}
		ts = append(ts, e.targets(out.Callee, d, visiting)...)
	}
	delete(visiting, n)
	return ts
}

// origin maps a generic instance to its generic declaration.
func origin(fn *ssa.Function) *ssa.Function {
	if o := fn.Origin(); o != nil {
		return o
	}
	return fn
}

// named returns the outermost named function enclosing fn (fn itself when
// it is not a function literal), through generic instances.
func named(fn *ssa.Function) *ssa.Function {
	for fn != nil {
		fn = origin(fn)
		if fn.Parent() == nil {
			return fn
		}
		fn = fn.Parent()
	}
	return nil
}

// position returns the file (relative to root) and `func` keyword line of a
// declared function, or false when it is excluded.
func (e *extractor) position(fn *ssa.Function) (site, bool) {
	pos := fn.Pos()
	if decl, ok := fn.Syntax().(*ast.FuncDecl); ok {
		pos = decl.Pos() // the `func` keyword: FuncDecl.Pos never includes the doc comment
	}
	if !pos.IsValid() {
		e.stats.NoPosition++
		return site{}, false
	}
	p := e.fset.Position(pos)
	rel := e.rel(p.Filename)
	if rel == "" {
		e.stats.OutsideRoot++
		return site{}, false
	}
	return site{rel, p.Line}, true
}

// rel maps an absolute file to its posix path relative to root, or "" when
// it is outside root, under a vendor/ directory, or generated.
func (e *extractor) rel(file string) string {
	if r, ok := e.relCache[file]; ok {
		return r
	}
	r := ""
	if !e.generated[file] {
		real := file
		if s, err := filepath.EvalSymlinks(file); err == nil {
			real = s
		}
		if rp, err := filepath.Rel(e.root, real); err == nil && filepath.IsLocal(rp) {
			rp = filepath.ToSlash(rp)
			if rp != "vendor" && !strings.HasPrefix(rp, "vendor/") && !strings.Contains(rp, "/vendor/") {
				r = rp
			}
		}
	}
	e.relCache[file] = r
	return r
}
