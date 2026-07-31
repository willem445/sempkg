package com.example.app

import com.example.geo.Circle

// Entry point exercising a cross-file call AND a cross-file construction.
def summarize(radius: Double): Double = {
  val c = new Circle(radius)
  circleArea(radius)
}
