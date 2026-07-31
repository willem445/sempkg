// Geometry method/function implementations for the C++ fixture.
#include "geometry.hpp"
#include <cmath>

namespace geo {

/// A file-scoped global variable.
const Scalar UNIT = 1.0;

/// Free function used by Point::distanceTo and across files.
Scalar hypot_scalar(Scalar a, Scalar b) {
    return std::sqrt(a * a + b * b);
}

Point::Point(Scalar x, Scalar y) : x(x), y(y) {}

/// Instance method that calls a free function in this file (intra-file call).
Scalar Point::distanceTo(const Point &other) const {
    Scalar dx = x - other.x;
    Scalar dy = y - other.y;
    return hypot_scalar(dx, dy);
}

} // namespace geo
