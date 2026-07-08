"""Entry module for the Python fixture.

Exercises: imports, cross-file calls into ``shapes``, an async function, a
class with a method, and a plain function.
"""

from __future__ import annotations

import asyncio

from shapes import Circle, Kind, Scalar, circle_area


class Report:
    """A class whose method makes cross-file calls into ``shapes``."""

    def __init__(self, kind: Kind) -> None:
        self.kind = kind

    def measure(self, radius: Scalar) -> Scalar:
        circle = Circle(radius)
        return circle.area()


def summarize(radii: list[Scalar]) -> Scalar:
    """Plain function calling a function defined in another file."""
    return sum(circle_area(r) for r in radii)


async def gather_measurements(radii: list[Scalar]) -> Scalar:
    """An async function — the reader must set ``is_async`` on this node."""
    await asyncio.sleep(0)
    return summarize(radii)
