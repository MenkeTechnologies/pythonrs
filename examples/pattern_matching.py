"""Structural pattern matching (PEP 634): literals, sequences, mappings, classes."""

from dataclasses import dataclass


def describe(value):
    match value:
        case 0:
            return "zero"
        case int() if value < 0:
            return "negative int"
        case int():
            return "positive int"
        case [x]:
            return f"one-element list: {x}"
        case [x, y]:
            return f"pair: {x}, {y}"
        case [first, *rest]:
            return f"list starting {first}, then {rest}"
        case {"type": t, "value": v}:
            return f"tagged {t} = {v}"
        case str() as s:
            return f"string of length {len(s)}"
        case _:
            return "something else"


@dataclass
class Point:
    x: int
    y: int


def classify(point):
    match point:
        case Point(x=0, y=0):
            return "origin"
        case Point(x=0, y=y):
            return f"on y-axis at {y}"
        case Point(x=x, y=0):
            return f"on x-axis at {x}"
        case Point(x=x, y=y):
            return f"point ({x}, {y})"


samples = [0, -5, 42, [1], [1, 2], [1, 2, 3, 4], {"type": "id", "value": 7}, "hello"]
for s in samples:
    print(f"{str(s):24} -> {describe(s)}")

print()
for pt in [Point(0, 0), Point(0, 5), Point(3, 0), Point(2, 4)]:
    print(f"{pt} -> {classify(pt)}")
