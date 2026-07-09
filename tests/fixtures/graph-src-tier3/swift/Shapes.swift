import Foundation

typealias Scalar = Double

let pi: Scalar = 3.14159

/// Free function.
func circleArea(radius: Scalar) -> Scalar {
    return radius * radius * pi
}

protocol Shape {
    func area() -> Scalar
}

struct Point {
    var x: Scalar
    var y: Scalar

    func distanceTo(other: Point) -> Scalar {
        return hypot(x - other.x, y - other.y)
    }
}

class Base {
    var tag: Scalar = 0
}

class Circle: Base, Shape {
    var radius: Scalar

    init(radius: Scalar) {
        self.radius = radius
    }

    func area() -> Scalar {
        return circleArea(radius: radius)
    }
}

enum Suit {
    case hearts
    case spades
}

func hypot(_ a: Scalar, _ b: Scalar) -> Scalar {
    return (a * a + b * b).squareRoot()
}
