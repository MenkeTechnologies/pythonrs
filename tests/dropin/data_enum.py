from enum import Enum


class Color(Enum):
    RED = 1
    GREEN = 2
    BLUE = 3


print("name :", Color.GREEN.name)
print("value:", Color.GREEN.value)
print("byval:", Color(3).name)
print("iter :", [c.name for c in Color])
