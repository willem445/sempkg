package shapes

// Kind enumerates shape kinds (Go's enum idiom via const iota).
type Kind int

const (
	KindCircle Kind = iota
	KindRectangle
	KindEmpty
)

// Shape is an interface (Go has interfaces rather than abstract classes).
type Shape interface {
	Area() Scalar
}

// Circle is a struct implementing Shape.
type Circle struct {
	Radius Scalar
}

// Area implements Shape for Circle.
func (c Circle) Area() Scalar {
	return 3.14159 * c.Radius * c.Radius
}

// TotalDistance does cross-file calls: constructs Points and calls a method.
func TotalDistance(points []Point) Scalar {
	var total Scalar = 0.0
	for i := 1; i < len(points); i++ {
		total += points[i-1].DistanceTo(points[i])
	}
	return total
}
