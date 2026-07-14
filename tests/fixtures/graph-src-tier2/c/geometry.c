/* Geometry function implementations for the C fixture. */
#include "geometry.h"
#include <math.h>

/** A file-scoped global variable. */
const Scalar UNIT = 1.0;

/** Free function used by point_distance and across files. */
Scalar hypot_scalar(Scalar a, Scalar b) {
    return sqrt(a * a + b * b);
}

/** Computes the distance between two points (intra-file call). */
Scalar point_distance(const struct Point *a, const struct Point *b) {
    Scalar dx = a->x - b->x;
    Scalar dy = a->y - b->y;
    return hypot_scalar(dx, dy);
}
