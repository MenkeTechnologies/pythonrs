"""Closures and lexical scope: factories, counters, nonlocal, and memoization."""


def make_multiplier(factor):
    def multiply(x):
        return x * factor

    return multiply


def make_counter(start=0):
    count = start

    def increment(step=1):
        nonlocal count
        count += step
        return count

    return increment


def make_accumulator():
    total = 0

    def add(value):
        nonlocal total
        total += value
        return total

    return add


double = make_multiplier(2)
triple = make_multiplier(3)
print("double(5):", double(5), "| triple(5):", triple(5))

counter = make_counter(10)
print("counter:", counter(), counter(), counter(5))

acc = make_accumulator()
print("accumulate:", [acc(v) for v in [10, 20, 30]])

# The classic late-binding trap, and the default-argument fix.
late = [lambda: i for i in range(3)]
early = [lambda i=i: i for i in range(3)]
print("late binding:", [f() for f in late])
print("early binding:", [f() for f in early])

# A memoizing closure built by hand.
def memoize(func):
    cache = {}

    def wrapper(n):
        if n not in cache:
            cache[n] = func(n)
        return cache[n]

    return wrapper


@memoize
def slow_square(n):
    return n * n


print("memoized:", [slow_square(x) for x in [4, 4, 5, 5, 6]])
