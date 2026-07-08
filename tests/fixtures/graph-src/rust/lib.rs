//! Entry module for the Rust fixture.
//!
//! Exercises: imports (`use`), cross-file calls into `geometry`, an async fn,
//! and a plain function.

mod geometry;

use geometry::{hypot, Point, Scalar, Shape};

/// Cross-file calls: constructs a `Point` and invokes its method, both defined
/// in `geometry.rs`.
pub fn total_distance(points: &[(Scalar, Scalar)]) -> Scalar {
    let mut total = 0.0;
    for window in points.windows(2) {
        let a = Point::new(window[0].0, window[0].1);
        let b = Point::new(window[1].0, window[1].1);
        total += a.distance_to(&b);
    }
    total
}

/// Uses the enum and calls a free function from the other file.
pub fn describe(shape: &Shape) -> Scalar {
    let raw = shape.area();
    hypot(raw, 0.0)
}

/// An async function — the reader must set `is_async` on this node.
pub async fn fetch_and_measure(points: Vec<(Scalar, Scalar)>) -> Scalar {
    // Pretend this awaits I/O; the point is the async signature.
    total_distance(&points)
}
