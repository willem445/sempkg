<?php

const PI = 3.14159;

/**
 * Free function.
 */
function circleArea(float $radius): float {
    return $radius * $radius * PI;
}

interface Shape {
    public function area(): float;
}

trait Named {
    public function label(): string {
        return "shape";
    }
}

class Circle implements Shape {
    use Named;

    public float $radius;
    const KIND = "circle";

    public function __construct(float $radius) {
        $this->radius = $radius;
    }

    public function area(): float {
        return circleArea($this->radius);
    }
}

enum Suit {
    case Hearts;
    case Spades;
}
