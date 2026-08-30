"""Attribute access: the protocols that run before and instead of a lookup.

`__get__`/`__set__` on a class attribute intercept access to it on every
instance; `__getattr__` runs only when the normal lookup has already failed;
`property` and the two method decorators are ordinary descriptors underneath.
"""


class Celsius:
    """A data descriptor: it defines __set__, so it wins over the instance dict."""

    def __init__(self, name):
        self.name = "_" + name

    def __get__(self, obj, objtype=None):
        if obj is None:
            return self
        return getattr(obj, self.name, 0.0)

    def __set__(self, obj, value):
        if value < -273.15:
            raise ValueError("below absolute zero")
        setattr(obj, self.name, float(value))


class Reading:
    temp = Celsius("temp")

    def __init__(self, temp):
        self.temp = temp


r = Reading(21.5)
print(r.temp, r._temp)
r.temp = -40
print(r.temp)
try:
    r.temp = -300
except ValueError as e:
    print("rejected:", e)

# Accessed on the CLASS, the descriptor returns itself.
print(type(Reading.temp).__name__)


class Shape:
    """`property`, `classmethod` and `staticmethod` on one class."""

    sides = 0

    def __init__(self, size):
        self._size = size

    @property
    def area(self):
        return self._size**2

    @area.setter
    def area(self, value):
        self._size = value**0.5

    @classmethod
    def named(cls):
        return cls.__name__.lower()

    @staticmethod
    def describe(n):
        return f"{n}-sided"


s = Shape(4)
print(s.area)
s.area = 100
print(s._size)
print(Shape.named(), s.named())
print(Shape.describe(3), s.describe(5))


class Square(Shape):
    sides = 4


# `classmethod` receives the SUBclass, which is what makes it an alternate
# constructor rather than a function on the base.
print(Square.named(), Square.sides)


class Lazy:
    """`__getattr__` runs only after the normal lookup fails."""

    def __init__(self):
        self.real = "present"
        self.calls = []

    def __getattr__(self, name):
        self.calls.append(name)
        return f"made:{name}"


lz = Lazy()
print(lz.real)
print(lz.missing, lz.other)
print(lz.calls)
print(hasattr(lz, "anything"), getattr(lz, "x", "default-unused"))


class Slotted:
    """`__slots__` removes the instance dict, so an unlisted name cannot be set."""

    __slots__ = ("a", "b")

    def __init__(self, a, b):
        self.a = a
        self.b = b


sl = Slotted(1, 2)
print(sl.a, sl.b, Slotted.__slots__)
try:
    sl.c = 3
except AttributeError as e:
    print("no slot:", type(e).__name__)
try:
    print(sl.__dict__)
except AttributeError:
    print("no __dict__")
