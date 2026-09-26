//go:build never

package shapes

// debugArea is excluded by its build constraint: it must not be listed in
// analyzed_files, and its call is not an oracle edge.
func debugArea() int { return double(1) }
