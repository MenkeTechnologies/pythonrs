"""Functional style: map, filter, reduce, partial, and composition."""

from functools import reduce, partial

nums = list(range(1, 11))

# map / filter / reduce pipeline.
evens = list(filter(lambda x: x % 2 == 0, nums))
squared = list(map(lambda x: x * x, evens))
total = reduce(lambda a, b: a + b, squared, 0)
print("evens:", evens)
print("squared:", squared)
print("sum:", total)

# reduce building a value.
print("product 1..5:", reduce(lambda a, b: a * b, range(1, 6)))
print("max:", reduce(lambda a, b: a if a > b else b, [3, 1, 4, 1, 5, 9, 2]))

# partial application.
def power(base, exponent):
    return base**exponent


square = partial(power, exponent=2)
cube = partial(power, exponent=3)
print("squares:", [square(n) for n in range(1, 6)])
print("cubes:", [cube(n) for n in range(1, 6)])

# Function composition.
def compose(*funcs):
    def composed(x):
        for f in reversed(funcs):
            x = f(x)
        return x

    return composed


increment = lambda x: x + 1
double = lambda x: x * 2
pipeline = compose(increment, double, increment)  # ((x+1)*2)+1
print("compose(3):", pipeline(3))

# map over multiple iterables.
print("zipped sums:", list(map(lambda a, b: a + b, [1, 2, 3], [10, 20, 30])))

# A tiny pipeline helper, threading a value through transforms.
def thread(value, *transforms):
    for t in transforms:
        value = t(value)
    return value


print(
    "threaded:",
    thread(
        range(10),
        lambda xs: filter(lambda x: x % 2, xs),
        lambda xs: map(lambda x: x * 10, xs),
        list,
    ),
)
