//! Geometry primitives used by the Rust portion of the fixture.
//!
//! Exercises: struct + fields, impl methods, enum + members, a type alias,
//! and free functions that are called across files.

/// A type alias over a primitive — the reader must record `type_alias` nodes.
pub type Scalar = f64;

/// A struct with named fields (struct members).
pub struct Point {
    pub x: Scalar,
    pub y: Scalar,
}

impl Point {
    /// Associated constructor function (method / associated fn).
    pub fn new(x: Scalar, y: Scalar) -> Self {
        Point { x, y }
    }

    /// Instance method that calls a free function in this file (intra-file call).
    pub fn distance_to(&self, other: &Point) -> Scalar {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        hypot(dx, dy)
    }
}

/// An enum with several variants (enum members).
pub enum Shape {
    Circle { radius: Scalar },
    Rectangle { width: Scalar, height: Scalar },
    Empty,
}

impl Shape {
    /// Method with a match over the enum's members.
    pub fn area(&self) -> Scalar {
        match self {
            Shape::Circle { radius } => 3.14159 * radius * radius,
            Shape::Rectangle { width, height } => width * height,
            Shape::Empty => 0.0,
        }
    }
}

/// Free function used by `Point::distance_to`.
pub fn hypot(a: Scalar, b: Scalar) -> Scalar {
    (a * a + b * b).sqrt()
}
