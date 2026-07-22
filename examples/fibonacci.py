"""Fibonacci four ways: naive recursion, memoized, iterative, generator."""

import functools


def naive(n):
    return n if n < 2 else naive(n - 1) + naive(n - 2)


@functools.lru_cache(maxsize=None)
def memoized(n):
    return n if n < 2 else memoized(n - 1) + memoized(n - 2)


def iterative(n):
    a, b = 0, 1
    for _ in range(n):
        a, b = b, a + b
    return a


def stream():
    a, b = 0, 1
    while True:
        yield a
        a, b = b, a + b


print("naive:    ", [naive(i) for i in range(10)])
print("memoized: ", [memoized(i) for i in range(10)])
print("iterative:", [iterative(i) for i in range(10)])
print("generator:", list(__import__("itertools").islice(stream(), 10)))
print("fib(100): ", memoized(100))
print("cache:    ", memoized.cache_info().hits > 0)
