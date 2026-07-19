from dataclasses import dataclass, field


@dataclass
class Point:
    x: int
    y: int = 0
    tags: list = field(default_factory=list)

    def dist2(self):
        return self.x * self.x + self.y * self.y


p = Point(3, 4)
print("repr :", p)
print("dist2:", p.dist2())
print("eq   :", Point(1, 1) == Point(1, 1))
