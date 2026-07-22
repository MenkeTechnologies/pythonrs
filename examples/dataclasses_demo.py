"""Dataclasses: defaults, ordering, frozen, and the generated dunders."""

from dataclasses import dataclass, field, asdict, replace


@dataclass(order=True)
class Point:
    x: int
    y: int = 0
    tags: list = field(default_factory=list)


@dataclass(frozen=True)
class RGB:
    r: int
    g: int
    b: int

    def hex(self):
        return f"#{self.r:02x}{self.g:02x}{self.b:02x}"


p = Point(3, 4)
print(p)
print("equal:", p == Point(3, 4))
print("ordered:", Point(1, 2) < Point(1, 3))
print("asdict:", asdict(p))
print("replace:", replace(p, x=10))
print("sorted:", sorted([Point(3), Point(1), Point(2)]))

color = RGB(255, 128, 0)
print(color, "->", color.hex())
try:
    color.r = 0
except Exception as e:
    print("frozen:", type(e).__name__)
