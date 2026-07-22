"""Enums: plain, IntEnum ordering, Flag combinations, and auto()."""

from enum import Enum, IntEnum, Flag, auto


class Color(Enum):
    RED = 1
    GREEN = 2
    BLUE = 3


class Priority(IntEnum):
    LOW = 1
    MEDIUM = 5
    HIGH = 10


class Permission(Flag):
    READ = auto()
    WRITE = auto()
    EXECUTE = auto()


print("members:", [c.name for c in Color])
print("by value:", Color(2), "by name:", Color["BLUE"])
print("identity:", Color.RED is Color.RED)

print("int math:", int(Priority.HIGH) + 1, Priority.HIGH > Priority.LOW)
print("sorted:", sorted(Priority, key=lambda p: p.value))

rw = Permission.READ | Permission.WRITE
print("combined:", rw)
print("has READ:", Permission.READ in rw)
print("has EXECUTE:", Permission.EXECUTE in rw)
