package fixture;

import java.util.List;

/** An enum with members. */
enum Shape {
    CIRCLE,
    RECTANGLE,
    EMPTY
}

/** An interface with a single method. */
interface Measurable {
    double area();
}

/** Cross-file usage: iterates Points and calls their method. */
class Report {
    /** Sums the pairwise distance of consecutive points. */
    public double total(Point[] points) {
        double sum = 0.0;
        for (int i = 1; i < points.length; i++) {
            // Receiver is an array element — its type cannot be inferred, so
            // CodeGraph drops this call (precision-first).
            sum += points[i - 1].distanceTo(points[i]);
        }
        return sum;
    }

    /** Constructs Points (instantiation) and makes a same-class call. */
    public double originGap() {
        Point origin = new Point(0.0, 0.0);
        Point far = new Point(3.0, 4.0);
        return gap(origin, far);
    }

    /** Same-class helper: an unqualified call target and a typed-receiver call. */
    private double gap(Point a, Point b) {
        return a.distanceTo(b);
    }
}
