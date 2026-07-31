// Entry module for the TypeScript fixture.
//
// Exercises: imports, cross-file calls into `geometry`, an async function, a
// class with a method, and a plain function.

import { hypot, Kind, Point, Scalar } from "./geometry";

// A class whose method makes cross-file calls into `geometry`.
export class Report {
  kind: Kind;

  constructor(kind: Kind) {
    this.kind = kind;
  }

  measure(points: Array<[Scalar, Scalar]>): Scalar {
    let total = 0;
    for (let i = 1; i < points.length; i++) {
      const a = new Point(points[i - 1][0], points[i - 1][1]);
      const b = new Point(points[i][0], points[i][1]);
      total += a.distanceTo(b);
    }
    return total;
  }
}

// Plain function calling a function defined in another file.
export function magnitude(x: Scalar, y: Scalar): Scalar {
  return hypot(x, y);
}

// An async function — the reader must set `is_async` on this node.
export async function fetchAndMeasure(
  points: Array<[Scalar, Scalar]>,
): Promise<Scalar> {
  const report = new Report(Kind.Circle);
  return report.measure(points);
}
