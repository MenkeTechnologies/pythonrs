//! Headless language tests: run a Python snippet that binds a global, then read
//! that global's `repr` back from the host. No `python3` required, so these run
//! in CI. Each snippet exercises a distinct language feature end to end
//! (lex → parse → lower → fusevm execute), and the expected value is the value
//! CPython produces for the same program.

use pythonrs::{eval_str, host};

/// Run `src`, then return the `repr` of global `name`.
fn g(src: &str, name: &str) -> String {
    eval_str(src).expect("program should run without error");
    host::with_host(|h| {
        let v = h
            .read_global(name)
            .unwrap_or_else(|| panic!("global {name} unbound"));
        h.repr_of(&v)
    })
}

#[test]
fn arithmetic_and_precedence() {
    assert_eq!(g("x = 2 + 3 * 4 - 1", "x"), "13");
    assert_eq!(g("x = 7 // 2", "x"), "3");
    assert_eq!(g("x = 7 / 2", "x"), "3.5");
    assert_eq!(g("x = 2 ** 10", "x"), "1024");
    assert_eq!(g("x = 17 % 5", "x"), "2");
    assert_eq!(g("x = -3 + 2 * 4", "x"), "5");
}

#[test]
fn bignum_promotion() {
    assert_eq!(g("x = 2 ** 64", "x"), "18446744073709551616");
    assert_eq!(
        g("f = 1\nfor i in range(1, 26): f = f * i\nx = f", "x"),
        "15511210043330985984000000"
    );
}

#[test]
fn strings_and_fstrings() {
    assert_eq!(g("x = 'a' + 'b' * 3", "x"), "'abbb'");
    assert_eq!(g("x = 'Hello'.upper()", "x"), "'HELLO'");
    assert_eq!(g("x = ' hi '.strip()", "x"), "'hi'");
    assert_eq!(g("n = 42\nx = f'n={n} sq={n*n}'", "x"), "'n=42 sq=1764'");
    assert_eq!(g("x = f'{3.14159:.2f}'", "x"), "'3.14'");
    assert_eq!(g("x = ','.join(['a', 'b', 'c'])", "x"), "'a,b,c'");
    assert_eq!(g("x = 'a,b,c'.split(',')", "x"), "['a', 'b', 'c']");
}

#[test]
fn lists_dicts_sets_tuples() {
    assert_eq!(g("x = [1, 2, 3] + [4]", "x"), "[1, 2, 3, 4]");
    assert_eq!(g("a = [1, 2]\na.append(3)\nx = a", "x"), "[1, 2, 3]");
    assert_eq!(g("x = {'a': 1, 'b': 2}", "x"), "{'a': 1, 'b': 2}");
    assert_eq!(
        g("d = {'a': 1}\nd['b'] = 2\nx = d", "x"),
        "{'a': 1, 'b': 2}"
    );
    assert_eq!(g("x = sorted({3, 1, 2, 1})", "x"), "[1, 2, 3]");
    assert_eq!(g("x = (1, 2, 3)[1]", "x"), "2");
}

#[test]
fn slicing() {
    assert_eq!(g("x = list(range(10))[2:8:2]", "x"), "[2, 4, 6]");
    assert_eq!(g("x = [1, 2, 3, 4, 5][::-1]", "x"), "[5, 4, 3, 2, 1]");
    assert_eq!(g("x = 'python'[1:4]", "x"), "'yth'");
    assert_eq!(g("x = [1, 2, 3, 4][-2:]", "x"), "[3, 4]");
}

#[test]
fn comprehensions() {
    assert_eq!(g("x = [i * i for i in range(5)]", "x"), "[0, 1, 4, 9, 16]");
    assert_eq!(
        g("x = [i for i in range(10) if i % 2 == 0]", "x"),
        "[0, 2, 4, 6, 8]"
    );
    assert_eq!(
        g("x = {i: i * i for i in range(3)}", "x"),
        "{0: 0, 1: 1, 2: 4}"
    );
    assert_eq!(
        g("x = [y for row in [[1, 2], [3, 4]] for y in row]", "x"),
        "[1, 2, 3, 4]"
    );
}

#[test]
fn functions_defaults_varargs() {
    assert_eq!(g("def f(a, b=10):\n    return a + b\nx = f(5)", "x"), "15");
    assert_eq!(
        g(
            "def f(*args):\n    return sum(args)\nx = f(1, 2, 3, 4)",
            "x"
        ),
        "10"
    );
    assert_eq!(
        g("def f(a, **kw):\n    return kw['k']\nx = f(1, k=99)", "x"),
        "99"
    );
}

#[test]
fn closures() {
    assert_eq!(
        g(
            "def make(n):\n    def add(x):\n        return x + n\n    return add\nx = make(10)(5)",
            "x"
        ),
        "15"
    );
}

#[test]
fn classes_and_inheritance() {
    let src = "\
class A:
    def __init__(self, v):
        self.v = v
    def go(self):
        return self.v * 2
class B(A):
    def go(self):
        return self.v * 3
x = B(7).go()";
    assert_eq!(g(src, "x"), "21");
    assert_eq!(g("class A:\n    pass\nx = isinstance(A(), A)", "x"), "True");
}

#[test]
fn exceptions() {
    let src = "\
try:
    z = 1 / 0
    y = 'no'
except ZeroDivisionError:
    y = 'caught'
finally:
    w = 'done'
x = y + '/' + w";
    assert_eq!(g(src, "x"), "'caught/done'");
    assert_eq!(
        g(
            "try:\n    raise ValueError('boom')\nexcept ValueError as e:\n    x = str(e)",
            "x"
        ),
        "'boom'"
    );
}

#[test]
fn builtins_and_hof() {
    assert_eq!(
        g("x = list(map(lambda n: n * 2, [1, 2, 3]))", "x"),
        "[2, 4, 6]"
    );
    assert_eq!(
        g("x = list(filter(lambda n: n > 2, [1, 2, 3, 4]))", "x"),
        "[3, 4]"
    );
    assert_eq!(g("x = sorted([3, 1, 2], reverse=True)", "x"), "[3, 2, 1]");
    assert_eq!(g("x = max([1, 5, 3], key=lambda n: -n)", "x"), "1");
    assert_eq!(g("x = sum(range(101))", "x"), "5050");
    assert_eq!(
        g("x = list(enumerate(['a', 'b']))", "x"),
        "[(0, 'a'), (1, 'b')]"
    );
}

#[test]
fn control_flow() {
    assert_eq!(
        g(
            "x = 0\nfor i in range(5):\n    if i == 3:\n        break\n    x += i",
            "x"
        ),
        "3"
    );
    assert_eq!(
        g(
            "x = []\nfor i in range(5):\n    if i % 2:\n        continue\n    x.append(i)",
            "x"
        ),
        "[0, 2, 4]"
    );
    assert_eq!(g("x = 'yes' if 5 > 3 else 'no'", "x"), "'yes'");
}

#[test]
fn cache_roundtrip_is_transparent() {
    // Running the same source twice must produce the same value (2nd run served
    // from the rkyv cache).
    let src = "x = sum([i * i for i in range(10)])";
    assert_eq!(g(src, "x"), "285");
    assert_eq!(g(src, "x"), "285");
}

#[test]
fn operator_dunders() {
    // Arithmetic / comparison operator overloading via dunders on a user class.
    let src = "
class V:
    def __init__(self, x): self.x = x
    def __add__(self, o): return V(self.x + o.x)
    def __sub__(self, o): return V(self.x - o.x)
    def __mul__(self, k): return V(self.x * k)
    def __mod__(self, o): return V(self.x % o.x)
    def __eq__(self, o): return self.x == o.x
    def __lt__(self, o): return self.x < o.x
a = (V(2) + V(3)).x
b = (V(10) - V(4)).x
c = (V(5) * 4).x
d = (V(17) % V(5)).x
e = V(1) == V(1)
f = V(1) == V(2)
g_ = V(1) < V(2)
xs = [v.x for v in sorted([V(3), V(1), V(2)])]
";
    assert_eq!(g(src, "a"), "5");
    assert_eq!(g(src, "b"), "6");
    assert_eq!(g(src, "c"), "20");
    assert_eq!(g(src, "d"), "2");
    assert_eq!(g(src, "e"), "True");
    assert_eq!(g(src, "f"), "False");
    assert_eq!(g(src, "g_"), "True");
    assert_eq!(g(src, "xs"), "[1, 2, 3]");
}

#[test]
fn dunder_repr_in_containers() {
    // `str`/`repr` of a container must dispatch each element's `__repr__`.
    let src = "
class P:
    def __init__(self, n): self.n = n
    def __repr__(self): return f'P({self.n})'
lst = str([P(1), P(2)])
tup = str((P(3),))
dct = str({'k': P(4)})
";
    assert_eq!(g(src, "lst"), "'[P(1), P(2)]'");
    assert_eq!(g(src, "tup"), "'(P(3),)'");
    assert_eq!(g(src, "dct"), "\"{'k': P(4)}\"");
}

// ── generators / yield ────────────────────────────────────────────────────────

#[test]
fn generators_basic() {
    let src = "
def count(n):
    i = 0
    while i < n:
        yield i
        i += 1
whole = list(count(5))
first_two = [0, 0]
g2 = count(2)
first_two[0] = next(g2)
first_two[1] = next(g2)
total = sum(count(10))
loop = []
for v in count(3):
    loop.append(v)
";
    assert_eq!(g(src, "whole"), "[0, 1, 2, 3, 4]");
    assert_eq!(g(src, "first_two"), "[0, 1]");
    assert_eq!(g(src, "total"), "45");
    assert_eq!(g(src, "loop"), "[0, 1, 2]");
}

#[test]
fn generators_yield_expression_and_from() {
    // A `yield` expression receives the value passed to the caller's resume; a
    // plain iteration sends None (falsy), so the echo accumulates the yields.
    let src = "
def squares(xs):
    for x in xs:
        yield x * x
def chained():
    yield from range(3)
    yield from [7, 8]
sq = list(squares(range(4)))
ch = list(chained())
# lazy generator expression: type is generator, evaluated on demand
gx = (i * i for i in range(5))
tname = type(gx).__name__
vals = list(gx)
filtered = list(n for n in range(6) if n % 2 == 0)
";
    assert_eq!(g(src, "sq"), "[0, 1, 4, 9]");
    assert_eq!(g(src, "ch"), "[0, 1, 2, 7, 8]");
    assert_eq!(g(src, "tname"), "'generator'");
    assert_eq!(g(src, "vals"), "[0, 1, 4, 9, 16]");
    assert_eq!(g(src, "filtered"), "[0, 2, 4]");
}

#[test]
fn generator_is_lazy() {
    // A generator expression must NOT evaluate its body eagerly: only the two
    // elements actually consumed by `next` are produced (an eager list would
    // divide by zero on the 0 element).
    let src = "
seen = []
def tap(x):
    seen.append(x)
    return x
gen = (tap(i) for i in range(100))
one = next(gen)
two = next(gen)
consumed = list(seen)
";
    assert_eq!(g(src, "one"), "0");
    assert_eq!(g(src, "two"), "1");
    assert_eq!(g(src, "consumed"), "[0, 1]");
}

// ── call-site * / ** unpacking ────────────────────────────────────────────────

#[test]
fn call_arg_unpacking() {
    let src = "
def f(a, b, c):
    return (a, b, c)
lst = [10, 20, 30]
r1 = f(*lst)
r2 = f(*[1], *[2, 3])
r3 = f(1, *[2], 3)
def h(a, b, c, x=0, y=0):
    return (a, b, c, x, y)
r4 = h(*[1, 2], 3, **{'x': 9}, y=8)
def var(*args, **kwargs):
    return (args, sorted(kwargs.items()))
r5 = var(*[1, 2], 3, **{'k': 4}, z=5)
";
    assert_eq!(g(src, "r1"), "(10, 20, 30)");
    assert_eq!(g(src, "r2"), "(1, 2, 3)");
    assert_eq!(g(src, "r3"), "(1, 2, 3)");
    assert_eq!(g(src, "r4"), "(1, 2, 3, 9, 8)");
    assert_eq!(g(src, "r5"), "((1, 2, 3), [('k', 4), ('z', 5)])");
}

// ── literal spreads ──────────────────────────────────────────────────────────

#[test]
fn literal_spreads() {
    assert_eq!(g("x = [*[1, 2], 3, *[4, 5]]", "x"), "[1, 2, 3, 4, 5]");
    assert_eq!(g("x = (*[1, 2], 3)", "x"), "(1, 2, 3)");
    assert_eq!(g("x = sorted({*[1, 2], *[2, 3, 4]})", "x"), "[1, 2, 3, 4]");
    // ** dict spread with later keys overriding earlier ones, insertion order.
    assert_eq!(
        g("x = {**{'a': 1}, 'b': 2, **{'c': 3, 'a': 10}}", "x"),
        "{'a': 10, 'b': 2, 'c': 3}"
    );
    // None is a legal dict key and must not be confused with a ** spread slot.
    assert_eq!(g("x = {**{'a': 1}, None: 2}", "x"), "{'a': 1, None: 2}");
}

// ── match / case ──────────────────────────────────────────────────────────────

#[test]
fn match_literal_capture_wildcard_or_guard() {
    let src = "
def d(v):
    match v:
        case 0:
            return 'zero'
        case 1 | 2 | 3:
            return 'small'
        case int() if v > 100:
            return 'big'
        case str() as s:
            return 'str:' + s
        case _:
            return 'other'
a = d(0)
b = d(2)
c = d(200)
e = d('hi')
f = d(3.5)
";
    assert_eq!(g(src, "a"), "'zero'");
    assert_eq!(g(src, "b"), "'small'");
    assert_eq!(g(src, "c"), "'big'");
    assert_eq!(g(src, "e"), "'str:hi'");
    assert_eq!(g(src, "f"), "'other'");
}

#[test]
fn match_sequence_and_mapping() {
    let src = "
def d(v):
    match v:
        case [a, b]:
            return ('pair', a, b)
        case [a, *rest]:
            return ('head', a, rest)
        case {'name': n, 'age': age}:
            return ('person', n, age)
        case _:
            return ('other',)
p = d([10, 20])
h = d([1, 2, 3, 4])
m = d({'name': 'Al', 'age': 30})
rest_bind = None
match {'k': 1, 'a': 2, 'b': 3}:
    case {'k': v, **others}:
        rest_bind = (v, sorted(others.items()))
";
    assert_eq!(g(src, "p"), "('pair', 10, 20)");
    assert_eq!(g(src, "h"), "('head', 1, [2, 3, 4])");
    assert_eq!(g(src, "m"), "('person', 'Al', 30)");
    assert_eq!(g(src, "rest_bind"), "(1, [('a', 2), ('b', 3)])");
}

#[test]
fn match_class_patterns() {
    let src = "
class Point:
    __match_args__ = ('x', 'y')
    def __init__(self, x, y):
        self.x = x
        self.y = y
def loc(p):
    match p:
        case Point(0, 0):
            return 'origin'
        case Point(x=0, y=y):
            return ('y-axis', y)
        case Point(x, y):
            return ('point', x, y)
        case _:
            return '?'
a = loc(Point(0, 0))
b = loc(Point(0, 5))
c = loc(Point(3, 4))
";
    assert_eq!(g(src, "a"), "'origin'");
    assert_eq!(g(src, "b"), "('y-axis', 5)");
    assert_eq!(g(src, "c"), "('point', 3, 4)");
}

// ── nonlocal ──────────────────────────────────────────────────────────────────

#[test]
fn nonlocal_rebinds_enclosing_function_scope() {
    // `nonlocal` writes to the nearest enclosing FUNCTION scope, distinct from
    // `global` (which would touch module scope).
    let src = "
def counter():
    n = 0
    def inc():
        nonlocal n
        n += 1
        return n
    return inc
c = counter()
calls = [c(), c(), c()]
outer_x = 'g'
def outer():
    x = 'outer'
    def inner():
        nonlocal x
        x = 'changed'
    inner()
    return x
changed = outer()
still_global = outer_x
";
    assert_eq!(g(src, "calls"), "[1, 2, 3]");
    assert_eq!(g(src, "changed"), "'changed'");
    // The module-level name of the same spelling must be untouched.
    assert_eq!(g(src, "still_global"), "'g'");
}

#[test]
fn nonlocal_skips_to_deep_enclosing_scope() {
    let src = "
def deep():
    a = 1
    def mid():
        def inner():
            nonlocal a
            a = 99
        inner()
    mid()
    return a
x = deep()
";
    assert_eq!(g(src, "x"), "99");
}

// ── comprehension own-scope ───────────────────────────────────────────────────

#[test]
fn comprehension_loop_var_does_not_leak() {
    // Python 3 gives comprehensions their own scope: the loop variable must not
    // leak, but enclosing variables are still readable.
    assert_eq!(
        g("i = 'before'\nsq = [i * i for i in range(4)]\nx = i", "x"),
        "'before'"
    );
    assert_eq!(
        g("k = 'keep'\nd = {v: v for v in range(2)}\nx = k", "x"),
        "'keep'"
    );
    // Enclosing var is read inside the comprehension.
    assert_eq!(
        g("y = 100\nx = [n + y for n in range(3)]", "x"),
        "[100, 101, 102]"
    );
    // Nested comprehension loop vars also stay contained.
    assert_eq!(
        g(
            "j = 'j'\nx = [a * b for a in range(2) for b in range(3)]\nleaked = j",
            "leaked"
        ),
        "'j'"
    );
}

// ── Python floor division / modulo semantics ─────────────────────────────────

#[test]
fn floor_division_signs() {
    // `//` floors toward negative infinity for every sign combination.
    assert_eq!(g("x = -7 // 2", "x"), "-4");
    assert_eq!(g("x = 7 // -2", "x"), "-4");
    assert_eq!(g("x = -7 // -2", "x"), "3");
    assert_eq!(g("x = 7 // 2", "x"), "3");
    // A large operand exercises the BigInt floor path.
    assert_eq!(g("x = (-7 * 10**30) // (3 * 10**20)", "x"), "-23333333334");
}

#[test]
fn modulo_takes_divisor_sign() {
    // `%` result carries the sign of the divisor.
    assert_eq!(g("x = -7 % 100", "x"), "93");
    assert_eq!(g("x = -7 % -100", "x"), "-7");
    assert_eq!(g("x = 7 % -100", "x"), "-93");
    assert_eq!(g("x = 0 % -5", "x"), "0");
    // Float modulo also floors.
    assert_eq!(g("x = -7.0 % 3.0", "x"), "2.0");
    // BigInt modulo path.
    assert_eq!(g("x = (-7 * 10**25) % 100", "x"), "0");
    assert_eq!(g("x = (-(10**25) - 7) % 100", "x"), "93");
}

#[test]
fn pow_three_arg_modular() {
    assert_eq!(g("x = pow(2, 10, 1000)", "x"), "24");
    assert_eq!(g("x = pow(3, 4, 5)", "x"), "1");
    // Large exponent must not overflow (modular square-and-multiply).
    assert_eq!(g("x = pow(2, 1000, 10**9 + 7)", "x"), "688423210");
    // Negative base normalizes to the modulus sign.
    assert_eq!(g("x = pow(-3, 3, 7)", "x"), "1");
    // Negative modulus yields a non-positive result.
    assert_eq!(g("x = pow(2, 3, -5)", "x"), "-2");
}

// ── printf-style `str % args` ────────────────────────────────────────────────

#[test]
fn percent_format_numeric() {
    assert_eq!(g("x = '%.2f' % 3.14159", "x"), "'3.14'");
    assert_eq!(g("x = '%5d' % 42", "x"), "'   42'");
    assert_eq!(g("x = '%-5d|' % 42", "x"), "'42   |'");
    assert_eq!(g("x = '%05d' % 42", "x"), "'00042'");
    assert_eq!(g("x = '%+d' % 7", "x"), "'+7'");
    assert_eq!(g("x = '% d' % 7", "x"), "' 7'");
    assert_eq!(g("x = '%x' % 255", "x"), "'ff'");
    assert_eq!(g("x = '%#x' % 255", "x"), "'0xff'");
    assert_eq!(g("x = '%o' % 8", "x"), "'10'");
    assert_eq!(g("x = '%e' % 12345.678", "x"), "'1.234568e+04'");
    assert_eq!(g("x = '%.2e' % 12345.678", "x"), "'1.23e+04'");
    assert_eq!(g("x = '%g' % 0.0001", "x"), "'0.0001'");
    assert_eq!(g("x = '%g' % 0.00001", "x"), "'1e-05'");
    assert_eq!(g("x = '%g' % 1000000", "x"), "'1e+06'");
}

#[test]
fn percent_format_strings_and_star() {
    assert_eq!(g("x = '%s=%s' % ('k', 3)", "x"), "'k=3'");
    assert_eq!(g("x = '%r' % 'hi'", "x"), "\"'hi'\"");
    assert_eq!(g("x = '%.3s' % 'abcdef'", "x"), "'abc'");
    // `*` pulls width / precision from the argument tuple.
    assert_eq!(g("x = '%*d' % (5, 42)", "x"), "'   42'");
    assert_eq!(g("x = '%.*f' % (2, 3.14159)", "x"), "'3.14'");
    // Mapping form.
    assert_eq!(
        g("x = '%(name)s is %(age)d' % {'name': 'x', 'age': 5}", "x"),
        "'x is 5'"
    );
    assert_eq!(g("x = '%c%c' % (72, 105)", "x"), "'Hi'");
    assert_eq!(g("x = '100%%' % ()", "x"), "'100%'");
}

#[test]
fn bignum_bitwise_shift_and_conversions() {
    // Shifts route through the BigInt path (no i64 wraparound / no panic).
    assert_eq!(g("x = 1 << 64", "x"), "18446744073709551616");
    assert_eq!(g("x = 1 << 100", "x"), "1267650600228229401496703205376");
    assert_eq!(g("x = -5 >> 1", "x"), "-3");
    // Bitwise ops on values beyond i64.
    assert_eq!(g("x = (10 ** 30) & 7", "x"), "0");
    assert_eq!(g("x = ~(10 ** 20)", "x"), "-100000000000000000001");
    // Exact integer comparison beyond f64 precision.
    assert_eq!(g("x = 10 ** 20 < 10 ** 20 + 1", "x"), "True");
    // int(float) and radix conversions are bignum-safe.
    assert_eq!(g("x = int(1e20)", "x"), "100000000000000000000");
    assert_eq!(g("x = hex(10 ** 20)", "x"), "'0x56bc75e2d63100000'");
    assert_eq!(g("x = abs(-(10 ** 20))", "x"), "100000000000000000000");
    // Base parsing with a prefix, and underscores.
    assert_eq!(g("x = int('0x1F', 16)", "x"), "31");
    assert_eq!(g("x = int('1_000')", "x"), "1000");
    // `bool` bit-ops stay `bool`.
    assert_eq!(g("x = True & False", "x"), "False");
    assert_eq!(g("x = True | False", "x"), "True");
}

#[test]
fn negative_shift_is_catchable_valueerror() {
    // `1 >> -1` must raise a catchable ValueError, never abort the process.
    assert_eq!(
        g(
            "try:\n    1 >> -1\n    x = 'no error'\nexcept ValueError as e:\n    x = str(e)",
            "x"
        ),
        "'negative shift count'"
    );
}

#[test]
fn custom_getitem_slice_and_slice_repr() {
    // A user `__getitem__` receiving a slice must not stack-overflow, and the
    // returned slice object reprs like CPython.
    assert_eq!(
        g(
            "class C:\n    def __getitem__(self, k):\n        return k\nx = C()[1:5:2]",
            "x"
        ),
        "slice(1, 5, 2)"
    );
    assert_eq!(
        g(
            "class C:\n    def __getitem__(self, k):\n        return k\nx = C()[::-1]",
            "x"
        ),
        "slice(None, None, -1)"
    );
}

#[test]
fn static_and_class_methods() {
    let src = "
class C:
    tag = 'cls'
    @staticmethod
    def f(x):
        return x * 2
    @classmethod
    def g(cls, x):
        return cls.tag + str(x)
    @classmethod
    def make(cls):
        return cls()
class D(C):
    tag = 'D'
via_cls = C.f(5)
via_inst = C().f(3)
cm_cls = C.g(5)
cm_inst = C().g(7)
cm_inherit = D.g(9)
unbound = (lambda h: h(10))(C.f)
alt_ctor = type(C.make()).__name__
";
    assert_eq!(g(src, "via_cls"), "10");
    assert_eq!(g(src, "via_inst"), "6");
    assert_eq!(g(src, "cm_cls"), "'cls5'");
    assert_eq!(g(src, "cm_inst"), "'cls7'");
    // classmethod binds the *derived* class, so D.g sees D.tag.
    assert_eq!(g(src, "cm_inherit"), "'D9'");
    assert_eq!(g(src, "unbound"), "20");
    assert_eq!(g(src, "alt_ctor"), "'C'");
}

#[test]
fn type_returns_a_real_class() {
    // type(x) compares/repr's as a class, not an internal builtin-function object.
    assert_eq!(g("x = type(5) == int", "x"), "True");
    assert_eq!(g("x = type('a') == str", "x"), "True");
    assert_eq!(g("x = type([]) == list", "x"), "True");
    assert_eq!(g("x = type(5) is int", "x"), "True");
    assert_eq!(g("x = type(5) is str", "x"), "False");
    assert_eq!(g("x = isinstance(int, type)", "x"), "True");
    assert_eq!(g("x = str(int)", "x"), "\"<class 'int'>\"");
    // A user class: type(instance) equals and is-identical to the class object.
    let src =
        "class B:\n    pass\nb = B()\neq = type(b) == B\nis_ = type(b) is B\nnm = type(b).__name__";
    assert_eq!(g(src, "eq"), "True");
    assert_eq!(g(src, "is_"), "True");
    assert_eq!(g(src, "nm"), "'B'");
}

#[test]
fn super_cooperative_inheritance() {
    // super().__init__ + method extension through a single chain.
    let src = "
class A:
    def __init__(self, x):
        self.x = x
    def greet(self):
        return 'A' + str(self.x)
class B(A):
    def __init__(self, x, y):
        super().__init__(x)
        self.y = y
    def greet(self):
        return super().greet() + 'B' + str(self.y)
b = B(1, 2)
coords = (b.x, b.y)
msg = b.greet()
";
    assert_eq!(g(src, "coords"), "(1, 2)");
    assert_eq!(g(src, "msg"), "'A1B2'");
}

#[test]
fn super_diamond_c3_mro() {
    // Cooperative super() across a diamond must visit each base once, in C3 order.
    let src = "
class A:
    def m(self):
        return ['A']
class B(A):
    def m(self):
        return ['B'] + super().m()
class C(A):
    def m(self):
        return ['C'] + super().m()
class D(B, C):
    def m(self):
        return ['D'] + super().m()
x = D().m()
";
    assert_eq!(g(src, "x"), "['D', 'B', 'C', 'A']");
}

#[test]
fn numeric_keys_unify_in_dict_and_set() {
    // 1, 1.0, True hash and compare equal, so they collapse to one key.
    assert_eq!(g("x = 1.0 in {1}", "x"), "True");
    assert_eq!(g("x = True in {1}", "x"), "True");
    assert_eq!(g("x = len({1, 1.0, True})", "x"), "1");
    // The set keeps the FIRST-inserted element object (1, an int).
    assert_eq!(g("x = sorted({1, 1.0, True})", "x"), "[1]");
    assert_eq!(g("x = {1, 1.0, True}", "x"), "{1}");
    // Dict keeps the first key object, updates the value.
    assert_eq!(g("x = {1: 'a', 1.0: 'b', True: 'c'}", "x"), "{1: 'c'}");
    assert_eq!(
        g("d = {}\nd[1] = 'a'\nd[1.0] = 'b'\nx = d", "x"),
        "{1: 'b'}"
    );
    // Bignum-valued float unifies with the bignum int key.
    assert_eq!(g("x = len({10 ** 20, float(10 ** 20)})", "x"), "1");
    // Merge / update follow the same rule.
    assert_eq!(g("x = {**{1: 'a'}, **{1.0: 'b'}}", "x"), "{1: 'b'}");
    assert_eq!(
        g("d = {1.0: 'a'}\nd.update({1: 'b'})\nx = d", "x"),
        "{1.0: 'b'}"
    );
    // float() accepts bignums and underscore-grouped literals.
    assert_eq!(g("x = float('1_000.5')", "x"), "1000.5");
}

#[test]
fn round_bankers_and_negative_ndigits() {
    // Round-half-to-even (banker's), returning an int with no ndigits.
    assert_eq!(g("x = round(2.5)", "x"), "2");
    assert_eq!(g("x = round(0.5)", "x"), "0");
    assert_eq!(g("x = round(1.5)", "x"), "2");
    assert_eq!(g("x = round(-2.5)", "x"), "-2");
    // Representation-correct: 2.675 is really 2.6749…, so it rounds down.
    assert_eq!(g("x = round(2.675, 2)", "x"), "2.67");
    assert_eq!(g("x = round(1.5 / 10.0, 1)", "x"), "0.1");
    // ndigits present -> float, even for a whole result.
    assert_eq!(g("x = round(2.5, 0)", "x"), "2.0");
    // Negative ndigits round ints/floats to powers of ten (half-to-even).
    assert_eq!(g("x = round(12345, -2)", "x"), "12300");
    assert_eq!(g("x = round(1250, -2)", "x"), "1200");
    assert_eq!(g("x = round(1350, -2)", "x"), "1400");
    assert_eq!(g("x = round(123.456, -1)", "x"), "120.0");
}

#[test]
fn format_negative_and_bignum_radix() {
    // Negative ints format as sign + magnitude, not two's complement.
    assert_eq!(g("x = '{:b}'.format(-7)", "x"), "'-111'");
    assert_eq!(g("x = '{:x}'.format(-255)", "x"), "'-ff'");
    assert_eq!(g("x = '{:#x}'.format(-255)", "x"), "'-0xff'");
    assert_eq!(g("x = '{:08b}'.format(-7)", "x"), "'-0000111'");
    // Bignum-safe radix + decimal formatting.
    assert_eq!(g("x = '{:x}'.format(10 ** 20)", "x"), "'56bc75e2d63100000'");
    assert_eq!(
        g("x = '{:d}'.format(10 ** 20)", "x"),
        "'100000000000000000000'"
    );
    // The `format()` builtin path (regression: had a double-borrow panic).
    assert_eq!(g("x = format(255, 'x')", "x"), "'ff'");
    assert_eq!(g("x = format(-7, 'b')", "x"), "'-111'");
}

#[test]
fn slice_negative_step_clamping() {
    // Start beyond len with a negative step clamps to the last index.
    assert_eq!(g("x = [1, 2, 3, 4, 5][5:-2:-2]", "x"), "[5]");
    assert_eq!(g("x = (10, 20, 30, 40)[5::-2]", "x"), "(40, 20)");
    assert_eq!(g("x = (10, 20, 30, 40)[5:-2:-2]", "x"), "(40,)");
    assert_eq!(g("x = [0, 1, 2, 3, 4, 5, 6][10:2:-2]", "x"), "[6, 4]");
    assert_eq!(g("x = [1, 2, 3, 4, 5][-1:-4:-1]", "x"), "[5, 4, 3]");
}

#[test]
fn range_membership_is_constant_time() {
    // O(1) membership must not iterate a huge range.
    assert_eq!(g("x = 999999999999 in range(1000000000000)", "x"), "True");
    assert_eq!(g("x = 4 in range(0, 10, 2)", "x"), "True");
    assert_eq!(g("x = 5 in range(0, 10, 2)", "x"), "False");
    assert_eq!(g("x = 4 in range(10, 0, -2)", "x"), "True");
    // Integral float equals its int value; a fractional float never matches.
    assert_eq!(g("x = 2.0 in range(5)", "x"), "True");
    assert_eq!(g("x = 2.5 in range(5)", "x"), "False");
}

#[test]
fn property_descriptor() {
    // Read-only property.
    assert_eq!(
        g(
            "class C:\n    @property\n    def x(self): return 42\nx = C().x",
            "x"
        ),
        "42"
    );
    // getter + setter round-trip.
    assert_eq!(
        g(
            "class C:\n    @property\n    def v(self): return self._v\n    @v.setter\n    def v(self, n): self._v = n * 2\nc = C()\nc.v = 5\nx = c.v",
            "x"
        ),
        "10"
    );
    // property() functional form with fget/fset.
    assert_eq!(
        g(
            "class C:\n    def _g(self): return self._n + 1\n    def _s(self, n): self._n = n\n    n = property(_g, _s)\nc = C()\nc.n = 10\nx = c.n",
            "x"
        ),
        "11"
    );
}

#[test]
fn user_data_descriptor() {
    // A data descriptor (__get__/__set__) overrides the instance dict.
    assert_eq!(
        g(
            "class D:\n    def __get__(self, o, t=None): return o._raw * 3\n    def __set__(self, o, val): o._raw = val\nclass C:\n    d = D()\nc = C()\nc.d = 4\nx = c.d",
            "x"
        ),
        "12"
    );
}

#[test]
fn set_name_hook() {
    assert_eq!(
        g(
            "seen = []\nclass D:\n    def __set_name__(self, owner, name): seen.append((owner.__name__, name))\nclass C:\n    a = D()\n    b = D()\nx = seen",
            "x"
        ),
        "[('C', 'a'), ('C', 'b')]"
    );
}

#[test]
fn call_dunder() {
    assert_eq!(
        g(
            "class C:\n    def __call__(self, x): return x + 1\nc = C()\nx = c(41)",
            "x"
        ),
        "42"
    );
    assert_eq!(
        g(
            "class C:\n    def __call__(self): return 0\nx = callable(C())",
            "x"
        ),
        "True"
    );
    assert_eq!(g("class C:\n    pass\nx = callable(C())", "x"), "False");
}

#[test]
fn getattr_fallback() {
    assert_eq!(
        g(
            "class C:\n    def __getattr__(self, n): return 'dyn:' + n\nx = C().missing",
            "x"
        ),
        "'dyn:missing'"
    );
}

#[test]
fn format_dunder() {
    // f-string honors __format__ with the spec.
    assert_eq!(
        g(
            "class C:\n    def __format__(self, s): return 'F[' + s + ']'\nx = f'{C():>3}'",
            "x"
        ),
        "'F[>3]'"
    );
    // str.format honors __format__ and !r conversion.
    assert_eq!(
        g("class C:\n    def __format__(self, s): return 'z'\n    def __repr__(self): return 'R'\nx = '{}-{!r}'.format(C(), C())", "x"),
        "'z-R'"
    );
    // format() builtin.
    assert_eq!(
        g(
            "class C:\n    def __format__(self, s): return 'q' + s\nx = format(C(), 'w')",
            "x"
        ),
        "'qw'"
    );
}

#[test]
fn ne_derived_and_not_implemented() {
    // __ne__ is derived from __eq__ when not defined.
    assert_eq!(
        g("class C:\n    def __init__(s, v): s.v = v\n    def __eq__(s, o): return s.v == o.v\nx = (C(1) == C(1), C(1) != C(2), C(1) != C(1))", "x"),
        "(True, True, False)"
    );
    // Returning NotImplemented falls back to identity (== against a foreign type).
    assert_eq!(
        g("class A:\n    def __eq__(s, o):\n        if isinstance(o, A): return True\n        return NotImplemented\nx = (A() == A(), A() == 5, 5 == A())", "x"),
        "(True, False, False)"
    );
}

#[test]
fn unary_dunders() {
    // Unwrap to scalars so the test doesn't depend on __repr__ dispatch in the
    // read-back harness (repr_of is &self and can't call a method).
    assert_eq!(
        g("class V:\n    def __init__(s, x): s.x = x\n    def __neg__(s): return V(-s.x)\n    def __abs__(s): return V(abs(s.x))\n    def __invert__(s): return V(~s.x)\n    def __pos__(s): return V(+s.x)\nx = ((-V(5)).x, abs(V(-3)).x, (~V(4)).x, (+V(7)).x)", "x"),
        "(-5, 3, -5, 7)"
    );
}

#[test]
fn iteration_protocol() {
    // __getitem__ sequence-protocol iteration.
    assert_eq!(
        g("class S:\n    def __init__(s): s.d = [10, 20, 30]\n    def __getitem__(s, i):\n        if i >= len(s.d): raise IndexError\n        return s.d[i]\nx = [list(S()), 20 in S(), 99 in S()]", "x"),
        "[[10, 20, 30], True, False]"
    );
    // __contains__ overrides iteration.
    assert_eq!(
        g(
            "class C:\n    def __contains__(s, x): return x == 42\nx = (42 in C(), 1 in C())",
            "x"
        ),
        "(True, False)"
    );
    // __reversed__.
    assert_eq!(
        g(
            "class C:\n    def __reversed__(s): return iter([3, 2, 1])\nx = list(reversed(C()))",
            "x"
        ),
        "[3, 2, 1]"
    );
}

#[test]
fn new_dunder() {
    // __new__ creates the instance and __init__ receives the same args.
    assert_eq!(
        g("class C:\n    def __new__(cls, x): return object.__new__(cls)\n    def __init__(self, x): self.x = x * 2\nx = C(7).x", "x"),
        "14"
    );
    // __new__ returning a foreign object skips __init__.
    assert_eq!(
        g("class C:\n    def __new__(cls): return 99\n    def __init__(self): self.bad = True\nx = C()", "x"),
        "99"
    );
}

#[test]
fn bool_len_dunder_dispatch() {
    // bool()/any/all honor __bool__ then __len__ on instances.
    assert_eq!(
        g("class C:\n    def __init__(s, n): s.n = n\n    def __len__(s): return s.n\nx = (bool(C(0)), bool(C(3)), any([C(0), C(2)]), all([C(1), C(0)]))", "x"),
        "(False, True, True, False)"
    );
    assert_eq!(
        g(
            "class B:\n    def __bool__(s): return False\nx = bool(B())",
            "x"
        ),
        "False"
    );
}

#[test]
fn bare_reraise_in_handler() {
    // A bare `raise` in an except handler re-raises the active exception, caught
    // by an outer handler.
    assert_eq!(
        g(
            "def f():\n    try:\n        raise ValueError('boom')\n    except ValueError:\n        raise\nx = 'unset'\ntry:\n    f()\nexcept ValueError as e:\n    x = str(e)",
            "x"
        ),
        "'boom'"
    );
}

#[test]
fn instance_and_class_introspection() {
    // Instance __class__ / __dict__ and vars().
    assert_eq!(
        g("class C:\n    def __init__(s): s.a = 1; s.b = 2\nc = C()\nx = (c.__class__.__name__, c.__dict__, vars(c))", "x"),
        "('C', {'a': 1, 'b': 2}, {'a': 1, 'b': 2})"
    );
    // Class __bases__ / __mro__ names.
    assert_eq!(
        g("class A: pass\nclass B(A): pass\nx = ([b.__name__ for b in B.__bases__], [c.__name__ for c in B.__mro__])", "x"),
        "(['A'], ['B', 'A', 'object'])"
    );
    // User class repr carries the __main__ module qualifier (builtins don't).
    assert_eq!(
        g("class Widget: pass\nx = repr(Widget)", "x"),
        "\"<class '__main__.Widget'>\""
    );
}
