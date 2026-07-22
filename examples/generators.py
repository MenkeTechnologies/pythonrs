"""Generators: lazy sequences, delegation with `yield from`, send/return."""

import itertools


def count_up(start=0, step=1):
    n = start
    while True:
        yield n
        n += step


def take(iterable, n):
    return list(itertools.islice(iterable, n))


def primes():
    seen = []
    for n in count_up(2):
        if all(n % p for p in seen):
            seen.append(n)
            yield n


def flatten(nested):
    for item in nested:
        if isinstance(item, list):
            yield from flatten(item)
        else:
            yield item


def running_total():
    total = 0
    while True:
        value = yield total
        total += value


print("naturals:", take(count_up(), 6))
print("evens:   ", take(count_up(0, 2), 6))
print("primes:  ", take(primes(), 8))
print("flattened:", list(flatten([1, [2, [3, 4], 5], [[6]], 7])))

acc = running_total()
next(acc)
print("running:", [acc.send(v) for v in [10, 20, 5, 100]])
