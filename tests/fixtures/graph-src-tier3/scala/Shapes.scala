package com.example.geo

import scala.math.sqrt

val pi: Double = 3.14159

type Scalar = Double

/** Free function. */
def circleArea(radius: Scalar): Scalar = {
  radius * radius * pi
}

trait Shape {
  def area(): Scalar
}

class Circle(val radius: Scalar) extends Shape {
  def area(): Scalar = {
    circleArea(radius)
  }
}

object Registry {
  def count(): Int = 0
}

enum Suit {
  case Hearts
  case Spades
}

def hypot(a: Scalar, b: Scalar): Scalar = sqrt(a * a + b * b)
