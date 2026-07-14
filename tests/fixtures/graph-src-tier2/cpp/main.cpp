// Entry file for the C++ fixture: cross-file calls into geometry.cpp.
#include "geometry.hpp"

namespace geo {

/// Cross-file: constructs Points and calls their method.
Scalar total_distance(const Point *pts, int n) {
    Scalar total = 0.0;
    for (int i = 1; i < n; i++) {
        total += pts[i - 1].distanceTo(pts[i]);
    }
    return total;
}

/// Heap construction (instantiation), a namespace-qualified call, a member call.
Scalar make_and_measure() {
    Point *p = new Point(3.0, 4.0);
    Scalar h = geo::hypot_scalar(p->x, p->y);
    return h + p->distanceTo(*p);
}

} // namespace geo

int main() {
    geo::Point a(0.0, 0.0);
    geo::Point b(3.0, 4.0);
    return 0;
}
