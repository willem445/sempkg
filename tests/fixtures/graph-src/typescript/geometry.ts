// Geometry helpers for the TypeScript portion of the fixture.
//
// Exercises: a type alias, an enum + members, a class with methods and a
// member, and an exported free function.

// A type alias — the reader should record a `type_alias` node.
export type Scalar = number;

// An enum with several members.
export enum Kind {
  Circle = "circle",
  Rectangle = "rectangle",
  Empty = "empty",
}

// A class with a member field and methods.
export class Point {
  x: Scalar;
  y: Scalar;

  constructor(x: Scalar, y: Scalar) {
    this.x = x;
    this.y = y;
  }

  // Method that calls a free function in this file (intra-file call).
  distanceTo(other: Point): Scalar {
    return hypot(this.x - other.x, this.y - other.y);
  }
}

// Exported free function used by `Point.distanceTo`.
export function hypot(a: Scalar, b: Scalar): Scalar {
  return Math.sqrt(a * a + b * b);
}
