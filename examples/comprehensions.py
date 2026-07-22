"""List, dict, set, and generator comprehensions — including nesting and walrus."""

# List comprehensions.
print([x * x for x in range(6)])
print([x for x in range(20) if x % 3 == 0])
print([x if x % 2 == 0 else -x for x in range(6)])

# Nested / flattening.
matrix = [[1, 2, 3], [4, 5, 6], [7, 8, 9]]
print([n for row in matrix for n in row])
print([[row[i] for row in matrix] for i in range(3)])  # transpose

# Dict comprehensions.
print({c: ord(c) for c in "abc"})
squares = {i: i * i for i in range(5)}
print(squares)
print({v: k for k, v in squares.items()})

# Set comprehension (sorted for deterministic output).
print(sorted({x % 5 for x in range(20)}))

# Generator expression consumed lazily.
total = sum(x * x for x in range(100) if x % 2)
print("sum of odd squares < 100:", total)

# Walrus in a comprehension keeps the last computed value.
data = [1, 2, 3, 4, 5, 6]
print([y for x in data if (y := x * x) > 10])

# Conditional dict build.
scores = {"alice": 85, "bob": 42, "carol": 91, "dan": 68}
print({name: s for name, s in scores.items() if s >= 70})
print(sorted(name for name, s in scores.items() if s < 70))
