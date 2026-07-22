"""Inheritance: super(), method resolution order, overriding, and mixins."""


class Shape:
    def __init__(self, name):
        self.name = name

    def area(self):
        raise NotImplementedError(f"{type(self).__name__} must define area()")

    def describe(self):
        return f"{self.name} with area {self.area():.2f}"


class Circle(Shape):
    def __init__(self, radius):
        super().__init__("Circle")
        self.radius = radius

    def area(self):
        return 3.14159 * self.radius**2


class Rectangle(Shape):
    def __init__(self, width, height):
        super().__init__("Rectangle")
        self.width, self.height = width, height

    def area(self):
        return self.width * self.height


class Square(Rectangle):
    def __init__(self, side):
        super().__init__(side, side)
        self.name = "Square"


class SerializableMixin:
    def to_dict(self):
        return {k: v for k, v in vars(self).items()}


class Point(SerializableMixin):
    def __init__(self, x, y):
        self.x, self.y = x, y


shapes = [Circle(2), Rectangle(3, 4), Square(5)]
for shape in shapes:
    print(shape.describe())

print("total area:", round(sum(s.area() for s in shapes), 2))
print("MRO:", [c.__name__ for c in Square.__mro__])
print("isinstance:", isinstance(Square(1), Rectangle), isinstance(Square(1), Shape))

# The base class refuses to compute an area it does not define.
try:
    Shape("generic").area()
except NotImplementedError as e:
    print("abstract:", e)

print("mixin:", Point(1, 2).to_dict())
