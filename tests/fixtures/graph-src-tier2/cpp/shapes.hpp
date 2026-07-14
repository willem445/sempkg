// Inheritance constructs for the C++ tier-2 fixture (issue #78 edge alignment).
//
// Exercises single + multiple inheritance. CodeGraph 0.9.7 records every base in
// a `base_class_clause` as an `extends` edge (C++ has no `interface` kind, so an
// abstract base is still `extends`). Return types are primitive (`double`) here
// so CodeGraph's return-type-misread fabrication does not fire — that quirk is
// exercised (and whitelisted) by geometry.hpp's `Scalar`-returning method.
#ifndef SHAPES_HPP
#define SHAPES_HPP

namespace geo {

/// An abstract base (interface-like): a pure virtual method.
class Drawable {
public:
    virtual double area() const = 0;
};

/// A concrete base class with a field.
class Solid {
public:
    double density;
};

/// Single inheritance from a concrete base.
class Disc : public Solid {
public:
    double radius;
};

/// Multiple inheritance: a concrete base + an abstract (interface-like) base.
class Prism : public Solid, public Drawable {
public:
    double side;
    double area() const;
};

} // namespace geo

#endif
