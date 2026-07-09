package com.example.geo

import kotlin.math.sqrt

const val PI: Double = 3.14159

typealias Scalar = Double

/** Free function. */
fun circleArea(radius: Scalar): Scalar {
    return radius * radius * PI
}

interface Shape {
    fun area(): Scalar
}

data class Point(val x: Scalar, val y: Scalar) {
    fun distanceTo(other: Point): Scalar {
        return hypot(x - other.x, y - other.y)
    }
}

class Circle(val radius: Scalar) : Shape {
    override fun area(): Scalar {
        return circleArea(radius)
    }
}

enum class Suit {
    HEARTS,
    SPADES
}

object Registry {
    fun count(): Int = 0
}

fun Scalar.doubled(): Scalar = this * 2

fun hypot(a: Scalar, b: Scalar): Scalar = sqrt(a * a + b * b)
