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
             "1<'a'", "int('12',1)", "dict(1)",
             "2.0**10000", "float(2**2000)", "(2**2000)*1.0", "(2**2000)/1",
             "range(1.5)", "range('a')", "[1,2][0.5]", "abs('x')",
             "'{'.format()", "'%d' % 'x'", "min([])",
             "chr(-1)", "ord('ab')", "'a'.encode('nope')", "b'\\xff'.decode()"]:
    try:
        eval(code)
        print(code, "-> NO-RAISE")
    except BaseException as e:
        print(code, "->", type(e).__name__, ":", e)
# CPython REWORDED these three, so their MESSAGE is not version-stable, and
# this corpus is compared against any reference from 3.9 on (tests/parity.rs).
# 3.12 says "integer modulo by zero", "integer division or modulo by zero" and
# "1 is not in list"; 3.14 unified the first two to the bare "division by zero",
# which is the wording pythonrs targets — pinned by tests/lang.rs
# zero_division_messages_match_314. The exception TYPE is stable in every
# release, so that is what these three compare.
for code in ["1%0", "divmod(1,0)", "[].index(1)"]:
    try:
        eval(code)
        print(code, "-> NO-RAISE")
    except BaseException as e:
        print(code, "->", type(e).__name__)
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
#==#
# ── dir(): every name it lists must be one getattr can produce ──────────────
# The invariant, not the list — the list itself moved between releases
# (`__getstate__` in 3.11, `__buffer__` in 3.12), but "listed implies reachable"
# has held since 3.9 and is exactly what was broken: `dir()` enumerated each
# builtin type's own methods and five inherited names, so it was 24 names short
# on `str` and 26 on `list`.
VALUES = ["'a'", "b'a'", "bytearray(b'a')", "5", "1.5", "True", "1j", "[1]", "(1,)",
          "{1: 2}", "{1}", "frozenset([1])", "range(3)", "slice(1, 2)", "None",
          "...", "NotImplemented", "object"]
for src in VALUES:
    x = eval(src)
    names = dir(x)
    print(src, len(names) > 20, names == sorted(names), sorted(set(names)) == names,
          [n for n in names if not hasattr(x, n)])
#==#
# ── the object surface every value inherits, reached as a bound method ──────
print((5).__eq__(5), (5).__eq__(1.5), (1.5).__eq__(5), (5).__eq__("a"), "a".__eq__(5))
print([].__eq__(()), {1}.__eq__(frozenset([1])), b"a".__eq__(bytearray(b"a")))
print(None.__eq__(None), None.__eq__(1), (...).__eq__(...), (5).__ne__(2), [].__ne__(5))
print((5).__lt__(2), (5).__lt__(1.5), (1.5).__lt__(5), "a".__lt__("b"), [].__lt__(5))
print((5).__hash__() == hash(5), "a".__hash__() == hash("a"), [].__hash__, {}.__hash__)
print((5).__str__(), (5).__repr__(), "a".__str__(), [1].__repr__(), (5).__format__("x"))
print([].__getstate__(), (5).__init__(), [].__init_subclass__(), "a".__subclasshook__(int))
print(type((5).__sizeof__()).__name__, type([].__sizeof__()).__name__)
print(sorted("a".__dir__()) == dir("a"), (5).__getattribute__("real"))
#==#
# ── the attributes dir() was hiding while getattr answered them ─────────────
print("a".isascii(), "é".isascii(), (5).real, (5).imag, (5).numerator, (5).denominator)
print(range(1, 7, 2).start, range(1, 7, 2).stop, range(1, 7, 2).step, range(5).start, range(5).step)
print(slice(1, 7, 2).start, slice(1, 7, 2).stop, slice(1, 7, 2).step, slice(3).start)
print((5).from_bytes(b"\x01\x02"), int.from_bytes(b"\x01\x02"), (1.5).fromhex("0x1.8p+0"))
print("ab".__getnewargs__(), (5).__getnewargs__(), True.__getnewargs__(), (1.5).__getnewargs__())
print((1, 2).__getnewargs__(), b"ab".__getnewargs__(), (1j).__getnewargs__())
print(list([1, 2].__reversed__()), list({1: 2, 3: 4}.__reversed__()), list(range(3).__reversed__()))
print(list({1: 2}.keys().__reversed__()), list({1: 2}.items().__reversed__()))
print([].__class_getitem__(int), dict.__class_getitem__(int), tuple.__class_getitem__(int))
print((1j).__neg__(), (1j).__pos__(), (1j).__complex__(), b"ab".__bytes__())
print(bytearray(b"ab").copy(), (1.5).__getformat__("double"), float.__getformat__("float"))
#==#
# ── a missing attribute must stay missing, with the type's own hint ─────────
# Each of these is a name that IS on some other builtin type. Granting the
# object surface must not have leaked any of them onto a type CPython does not
# put them on.
ABSENT = [("'a'", "__class_getitem__"), ("[1]", "__getnewargs__"), ("{1}", "__reversed__"),
          ("(1,)", "__setitem__"), ("5", "__len__"), ("[1]", "__bool__"),
          ("'a'", "__iadd__"), ("b'a'", "__alloc__"), ("5", "from_number"),
          ("[1]", "resize"), ("'a'", "__buffer__"), ("(1,)", "append")]
for src, attr in ABSENT:
    print(src, attr, hasattr(eval(src), attr))
#==#
# ── the object surface on a user class, and the type surface on a class ─────
class A:
    x = 1
class B(A):
    pass
a, b = A(), A()
print(a.__eq__(a), a.__eq__(b), a.__ne__(b), a.__lt__(b), a.__hash__() == hash(a))
print(a.__init__(), a.__getstate__(), A.__subclasshook__(int), A.__init_subclass__())
print(a.__setattr__("z", 1), a.z, a.__getattribute__("z"), a.__delattr__("z"), hasattr(a, "z"))
print(a.__format__("") == str(a), a.__repr__() == repr(a), a.__str__() == str(a))
print(type(a.__sizeof__()).__name__, sorted(a.__dir__()) == dir(a))
print(A.mro(), B.mro(), A.__base__, B.__base__, int.__base__, object.__base__)
print(A.__instancecheck__(a), A.__instancecheck__(B()), B.__instancecheck__(a))
print(A.__subclasscheck__(B), B.__subclasscheck__(A), int.__subclasscheck__(bool))
print(A.__prepare__(), A.__type_params__, A.__text_signature__, int.__type_params__)
print(A | int, int | A, list[A], type(A.__call__()).__name__, A.__subclasses__())
print(sorted(dir(object())), len(dir(object())) == len(dir(object)))
#==#
# ── an unbound descriptor type-checks its receiver ─────────────────────────
# Reached through the TYPE object, a method is CPython's unbound descriptor and
# rejects a receiver of the wrong type before running — with two distinct
# wordings, one for a C slot and one for a method-table entry. pythonrs answered
# `AttributeError: 'int' object has no attribute 'lower'`, the wrong exception
# type entirely, so `except TypeError` around descriptor plumbing missed it.
CALLS = ["str.lower(5)", "str.upper('a')", "list.append(5, 1)", "dict.get(5, 1)",
         "int.bit_length('a')", "int.bit_length(True)", "str.lower(True)",
         "str.join(5, [])", "str.encode(b'a')", "bytes.decode('a')",
         "float.is_integer(5)", "tuple.count((1, 2), 1)", "list.__len__([1, 2])",
         "str.__len__(5)", "str.__contains__(5, 'a')", "list.__getitem__(5, 0)",
         "dict.__contains__(5, 'a')", "set.__contains__(5, 1)", "str.__eq__(5, 'a')",
         "str.__add__(5, 'a')", "list.__init__(5)", "int.__or__(str)",
         "object.__repr__(5)[:12]", "object.__str__('a')"]
for src in CALLS:
    try:
        print(src, "->", repr(eval(src)))
    except BaseException as e:
        print(src, "->", type(e).__name__, e)
#==#
# ── the exception state is left behind on EVERY exit from a handler ─────────
# Returning out of an `except` block skipped the restore, so the handled
# exception stayed installed as "currently being handled" for the rest of the
# program and the next unrelated `raise` anywhere picked it up as its implicit
# `__context__` — printing a spurious "During handling of the above exception".
def ret_from_handler():
    try:
        raise KeyError("k")
    except KeyError:
        return "returned"
print(ret_from_handler())
try:
    raise ValueError("v")
except ValueError as e:
    print(repr(e.__context__), e.__suppress_context__)
def ret_with_finally():
    try:
        raise IndexError("i")
    except IndexError:
        return 1
    finally:
        pass
ret_with_finally()
try:
    raise TypeError("t")
except TypeError as e:
    print(repr(e.__context__))
# the context that SHOULD be there still is
try:
    try:
        raise KeyError("inner")
    except KeyError:
        raise ValueError("during")
except ValueError as e:
    print(repr(e.__context__), repr(e.__cause__))
#==#
# ── range over the whole word, and cyclic containers ───────────────────────
import sys
M = sys.maxsize
print(len(range(M)), len(range(-M, M, 2)), len(range(M, M)), len(range(0)))
print(range(-M - 1, M), range(-M - 1, M)[0], range(M)[M - 1], bool(range(M)))
for src in ["len(range(-M - 1, M))", "len(range(M, -M - 1, -1))"]:
    try:
        print(src, "->", eval(src))
    except BaseException as e:
        print(src, "->", type(e).__name__, e)
# A container that contains itself: the identity shortcut in element comparison
# is what makes this terminate at all.
a = [1, 2]
a.append(a)
b = [1, 2]
b.append(b)
print(a, a == a, len(a))
d = {}
d["self"] = d
print(d, d == d)
t = ([],)
t[0].append(t)
print(t)
# Two DIFFERENT cycles have no shortcut, and recursing the whole way used to
# abort the process outright. Only the exception TYPE is compared: 3.14 words it
# with a machine-dependent stack size.
for src in ["a == b", "d == {'self': {'self': 1}}"]:
    try:
        print(src, "->", eval(src))
    except BaseException as e:
        print(src, "->", type(e).__name__)
#==#
# ── property is a descriptor, and says so ──────────────────────────────────
# `C.x.fget` and `C.x.__get__(obj)` were AttributeErrors, so any code driving a
# property through the descriptor protocol by hand — which is how `abc` wraps an
# abstract property and how `inspect` tells a data descriptor from a plain one —
# could not touch one.
def get_v(self):
    return "bare"
class C:
    def __init__(self):
        self._x = 0
    @property
    def x(self):
        return self._x
    @x.setter
    def x(self, v):
        self._x = v * 2
    y = property(get_v)
p, c = C.x, C()
# `property.__name__` is 3.13+, and this corpus is compared against any
# reference from 3.9 on, so it is pinned in tests/lang.rs
# (property_name_is_the_attribute_it_was_bound_to) instead of here.
print(p.fget.__name__, p.fset.__name__, p.fdel)
print(p.__isabstractmethod__, p.__get__(None, C) is p)
print(p.__get__(c), p.__set__(c, 5), p.__get__(c), c._x)
try:
    p.__delete__(c)
except AttributeError as e:
    print("AttributeError:", e)
print([n for n in ["fget", "fset", "fdel", "__get__", "__set__", "__delete__",
                   "__set_name__", "__isabstractmethod__",
                   "getter", "setter", "deleter"] if n not in dir(p)])
#==#
# ── a generator knows its own name and whether it is parked ────────────────
def gen():
    yield 1
    yield 2
g = gen()
print(g.__name__, g.__qualname__, g.gi_running, g.gi_suspended)
next(g)
print(g.gi_running, g.gi_suspended)
list(g)
print(g.gi_running, g.gi_suspended)
def selfaware():
    yield (me.gi_running, me.gi_suspended)
me = selfaware()
print(next(me))
#==#
# ── iterator protocol: what __iter__ must return, and where it is checked ────
# `iter()` validated its result but `list(obj)` and `for x in obj` did not, so
# the same broken class reported CPython's message from one spelling and a bare
# "not an iterator" from the other two.
class BadIter:
    def __iter__(self):
        return 42
class Seq:
    def __init__(self, n):
        self.n = n
    def __getitem__(self, i):
        if i >= self.n:
            raise IndexError(i)
        return i * i
class Counter3:
    def __init__(self):
        self.i = 0
    def __iter__(self):
        return self
    def __next__(self):
        self.i += 1
        if self.i > 3:
            raise StopIteration
        return self.i
for label in ["list(BadIter())", "[x for x in BadIter()]", "iter(BadIter())",
              "tuple(BadIter())", "sum(BadIter())", "sorted(BadIter())"]:
    try:
        eval(label)
        print(label, "-> NO-RAISE")
    except TypeError as e:
        print(label, "->", e)
print(list(Seq(4)), [x for x in Seq(3)], sum(Seq(4)), max(Seq(4)))
print(list(Counter3()), sum(Counter3()), list(zip(Counter3(), "abcd")))
it = iter(Counter3())
print(next(it), list(it), next(it, "exhausted"))
print(iter(Counter3()) is not None, iter([1]) is not iter([1]))
#==#
# ── generators: send / throw / close, yield from, and the return value ──────
def echo():
    got = yield "first"
    got2 = yield f"got:{got}"
    return f"done:{got2}"
g = echo()
print(next(g), g.send("A"))
try:
    g.send("B")
except StopIteration as e:
    print("StopIteration value", e.value, e.args)
def inner():
    yield 1
    yield 2
    return "INNER"
def outer():
    got = yield from inner()
    yield f"after:{got}"
print(list(outer()))
def with_finally():
    try:
        yield 1
        yield 2
    finally:
        print("generator finalized")
gf = with_finally()
print(next(gf))
gf.close()
def catcher():
    try:
        yield "ready"
    except ValueError as e:
        yield f"caught:{e}"
c = catcher()
print(next(c), c.throw(ValueError("thrown")))
gexp = (n * n for n in range(4))
print(list(gexp), list(gexp))
#==#
# ── context managers: __exit__ suppression, nesting, parenthesized with ──────
class CM:
    def __init__(self, name, suppress=False):
        self.name = name
        self.suppress = suppress
    def __enter__(self):
        print("enter", self.name)
        return self.name
    def __exit__(self, et, ev, tb):
        print("exit", self.name, et.__name__ if et else None, ev)
        return self.suppress
with CM("plain") as v:
    print("body", v)
with CM("swallow", True):
    raise ValueError("suppressed by __exit__")
print("still running")
try:
    with CM("propagate"):
        raise KeyError("kept")
except KeyError as e:
    print("propagated", e)
with CM("x") as x, CM("y") as y:
    print("two", x, y)
with (CM("p") as p, CM("q") as q):
    print("parenthesized", p, q)
