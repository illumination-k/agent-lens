package main

import (
	"reflect"
	"testing"
)

// Line numbers refer to testdata/sample/shapes/shapes.go.
const (
	squareArea = 13
	rectArea   = 21
	double     = 25
	mapFn      = 30
	total      = 39
	run        = 48
	describe   = 67
	sqFn       = 73
	sqString   = 79
	methodVal  = 82
	methodExpr = 88
	funcVar    = 94
	gen        = 100
	useGen     = 105
)

func TestRunSample(t *testing.T) {
	out, stats, err := Run("testdata/sample", []string{"./..."})
	if err != nil {
		t.Fatal(err)
	}
	if out.Oracle != "go-vta" || out.Language != "go" || out.Kind != "type-checker" {
		t.Errorf("header = %q %q %q", out.Oracle, out.Language, out.Kind)
	}
	const f = "shapes/shapes.go"
	e := func(caller, line, callee int, dispatch string) Edge {
		return Edge{f, caller, line, f, callee, dispatch}
	}
	want := []Edge{
		// Interface call with two implementations: one dynamic edge each.
		e(total, 42, squareArea, "dynamic"),
		e(total, 42, rectArea, "dynamic"),
		// Method on a concrete type, then a direct call.
		e(run, 50, squareArea, "static"),
		e(run, 51, double, "static"),
		e(run, 52, total, "static"),
		// A call inside a closure belongs to the enclosing named function,
		// at the closure's own call line.
		e(run, 54, double, "static"),
		// A generic function maps to its declaration, not the instance.
		e(run, 56, mapFn, "static"),
		// Interface method value on a promoted method: the $bound and
		// promotion wrappers collapse into Square.Area.
		e(describe, 70, squareArea, "dynamic"),
		e(describe, 70, sqFn, "static"),
		// Concrete method value called through a func parameter.
		e(sqFn, 73, squareArea, "dynamic"),
		// SSA forwards a local function value to its callee, but the
		// source calls through the value: dynamic.
		e(methodVal, 84, sqString, "dynamic"),
		e(methodExpr, 90, sqString, "dynamic"),
		e(funcVar, 96, double, "dynamic"),
		// A method on a type parameter depends on the instantiation.
		e(gen, 101, sqString, "dynamic"),
		e(useGen, 106, gen, "static"),
		// Test files are loaded; the synthetic test main is not emitted.
		{"shapes/shapes_test.go", 5, 6, f, run, "static"},
	}
	if !reflect.DeepEqual(out.Edges, want) {
		t.Errorf("edges:\n got %+v\nwant %+v", out.Edges, want)
	}
	// A file excluded by a build constraint is not analyzed.
	wantFiles := []string{"shapes/shapes.go", "shapes/shapes_test.go"}
	if !reflect.DeepEqual(out.AnalyzedFiles, wantFiles) {
		t.Errorf("analyzed files = %v, want %v", out.AnalyzedFiles, wantFiles)
	}
	// inc(xs[0]) at line 57 and f(x) inside Map both call the closure.
	if stats.ClosureCallee == 0 {
		t.Error("closure callees were not counted")
	}
}

func TestRelExcludesVendorAndOutside(t *testing.T) {
	e := &extractor{root: "/r", generated: map[string]bool{"/r/gen.go": true}, relCache: map[string]string{}}
	cases := map[string]string{
		"/r/a/b.go":        "a/b.go",
		"/r/vendor/x/y.go": "",
		"/r/a/vendor/y.go": "",
		"/r/gen.go":        "",
		"/elsewhere/c.go":  "",
		"/r/../r2/c.go":    "",
	}
	for in, want := range cases {
		if got := e.rel(in); got != want {
			t.Errorf("rel(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestSplitPatterns(t *testing.T) {
	got := splitPatterns("./a/..., ./b  ./c")
	want := []string{"./a/...", "./b", "./c"}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("got %v, want %v", got, want)
	}
}
