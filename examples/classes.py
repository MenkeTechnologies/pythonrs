class Shape:
    def __init__(self, name):
        self.name = name
    def area(self):
        return 0
    def __repr__(self):
        return f"{self.name}({self.area()})"

class Circle(Shape):
    def __init__(self, r):
        self.name = "Circle"
        self.r = r
    def area(self):
        return round(3.14159 * self.r ** 2, 2)

class Square(Shape):
    def __init__(self, s):
        self.name = "Square"
        self.s = s
    def area(self):
        return self.s * self.s

shapes = [Circle(2), Square(3), Circle(1)]
for sh in shapes:
    print(sh, "area =", sh.area())
print("total:", sum(s.area() for s in shapes))
print("biggest:", max(shapes, key=lambda s: s.area()))
