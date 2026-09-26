// Package shapes is the oracle's fixture: one call of each dispatch shape.
package shapes

// Shape has two implementations, so a call through it is dynamic.
type Shape interface {
	Area() int
}

// Square is one Shape.
type Square struct{ Side int }

// Area is Square's Shape method.
func (s Square) Area() int {
	return s.Side * s.Side
}

// Rect is the other Shape.
type Rect struct{ W, H int }

// Area is Rect's Shape method.
func (r Rect) Area() int {
	return r.W * r.H
}

func double(x int) int {
	return x * 2
}

// Map is a generic function.
func Map[T any](xs []T, f func(T) T) []T {
	out := make([]T, 0, len(xs))
	for _, x := range xs {
		out = append(out, f(x))
	}
	return out
}

// Total calls Area through the interface.
func Total(shapes []Shape) int {
	sum := 0
	for _, s := range shapes {
		sum += s.Area()
	}
	return sum
}

// Run makes one call of every shape.
func Run() int {
	sq := Square{Side: 2}
	a := sq.Area()
	b := double(a)
	t := Total([]Shape{sq, Rect{W: 1, H: 3}})
	inc := func(x int) int {
		return double(x) + 1
	}
	xs := Map([]int{a, b, t}, inc)
	return inc(xs[0])
}

// Labeled gets Area promoted from its embedded Square.
type Labeled struct {
	Square
	Name string
}

// Describe calls a promoted method through an interface method value.
func Describe() int {
	var s Shape = Labeled{Square: Square{Side: 3}, Name: "l"}
	f := s.Area
	return f() + sq(Square{Side: 1}.Area)
}

func sq(f func() int) int { return f() }

// Stringer is a constraint with one method.
type Stringer interface{ String() string }

// String makes Square a Stringer.
func (s Square) String() string { return "square" }

// MethodValue calls a method through a local method value.
func MethodValue(s Square) string {
	f := s.String
	return f()
}

// MethodExpr calls a method through a local method expression.
func MethodExpr() string {
	f := Square.String
	return f(Square{})
}

// FuncVar calls a function through a local variable.
func FuncVar() int {
	f := double
	return f(1)
}

// Gen calls a method on a type-parameter value.
func Gen[T Stringer](x T) string {
	return x.String()
}

// UseGen instantiates Gen.
func UseGen() string {
	return Gen(Square{})
}
