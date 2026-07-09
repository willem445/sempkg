"""Shape helpers for the Python portion of the fixture.

Exercises: classes with methods and members, an enum, a module-level function,
and a type alias.
"""

from __future__ import annotations

from enum import Enum

# A type alias (PEP 613 style assignment) — the reader should record it.
Scalar = float


class Kind(Enum):
    """An enum with several members."""

    CIRCLE = "circle"
    RECTANGLE = "rectangle"
    EMPTY = "empty"


class Circle:
    """A class with an initializer, an attribute member, and a method."""

    def __init__(self, radius: Scalar) -> None:
        self.radius = radius

    def area(self) -> Scalar:
        """Instance method calling a module-level function (intra-file call)."""
        return circle_area(self.radius)


def circle_area(radius: Scalar) -> Scalar:
    """Module-level function used by ``Circle.area``."""
    return 3.14159 * radius * radius


class Shape:
    """A base class."""

    def measure(self) -> Scalar:
        return 0.0


class Square(Shape):
    """A subclass — ``class Square(Shape)`` is an ``extends`` edge (Square -> Shape)."""

    def measure(self) -> Scalar:
        return 1.0
