# A top-level constant.
PI = 3.14159

# Free function used across files.
def circle_area(radius)
  radius * radius * PI
end

module Geometry
  # A class with methods.
  class Point
    # Constructor.
    def initialize(x, y)
      @x = x
      @y = y
    end

    def distance_to(other)
      hypot(@x - other.x, @y - other.y)
    end

    def self.origin
      Point.new(0, 0)
    end
  end
end

def hypot(a, b)
  Math.sqrt(a * a + b * b)
end
