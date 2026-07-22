"""Type hints at runtime: annotations, typing generics, NamedTuple, cached_property."""

import functools
from typing import NamedTuple, Optional, List, Dict


def greet(name: str, times: int = 1) -> str:
    return " ".join([f"Hi {name}"] * times)


def summarize(items: List[int], label: Optional[str] = None) -> Dict[str, int]:
    return {"count": len(items), "total": sum(items)}


class Employee(NamedTuple):
    name: str
    salary: int
    department: str = "General"


class Circle:
    def __init__(self, radius: float):
        self.radius = radius

    @functools.cached_property
    def area(self) -> float:
        print(f"  (computing area for r={self.radius})")
        return round(3.14159 * self.radius**2, 4)


# Annotations are introspectable at runtime.
print("greet annotations:", greet.__annotations__)
print("summarize return:", summarize.__annotations__["return"])
print(greet("world", 2))
print(summarize([10, 20, 30], "scores"))

# NamedTuple with a default field.
e = Employee("Alice", 90000)
print("employee:", e)
print("fields:", e._fields)
print("as dict:", e._asdict())

# cached_property computes once, then caches.
c = Circle(5)
print("area:", c.area)
print("area again:", c.area)  # no recompute
