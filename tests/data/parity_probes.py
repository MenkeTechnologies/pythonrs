# Differential parity probes: each block between `#==#` markers is a complete
# program run under BOTH the reference `python3` and the built `python`, whose
# stdout, stderr and exit code must agree byte for byte. See tests/parity.rs.
#
# Every probe must be DETERMINISTIC for reasons unrelated to parity: no clock,
# no `random`, no `id()`/address in any repr, no bare `set` iteration order, no
# environment or filesystem reads. A probe that is merely flaky reports a
# divergence that is not one, and the harness cannot tell the difference.
#
# Probes must also be VERSION-STABLE back to CPython 3.9 on stdout, because the
# reference is whatever `python3` the machine has. Anything whose wording moved
# between releases belongs in the stderr-gated section the harness only compares
# against a new enough reference.

# ── float repr: shortest round-trip, boundaries, signed zero ─────────────────
print(repr(0.1), repr(1e16), repr(1e17), repr(1e-5), repr(1e-4))
print(repr(5e-324), repr(1.7976931348623157e308), repr(-0.0), 0.0 == -0.0)
print(repr(float('inf')), repr(float('-inf')), repr(float('nan')))
print(str(1e100), repr(1e100), repr(1.0), str(1.0), repr(100.0), repr(1e15))
print(repr(3.14), repr(2.675), repr(0.1 + 0.2), repr(1 / 3), repr(2 / 3))
print(repr(2.0 ** 1023), repr(1e-300 ** 2), repr(2.0 ** -10000))
#==#
# ── integer // and % sign rules, divmod, bignum ──────────────────────────────
print(7 // 2, -7 // 2, 7 // -2, -7 // -2, 7 % 2, -7 % 2, 7 % -2, -7 % -2)
print(7.0 // 2.0, -7.0 // 2.0, 7.0 % 2.0, -7.0 % 2.0)
print(divmod(-7, 2), divmod(7, -2), divmod(7.5, 2), divmod(10 ** 30, 7))
print(10 ** 20, (10 ** 20) // 7, (10 ** 20) % 7, -(10 ** 20) // 7)
print(2 ** 100, (2 ** 100).bit_length(), (-5).bit_length(), (0).bit_length())
print((-2) ** 3, (-2) ** 2, 2 ** 0.5, 0 ** 0, 0.0 ** 0, 2 ** -1)
#==#
# ── int/float conversion at the f64 boundary (exactness, not rounding) ───────
print((2 ** 2000) / (2 ** 1999), (2 ** 2000) / (2 ** 2000), (3 ** 200) / (3 ** 199))
print((10 ** 20) / 3, (2 ** 53 + 1) / 3, (7 ** 300) / (7 ** 150))
print(1 / (2 ** 2000), 1 / (2 ** 1074), 1 / (2 ** 1075), 0 / -1)
print(float(2 ** 1023), float(2 ** 53 + 1), float(2 ** 53 + 3), float(3 ** 100))
print((2 ** 2000) > 1.0, (2 ** 2000) == 1.0, 3 ** 34 == float(3 ** 34))
print((-1.0) ** float('inf'), (-2.0) ** float('-inf'), float('inf') ** 2)
#==#
# ── f-strings and the format mini-language ──────────────────────────────────
print(f"{3.14159:.2f}", f"{42:5d}", f"{42:<5}|", f"{42:>5}|", f"{42:^5}|", f"{42:05d}")
print(f"{255:x}", f"{255:X}", f"{255:o}", f"{255:b}", f"{255:#x}", f"{-255:#x}")
print(f"{1234567:,}", f"{1234567:_}", f"{1234.5678:,.2f}", f"{0.5:.0%}")
print(f"{1.5:e}", f"{1.5:E}", f"{0:g}", f"{1e20:g}", f"{1e-5:g}", f"{1234567:g}")
print(f"{'ab':*^8}", f"{'ab':*<8}", f"{'ab':*>8}", f"{'abc':.2}", f"{None}", f"{[1, 2]}")
print(format(3.14159, ".3f"), format(255, "x"), format(True), format(1.0), format(-0.0))
print("{0:{1}}".format(3.14159, ".2f"), "{!r}".format("a"), "{{}}".format())
#==#
# ── %-formatting ────────────────────────────────────────────────────────────
print("%d %s %r" % (1, "a", "a"), "%5.2f" % 3.14159, "%x" % 255, "%-5d|" % 3)
print("%+d" % 3, "%s" % (1,), "%(a)s" % {"a": 1}, "%c" % 65, "%%")
print("%e" % 1234.5, "%g" % 0.0001, "%o" % 8, "%X" % 255, "%.3s" % "abcdef")
print("%*d" % (5, 42), "%05.1f" % 3.14159, "%r" % (1.5,), "%s" % [1, 2])
#==#
# ── slicing with negative and None bounds ───────────────────────────────────
print([1, 2, 3][::-1], [1, 2, 3, 4, 5][1:4], [1, 2, 3, 4, 5][-2:], [1, 2, 3, 4, 5][:-2])
print([1, 2, 3, 4, 5][::2], [1, 2, 3, 4, 5][::-2], [1, 2, 3, 4, 5][None:None])
print([1, 2, 3][5:], [1, 2, 3][-10:], "abcdef"[::-1], "abcdef"[1:-1], "abc"[10:20])
a = [0, 1, 2, 3, 4, 5]
a[1:3] = [9]
print(a)
del a[::2]
print(a, a[::-1][:2])
print(list(range(10))[slice(2, 8, 3)], slice(1, 2, 3).indices(10))
#==#
# ── range semantics ─────────────────────────────────────────────────────────
print(list(range(5)), list(range(5, 0, -1)), list(range(0)), list(range(0, 10, 3)))
print(len(range(10 ** 6)), range(5) == range(5), range(0, 5, 2) == range(0, 6, 2))
print(range(5)[2], list(range(10)[2:5]), 3 in range(0, 10, 2), 4 in range(0, 10, 2))
print(repr(range(3)), repr(range(1, 3)), repr(range(1, 9, 2)), list(reversed(range(3))))
print(range(2 ** 70)[5], list(range(-5, -1)), list(range(5, 5)))
#==#
# ── dict/set ordering and iteration ─────────────────────────────────────────
d = {'b': 1, 'a': 2, 'c': 3}
print(d, list(d), list(d.items()), list(reversed(d)))
d2 = {'a': 1}
d2['b'] = 2
del d2['a']
d2['a'] = 3
print(d2, list(d2.keys()), list(d2.values()))
print({**{'x': 1}, **{'y': 2}}, dict(zip("ab", [1, 2])), dict([('k', 'v')]))
print(sorted({1, 2, 3} | {3, 4}), sorted({1, 2, 3} & {2, 3}), sorted({1, 2} ^ {2, 3}))
print(len({1, 1.0, True}), {1: 'a', 1.0: 'b', True: 'c'}, sorted(set("hello")))
print(d.get('z'), d.get('z', 0), d.pop('a'), d, {}.setdefault('k', []))
#==#
# ── sorted / list.sort: stability and key ───────────────────────────────────
print(sorted([3, 1, 2]), sorted("cba"), sorted([1, 2, 3], reverse=True))
print(sorted([(1, 'b'), (1, 'a'), (0, 'z')]), sorted(['bb', 'a', 'ccc'], key=len))
print(sorted([-3, 1, -2], key=abs), sorted([1, 2, 3, 4], key=lambda n: n % 2))
# Stability: equal keys keep their input order, and only a stable sort proves it.
pairs = [('a', 1), ('b', 0), ('c', 1), ('d', 0), ('e', 1)]
print(sorted(pairs, key=lambda p: p[1]))
x = [3, 1, 2]
x.sort()
print(x)
x.sort(key=lambda n: -n)
print(x, x.index(2), x.count(3))
print(min([3, 1, 2]), max([3, 1, 2]), min("cab"), max([1, 2], key=lambda n: -n))
#==#
# ── string methods ──────────────────────────────────────────────────────────
print("a b  c".split(), " a b ".split(), "a,b,,c".split(","), "a,b,c".split(",", 1))
print("".split(","), "a b c".rsplit(" ", 1), "  ".split(), "x".split("x"))
print("abc".partition("b"), "abc".rpartition("b"), "abc".partition("z"))
print("ab".center(6) + "|", "ab".center(7, "-"), "5".zfill(3), "-5".zfill(3), "+5".zfill(3))
print("abc".zfill(2), "Hello".upper(), "hello world".title(), "Hello".swapcase())
print("  x  ".strip() + "|", "xxaxx".strip("x"), "abc".removeprefix("a"), "abc".removesuffix("c"))
print("aaa".replace("a", "b", 2), "abc".find("z"), "abc".index("b"), "abc".count("a"))
print("abc".startswith(("x", "a")), "abc".endswith("c"), "-".join(["a", "b"]))
print("a\tb".expandtabs(4), "Ab1".isalnum(), "".isalpha(), "12".isdigit(), "a".ljust(3) + "|")
print("line1\nline2".splitlines(), "line1\nline2".splitlines(True))
#==#
# ── truthiness, bool arithmetic, chained comparison ─────────────────────────
print(bool(0), bool(""), bool([]), bool({}), bool(None), bool(0.0), bool(set()), bool((0,)))
print(True + True, True * 3, sum([True, False, True]), int(True), True == 1, [1, 2][True])
print(1 < 2 < 3, 3 > 2 > 1, 1 < 2 > 0, 1 == 1 != 2, (1 < 2) < 3, 1 < 1 < 1 / 0 if False else "skip")
print(any([]), all([]), any([0, 1]), all([1, 0]), None is None, [] == [])
print(0 or "x", 1 and "y", "" or 0, not 0, not [], 1 if [] else 2)
#==#
# ── augmented assignment on containers, repetition, concatenation ───────────
x = [1, 2]
x += [3]
print(x)
y = (1,)
y += (2,)
print(y)
s = {1}
s |= {2}
print(sorted(s))
n = 5
n //= 2
n **= 3
n %= 5
print(n)
print([1, 2] * 2, 2 * [1, 2], (1,) * 3, "ab" * 2, [0] * 0, [[0]] * 2, [1] + [2])
lst = [[0]] * 2
lst[0].append(1)
print(lst)
#==#
# ── comprehension scoping ───────────────────────────────────────────────────
x = 10
def f():
    return [x for x in range(3)]
print(f(), x)
print([y * z for y in range(3) for z in range(2)])
print({k: v for k, v in [("a", 1), ("b", 2)]}, sorted({n % 3 for n in range(10)}))
print([n for n in range(5) if n % 2 == 0], list(n for n in range(3)))
print([[c for c in row] for row in ["ab", "cd"]])
total = 0
print([total := total + n for n in range(4)], total)
#==#
# ── exception types and message text ────────────────────────────────────────
for code in ["1/0", "[][0]", "{}['k']", "int('x')", "None.foo", "undefined_xyz",
             "'a'+1", "1+'a'", "len(1)", "[1,2].remove(9)", "(1,2)[5]",
             "'abc'.index('z')", "float('x')", "None()", "(1,2).__setitem__(0,3)",
             "(1).foo", "sorted([1,'a'])", "next(iter([]))", "[].pop()",
             "{}.pop('z')", "'a'*'b'", "'a,b'.split('')", "range(1,2,0)",
             "1<'a'", "1%0", "divmod(1,0)", "int('12',1)", "dict(1)",
             "2.0**10000", "float(2**2000)", "(2**2000)*1.0", "(2**2000)/1",
             "range(1.5)", "range('a')", "[1,2][0.5]", "abs('x')",
             "'{'.format()", "'%d' % 'x'", "[].index(1)", "min([])",
             "chr(-1)", "ord('ab')", "'a'.encode('nope')", "b'\\xff'.decode()"]:
    try:
        eval(code)
        print(code, "-> NO-RAISE")
    except BaseException as e:
        print(code, "->", type(e).__name__, ":", e)
#==#
# ── repr/str/ascii of every builtin container and scalar ────────────────────
print(repr("a'b"), repr('a"b'), repr("a\nb"), repr("a\tb"), repr("\\"), repr("\x00"))
print(repr("\x7f"), repr("é"), repr("\U0001F600"), len("\U0001F600"), ascii("é"))
print(str(b"ab"), repr(b"a\nb"), repr(bytearray(b"ab")), repr(b"\xff"), bytes([1, 2]))
print(chr(65), ord("A"), hex(255), oct(8), bin(5), hex(-255), abs(-2 ** 70))
print(repr([]), repr(()), repr((1,)), repr({}), repr(set()), repr({1}), repr([[]]))
print(repr(None), repr(True), repr(NotImplemented), repr(Ellipsis), repr(...))
print(str([1, "a", None, True]), str({'k': (1, [2])}), str(frozenset([1])))
#==#
# ── round / int / abs numeric coercions ─────────────────────────────────────
print(round(0.5), round(1.5), round(2.5), round(-0.5), round(-1.5), round(2.675, 2))
print(round(123.456, -1), round(2.5, 0), type(round(2.5, 0)).__name__, type(round(2.5)).__name__)
print(abs(-3), abs(-3.0), int(3.9), int(-3.9), int("0x1f", 16), int("  12  "))
print(float("1e3"), float("  .5 "), int(True), sum([1, 2, 3], 10), pow(2, 10, 7))
print(divmod(7.5, 2), 7.5 // 2, 7.5 % 2, -7.5 % 2, min(1, 2.0), type(min(1, 2.0)).__name__)
#==#
# ── iteration protocol and the lazy builtins ────────────────────────────────
print(list(enumerate("abc")), list(enumerate("abc", 1)), list(zip([1, 2], [3, 4, 5])))
print(list(map(str, [1, 2])), list(filter(None, [0, 1, 2, "", 3])), list(reversed("abc")))
it = iter([1, 2, 3])
print(next(it), next(it), list(it), next(it, "done"))
print(list(zip(*[[1, 2], [3, 4]])), tuple("ab"), list("ab"), set(), sorted(frozenset([2, 1])))
def gen():
    yield 1
    yield 2
    return 3
g = gen()
print(next(g), list(gen()), sum(gen()))
