/* Geometry primitives used by the C portion of the tier-2 fixture.
 *
 * Exercises: typedef (type alias), struct + fields, enum + members, and free
 * function declarations that are called across files.
 */
#ifndef GEOMETRY_H
#define GEOMETRY_H

/** A type alias over a primitive (typedef). */
typedef double Scalar;

/** A struct with named fields (struct members). */
struct Point {
    Scalar x;
    Scalar y;
};

/** An enumeration with several members. */
enum Shape {
    SHAPE_CIRCLE,
    SHAPE_RECTANGLE,
    SHAPE_EMPTY
};

/** Free function used by point_distance and across files. */
Scalar hypot_scalar(Scalar a, Scalar b);

/** Distance between two points. */
Scalar point_distance(const struct Point *a, const struct Point *b);

#endif
