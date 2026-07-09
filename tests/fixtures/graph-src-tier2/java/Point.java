package fixture;

/** A point with fields and an instance method. */
public class Point {
    public double x;
    public double y;

    /** Constructor. */
    public Point(double x, double y) {
        this.x = x;
        this.y = y;
    }

    /** Instance method calling a static cross-file function. */
    public double distanceTo(Point other) {
        double dx = this.x - other.x;
        double dy = this.y - other.y;
        return Geometry.hypot(dx, dy);
    }
}
