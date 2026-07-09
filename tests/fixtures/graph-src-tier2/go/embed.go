package shapes

// Base is an embeddable struct base.
type Base struct {
	Tag Scalar
}

// Disc embeds Base — Go struct embedding, which CodeGraph 0.9.7 records as an
// `extends` edge (Disc extends Base).
type Disc struct {
	Base
	R Scalar
}

// Reader is an interface.
type Reader interface {
	Read() Scalar
}

// ReadWriter embeds Reader — interface embedding, also an `extends` edge.
type ReadWriter interface {
	Reader
	Write(v Scalar) Scalar
}

// Wrap references user struct types in its signature (a `references` edge to
// each of Base and Disc; the Scalar alias is not referenced).
func Wrap(b Base) Disc {
	return Disc{}
}
