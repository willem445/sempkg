// Entry point exercising a cross-file call AND a cross-file construction.
func summarize(radius: Scalar) -> Scalar {
    let c = Circle(radius: radius)
    return circleArea(radius: c.radius)
}
