"""The numeric and binary types past `int` and `float`.

Every set is printed sorted: a set's repr order is not part of the language, and
this corpus is compared byte for byte.
"""

# `divmod` agrees with `//` and `%`, and the sign follows the DIVISOR — which is
# where Python parts company with C and with most of its own contemporaries.
for a, b in ((7, 2), (-7, 2), (7, -2), (-7, -2)):
    print(a, b, divmod(a, b), a // b, a % b)
print(divmod(7.5, 2), divmod(-7.5, 2))

# `round` is half-to-EVEN, not half-up, and a negative ndigits rounds left of
# the point. `round(2.675, 2)` is 2.67 because 2.675 is not exactly 2.675.
print([round(x) for x in (0.5, 1.5, 2.5, 3.5, -0.5, -1.5)])
print(round(2.675, 2), round(1.005, 2), round(1234, -2), round(1250, -2))
print(type(round(1.5)).__name__, type(round(1.5, 0)).__name__)

# Integers are arbitrary precision; floats are not, and the boundary shows.
big = 2**70
print(big, big + 1, len(str(2**200)))
print(0.1 + 0.2 == 0.3, abs(0.1 + 0.2 - 0.3) < 1e-15)
print((2**53 + 1) == float(2**53 + 1))

# True division always makes a float; floor division of ints stays int.
print(7 / 2, type(7 / 2).__name__, 7 // 2, type(7 // 2).__name__, 7.0 // 2)

# complex
z = complex(3, 4)
print(z, z.real, z.imag, abs(z), z.conjugate())
print(z + complex(1, -2), z * 2, (1j) ** 2)

# bytes are immutable sequences of ints; bytearray is the mutable form.
b = b"ab\x00c"
print(len(b), b[0], b[1:3], b.hex(), list(b))
print(bytes([72, 105]), b"a" + b"b", b"ab" * 2)
ba = bytearray(b"abc")
ba[0] = 90
ba.append(33)
print(ba, bytes(ba))
print("héllo".encode("utf-8"), b"h\xc3\xa9llo".decode("utf-8"))
print(len("héllo"), len("héllo".encode("utf-8")))

# frozenset is the hashable set, so it can be a dict key or a set member.
fs = frozenset([3, 1, 2])
print(sorted(fs), len(frozenset("aabbc")))
print(sorted(fs | frozenset([4])), sorted(fs & frozenset([2, 3, 9])), sorted(fs - {1}))
print(fs.issubset({1, 2, 3, 4}), fs.isdisjoint({9}))
seen = {frozenset([1, 2]): "pair"}
print(seen[frozenset([2, 1])])
print(sorted({frozenset([1]), frozenset([1]), frozenset([2])}, key=sorted))

# bool is a subclass of int, which is visible in arithmetic and in indexing.
print(True + True, isinstance(True, int), [10, 20][True])
print(int(True), float(False), bool(0.0), bool(""), bool([0]))
