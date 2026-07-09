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
            sum += points[i - 1].distanceTo(points[i]);
        }
        return sum;
    }
}
