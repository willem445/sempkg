package fixture;

/** An abstract base class. */
abstract class Animal {
    abstract double weight();
}

/**
 * Extends a base class (→ `extends`) and implements the cross-file `Measurable`
 * interface (→ `implements`); its `pair` method references user types in its
 * signature (→ `references` to Dog and Animal).
 */
class Dog extends Animal implements Measurable {
    public double weight() { return 1.0; }

    public double area() { return 0.0; }

    public Dog pair(Animal other) { return this; }
}
