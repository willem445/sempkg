using System;
using App.Geo;

namespace App.Runner
{
    public class Program
    {
        public static double Summarize(double radius)
        {
            var c = new Circle(radius);
            return c.Area();
        }
    }
}
