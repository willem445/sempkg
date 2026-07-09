/* Entry file for the C fixture: cross-file calls into geometry.c. */
#include "geometry.h"

/** Cross-file: calls point_distance defined in geometry.c. */
Scalar total_distance(const struct Point *pts, int n) {
    Scalar total = 0.0;
    for (int i = 1; i < n; i++) {
        total += point_distance(&pts[i - 1], &pts[i]);
    }
    return total;
}

int main(void) {
    struct Point pts[2] = {{0.0, 0.0}, {3.0, 4.0}};
    return (int) total_distance(pts, 2);
}
