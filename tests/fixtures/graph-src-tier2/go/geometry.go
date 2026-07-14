// Package shapes provides geometry primitives for the Go tier-2 fixture.
//
// Exercises: package-level func, method (receiver), struct + fields, type
// alias, package-level variable, imports, and cross-file calls.
package shapes

import "math"

// Scalar is a type alias over a primitive.
type Scalar = float64

// Point is a struct with named fields (struct members).
type Point struct {
	X Scalar
	Y Scalar
}

// Unit is a package-level variable.
var Unit Scalar = 1.0

// Hypot is a free function used by DistanceTo and across files.
func Hypot(a, b Scalar) Scalar {
	return math.Sqrt(a*a + b*b)
}

// DistanceTo is a method with a value receiver (intra-file call).
func (p Point) DistanceTo(other Point) Scalar {
	dx := p.X - other.X
	dy := p.Y - other.Y
	return Hypot(dx, dy)
}
