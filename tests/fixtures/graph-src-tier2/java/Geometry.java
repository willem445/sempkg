package fixture;

/** Geometry helpers for the Java tier-2 fixture. */
public class Geometry {
    /** A class-level constant field. */
    public static final double UNIT = 1.0;

    /** Static free function used by Point.distanceTo and across files. */
    public static double hypot(double a, double b) {
        return Math.sqrt(a * a + b * b);
    }
}
