require_relative "shapes"

# Uses a cross-file function.
def summarize(radius)
  area = circle_area(radius)
  puts area
end

summarize(2)
