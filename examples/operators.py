"""Operator overloading: a small immutable Vector with the full dunder set."""

import math
from functools import total_ordering


@total_ordering
class Vector:
    def __init__(self, *components):
        self.components = tuple(components)

    def __add__(self, other):
        return Vector(*(a + b for a, b in zip(self.components, other.components)))

    def __sub__(self, other):
        return Vector(*(a - b for a, b in zip(self.components, other.components)))

    def __mul__(self, scalar):
        return Vector(*(a * scalar for a in self.components))

    __rmul__ = __mul__

    def __neg__(self):
        return self * -1

    def __abs__(self):
        return math.sqrt(sum(a * a for a in self.components))

    def dot(self, other):
        return sum(a * b for a, b in zip(self.components, other.components))

    def __eq__(self, other):
        return self.components == other.components

    def __lt__(self, other):
        return abs(self) < abs(other)

    def __len__(self):
        return len(self.components)

    def __getitem__(self, i):
        return self.components[i]

    def __iter__(self):
        return iter(self.components)

    def __repr__(self):
        return f"Vector{self.components}"


u = Vector(1, 2, 3)
v = Vector(4, 5, 6)

print("u + v =", u + v)
print("u - v =", u - v)
print("u * 2 =", u * 2)
print("3 * u =", 3 * u)
print("-u    =", -u)
print("|u|   =", round(abs(u), 4))
print("u . v =", u.dot(v))
print("u[1]  =", u[1], "| len:", len(u))
print("as list:", list(u))
print("u == Vector(1,2,3):", u == Vector(1, 2, 3))
print("sorted by magnitude:", sorted([v, u, Vector(0, 0, 1)]))
