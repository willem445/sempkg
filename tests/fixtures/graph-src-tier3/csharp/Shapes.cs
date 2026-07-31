using System;

namespace App.Geo
{
    /// <summary>Free-standing helper class.</summary>
    public static class MathUtil
    {
        public const double PI = 3.14159;

        public static double CircleArea(double radius)
        {
            return radius * radius * PI;
        }
    }

    public interface IShape
    {
        double Area();
    }

    public struct PointStruct
    {
        public double X;
        public double Y;
    }

    public abstract class Base
    {
        public double Tag;
    }

    public class Circle : Base, IShape
    {
        public double Radius { get; set; }

        private readonly string _label;

        public Circle(double radius)
        {
            Radius = radius;
            _label = "circle";
        }

        public double Area()
        {
            return MathUtil.CircleArea(Radius);
        }

        // A method whose signature references user types → `references` edges to
        // Base (return) and Base (parameter). Base is abstract with no declared
        // constructor, so unlike a `Circle` return there is no type-name→
        // constructor collision to resolve.
        public Base Merge(Base other)
        {
            return other;
        }
    }

    public enum Suit
    {
        Hearts,
        Spades
    }
}
