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

// An interface — the reader must record an `interface` node. The member returns
// a primitive (`number`) rather than `Scalar`: CodeGraph emits a `references`
// edge from an interface member to a *named* return type, which semgraph does
// not (it does not walk interface bodies for reference sites), and references
// are not an acceptance metric — a primitive return keeps the fixture exact.
export interface Measurable {
  measure(): number;
}

// A base class for the `extends` edge below.
export class Base {}

// `extends Base` is an `extends` edge; `implements Measurable` is an
// `implements` edge (Marker -> Base, Marker -> Measurable).
export class Marker extends Base implements Measurable {
  measure(): number {
    return 0;
  }
}
