package com.example.app

// Entry point exercising a cross-file call into Shapes.scala.
def summarize(radius: Double): Double = {
  circleArea(radius)
}
