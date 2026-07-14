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

abstract class Base(val tag: Scalar)

// The primary parent after `extends` (Base) is the only `extends` edge; the
// `with Shape` mixin produces no edge, matching CodeGraph 0.9.7.
class Ring(val r: Scalar) extends Base(r) with Shape {
  def area(): Scalar = r
}

object Registry {
  def count(): Int = 0
}

enum Suit {
  case Hearts
  case Spades
}

def hypot(a: Scalar, b: Scalar): Scalar = sqrt(a * a + b * b)
