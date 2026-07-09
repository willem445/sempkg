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

    public class Circle : IShape
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
    }

    public enum Suit
    {
        Hearts,
        Spades
    }
}
