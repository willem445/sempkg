package com.example.app

import com.example.geo.Circle

fun summarize(radius: Double): Double {
    val c = Circle(radius)
    return c.area()
}
