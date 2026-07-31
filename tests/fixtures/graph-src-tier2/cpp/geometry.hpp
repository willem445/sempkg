// Geometry primitives used by the C++ portion of the tier-2 fixture.
//
// Exercises: namespace, using-alias (type alias), class + fields + methods,
// scoped enum + members, and free functions called across files.
#ifndef GEOMETRY_HPP
#define GEOMETRY_HPP

namespace geo {

/// A type alias over a primitive (using-declaration).
using Scalar = double;

/// A point class with fields and methods.
class Point {
public:
    Scalar x;
    Scalar y;

    /// Constructor.
    Point(Scalar x, Scalar y);

    /// Instance method calling a free function (cross-file call).
    Scalar distanceTo(const Point &other) const;
};

/// A scoped enumeration with several members.
enum class Shape {
    Circle,
    Rectangle,
    Empty
};

/// Free function used by Point::distanceTo and across files.
Scalar hypot_scalar(Scalar a, Scalar b);

} // namespace geo

#endif
