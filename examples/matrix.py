"""Matrices as lists of lists: transpose, multiply, and identity."""


def transpose(m):
    return [list(row) for row in zip(*m)]


def multiply(a, b):
    bt = transpose(b)
    return [[sum(x * y for x, y in zip(row, col)) for col in bt] for row in a]


def identity(n):
    return [[1 if i == j else 0 for j in range(n)] for i in range(n)]


def show(m, label):
    print(label)
    for row in m:
        print("  ", row)


a = [[1, 2, 3], [4, 5, 6]]
b = [[7, 8], [9, 10], [11, 12]]

show(a, "A =")
show(transpose(a), "A^T =")
show(multiply(a, b), "A * B =")
show(identity(3), "I3 =")

# Multiplying by identity is a no-op.
square = [[2, 0], [1, 3]]
print("A * I == A:", multiply(square, identity(2)) == square)

# Row/column sums.
print("row sums:", [sum(row) for row in a])
print("col sums:", [sum(col) for col in transpose(a)])
