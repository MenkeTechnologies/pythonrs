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

/// str positional-arg cluster: split/rsplit maxsplit, find/rfind/index/rindex
/// and count honoring start/end, startswith/endswith honoring start/end and
/// tuple prefixes. Char-index space, faithful to CPython 3.14.
#[test]
fn str_split_maxsplit() {
    // sep + maxsplit
    assert_eq!(g("x = 'a,b,c'.split(',', 1)", "x"), "['a', 'b,c']");
    assert_eq!(g("x = 'a,b,c'.split(',', 0)", "x"), "['a,b,c']");
    assert_eq!(g("x = 'a,b,c'.split(',', 5)", "x"), "['a', 'b', 'c']");
    assert_eq!(g("x = 'a,b,,c'.split(',')", "x"), "['a', 'b', '', 'c']");
    // whitespace split (sep is None) honors maxsplit; tail keeps inner/trailing ws
    assert_eq!(g("x = '  a  b  c  '.split()", "x"), "['a', 'b', 'c']");
    assert_eq!(g("x = 'a b c d'.split(None, 2)", "x"), "['a', 'b', 'c d']");
}

#[test]
fn str_rsplit_maxsplit() {
    // splits from the right and honors maxsplit
    assert_eq!(g("x = 'a b c'.rsplit(' ', 1)", "x"), "['a b', 'c']");
    assert_eq!(g("x = 'a,b,c,d'.rsplit(',', 2)", "x"), "['a,b', 'c', 'd']");
    assert_eq!(g("x = 'a,b,c'.rsplit(',')", "x"), "['a', 'b', 'c']");
    // whitespace rsplit with maxsplit
    assert_eq!(g("x = 'a b c d'.rsplit(None, 1)", "x"), "['a b c', 'd']");
    // prog-name idiom from the argv drop-in
    assert_eq!(g("x = '/a/b/prog.py'.rsplit('/', 1)[-1]", "x"), "'prog.py'");
}

#[test]
fn str_find_rfind_start_end() {
    assert_eq!(g("x = 'abcabc'.find('a', 1)", "x"), "3");
    assert_eq!(g("x = 'abcabc'.rfind('a')", "x"), "3");
    assert_eq!(g("x = 'abcabc'.find('a', 1, 2)", "x"), "-1");
    assert_eq!(g("x = 'abcabc'.find('c', -2)", "x"), "5");
    assert_eq!(g("x = 'abcabc'.rfind('a', 0, 2)", "x"), "0");
    // unicode: char index (2), not byte index (3, since é is 2 bytes)
    assert_eq!(g("x = 'héllo'.find('l')", "x"), "2");
}

#[test]
fn str_index_rindex_start_end() {
    assert_eq!(g("x = 'abcabc'.index('b', 2)", "x"), "4");
    assert_eq!(g("x = 'abcabc'.rindex('b')", "x"), "4");
    // ValueError when not present in the given range
    assert_eq!(
        g(
            "try:\n    'abcabc'.index('b', 5)\n    x = 'no error'\nexcept ValueError as e:\n    x = type(e).__name__",
            "x"
        ),
        "'ValueError'"
    );
}

#[test]
fn str_count_start_end() {
    assert_eq!(g("x = 'abcabc'.count('a', 1)", "x"), "1");
    assert_eq!(g("x = 'abcabc'.count('a')", "x"), "2");
    assert_eq!(g("x = 'aaa'.count('a', 1, 2)", "x"), "1");
    // empty needle counts gaps within the range
    assert_eq!(g("x = 'abc'.count('')", "x"), "4");
    assert_eq!(g("x = 'abc'.count('', 1)", "x"), "3");
}

#[test]
fn str_startswith_endswith_start_end() {
    assert_eq!(g("x = 'hello'.startswith('l', 2)", "x"), "True");
    assert_eq!(g("x = 'hello'.endswith('ll', 0, 4)", "x"), "True");
    assert_eq!(g("x = 'hello'.startswith('l', 2, 3)", "x"), "True");
    assert_eq!(g("x = 'hello'.startswith('he')", "x"), "True");
    assert_eq!(g("x = 'hello'.endswith('lo')", "x"), "True");
    // tuple of prefixes still works
    assert_eq!(g("x = 'hello'.startswith(('x', 'he'))", "x"), "True");
    assert_eq!(g("x = 'hello'.endswith(('x', 'lo'))", "x"), "True");
    assert_eq!(g("x = 'hello'.startswith(('x', 'y'))", "x"), "False");
}

#[test]
fn percent_format_dispatches_instance_str_repr() {
    // `%s`/`%r`/`%a` must call the user instance's __str__/__repr__ (resolved
    // outside the host borrow), matching CPython byte-for-byte.
    let cls = "class C:\n    def __str__(s): return 'S'\n    def __repr__(s): return 'R'\n";
    assert_eq!(g(&format!("{cls}x = '%s' % C()"), "x"), "'S'");
    assert_eq!(g(&format!("{cls}x = '%r' % C()"), "x"), "'R'");
    assert_eq!(g(&format!("{cls}x = '%a' % C()"), "x"), "'R'");
    // mixed tuple: instance + plain value
    assert_eq!(g(&format!("{cls}x = '%s=%d' % (C(), 5)"), "x"), "'S=5'");
    assert_eq!(
        g(&format!("{cls}x = '%s %r %a' % (C(), C(), C())"), "x"),
        "'S R R'"
    );
    // container holding instances (recurses through repr dispatch)
    assert_eq!(
        g(&format!("{cls}x = '%s' % ([C(), C()],)"), "x"),
        "'[R, R]'"
    );
    assert_eq!(g(&format!("{cls}x = '%r' % ((C(),),)"), "x"), "'(R,)'");
    // mapping form
    assert_eq!(g(&format!("{cls}x = '%(k)r' % {{'k': C()}}"), "x"), "'R'");
    // width/precision still apply after dispatch
    assert_eq!(g(&format!("{cls}x = '[%5s]' % C()"), "x"), "'[    S]'");
    assert_eq!(g(&format!("{cls}x = '%.1s' % C()"), "x"), "'S'");
    // `%=` (desugars to `t = t % v`) goes through the same path
    assert_eq!(g(&format!("{cls}x = '%s'\nx %= C()"), "x"), "'S'");
    // `%a` ascii-escapes a non-ASCII dispatched repr
    assert_eq!(
        g(
            "class U:\n    def __repr__(s): return 'é'\nx = '%a' % U()",
            "x"
        ),
        "'\\\\xe9'"
    );
    // plain values unaffected (no regression)
    assert_eq!(g("x = '%s and %r' % ('a', 'b')", "x"), "\"a and 'b'\"");
}

#[test]
fn fstring_nested_format_specs() {
    // A format spec may itself contain replacement fields, evaluated at runtime
    // and spliced into the spec before formatting (CPython semantics).
    assert_eq!(g("x = f'{3.14159:{5}.{2}f}'", "x"), "' 3.14'");
    assert_eq!(
        g("w = 8\nn = 2\nx = f'{3.14159:{w}.{n}f}'", "x"),
        "'    3.14'"
    );
    assert_eq!(g("w = 8\nx = f'{42:>{w}}'", "x"), "'      42'");
    assert_eq!(g("w = 8\nx = f'{42:0{w}d}'", "x"), "'00000042'");
    assert_eq!(g("x = f'{\"x\":{\"*\"}>{6}}'", "x"), "'*****x'");
    assert_eq!(
        g("w = 10\nx = f'{\"mid\":{\"=\"}^{w}}'", "x"),
        "'===mid===='"
    );
    assert_eq!(g("w = 8\nx = f'{255:#{w}x}'", "x"), "'    0xff'");
    // nested field with its own conversion
    assert_eq!(g("w = 5\nx = f'{3.14:>{w}}'", "x"), "' 3.14'");
    // non-nested spec still works (no regression)
    assert_eq!(g("x = f'{3.14159:.2f}'", "x"), "'3.14'");
    assert_eq!(g("x = f'{42:05d}'", "x"), "'00042'");
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

#[test]
fn generator_send_throw_close() {
    // .send() feeds a value into the yield expression.
    assert_eq!(
        g("def acc():\n    t = 0\n    while True:\n        x = yield t\n        t += x\na = acc()\nnext(a)\ny1 = a.send(5)\ny2 = a.send(10)\nx = (y1, y2)", "x"),
        "(5, 15)"
    );
    // .throw() raises at the suspended yield; a handler can resume.
    assert_eq!(
        g("def g():\n    try:\n        yield 1\n    except ValueError:\n        yield 99\ngen = g()\nnext(gen)\nx = gen.throw(ValueError())", "x"),
        "99"
    );
    // .close() runs finally and stops the generator.
    assert_eq!(
        g("log = []\ndef g():\n    try:\n        yield 1\n    finally:\n        log.append('closed')\ngen = g()\nnext(gen)\ngen.close()\nx = log", "x"),
        "['closed']"
    );
}

#[test]
fn generator_return_value() {
    // StopIteration carries the generator's return value.
    assert_eq!(
        g("def g():\n    yield 1\n    return 42\ngen = g()\nnext(gen)\nval = None\ntry:\n    next(gen)\nexcept StopIteration as e:\n    val = e.value\nx = val", "x"),
        "42"
    );
    // `yield from` evaluates to the delegated generator's return value.
    assert_eq!(
        g("def sub():\n    yield 1\n    yield 2\n    return 99\ndef main():\n    r = yield from sub()\n    yield r\nx = list(main())", "x"),
        "[1, 2, 99]"
    );
}

#[test]
fn keyword_only_defaults() {
    // A keyword-only param with a default may be omitted.
    assert_eq!(
        g(
            "def f(a, *, c, d=4): return a + c + d\nx = (f(1, c=3), f(1, c=3, d=10))",
            "x"
        ),
        "(8, 14)"
    );
    // All-optional keyword-only.
    assert_eq!(g("def f(a, *, c=10): return a + c\nx = f(1)", "x"), "11");
    // Lambda keyword-only default.
    assert_eq!(
        g("h = lambda a, *, b=2: a * b\nx = (h(5), h(5, b=3))", "x"),
        "(10, 15)"
    );
    // Mixed positional + keyword-only defaults.
    assert_eq!(
        g(
            "def f(a=1, b=2, *, c=3, d=4): return (a, b, c, d)\nx = (f(), f(10, c=30))",
            "x"
        ),
        "((1, 2, 3, 4), (10, 2, 30, 4))"
    );
}

#[test]
fn zero_to_negative_power_raises() {
    // `0 ** <negative>` is a ZeroDivisionError, not `inf`.
    assert_eq!(
        g(
            "x = 'unset'\ntry:\n    0 ** -1\nexcept ZeroDivisionError:\n    x = 'zde'",
            "x"
        ),
        "'zde'"
    );
    // Non-zero base still works.
    assert_eq!(g("x = 2 ** -1", "x"), "0.5");
}

#[test]
fn slots_enforcement() {
    // A fully-slotted instance rejects undeclared attributes.
    assert_eq!(
        g("class P:\n    __slots__ = ('x', 'y')\n    def __init__(s): s.x = 1; s.y = 2\np = P()\nres = 'unset'\ntry:\n    p.z = 3\nexcept AttributeError:\n    res = 'blocked'\nx = (p.x, p.y, res)", "x"),
        "(1, 2, 'blocked')"
    );
    // A non-slotted base restores the instance __dict__ (slots don't restrict).
    assert_eq!(
        g("class B: pass\nclass D(B):\n    __slots__ = ('a',)\nd = D()\nd.a = 1\nd.b = 2\nx = (d.a, d.b)", "x"),
        "(1, 2)"
    );
}

#[test]
fn complex_arithmetic() {
    assert_eq!(g("x = (1+2j) + (3+4j)", "x"), "(4+6j)");
    assert_eq!(g("x = (1+2j) * (3+4j)", "x"), "(-5+10j)");
    assert_eq!(g("x = (1+2j) - (3+4j)", "x"), "(-2-2j)");
    assert_eq!(g("x = complex(1, 2)", "x"), "(1+2j)");
    assert_eq!(g("x = complex('1+2j')", "x"), "(1+2j)");
    assert_eq!(g("x = complex('-2j')", "x"), "-2j");
    assert_eq!(g("x = abs(3+4j)", "x"), "5.0");
    assert_eq!(g("x = (2+3j).conjugate()", "x"), "(2-3j)");
    assert_eq!(g("x = ((2+3j).real, (2+3j).imag)", "x"), "(2.0, 3.0)");
    assert_eq!(g("x = (2+3j) ** 2", "x"), "(-5+12j)");
    assert_eq!(g("x = 2j ** 2", "x"), "(-4+0j)");
    // A negative real base to a fractional power yields a complex root.
    assert_eq!(
        g("x = (-8) ** (1/3)", "x"),
        "(1.0000000000000002+1.7320508075688772j)"
    );
    assert_eq!(g("x = (1+2j) == (1+2j)", "x"), "True");
    assert_eq!(g("x = bool(0j)", "x"), "False");
    // A zero-imaginary complex keys the same slot as the equal real number.
    assert_eq!(g("x = complex(1, 0) in {1}", "x"), "True");
}

#[test]
fn exception_chaining() {
    // `raise X from Y` sets __cause__ (and __suppress_context__).
    assert_eq!(
        g(
            "try:\n    try:\n        raise ValueError('inner')\n    except ValueError as e:\n        raise TypeError('outer') from e\nexcept TypeError as t:\n    x = type(t.__cause__).__name__",
            "x"
        ),
        "'ValueError'"
    );
    assert_eq!(
        g(
            "try:\n    try:\n        raise ValueError('inner')\n    except ValueError as e:\n        raise TypeError('outer') from e\nexcept TypeError as t:\n    x = t.__suppress_context__",
            "x"
        ),
        "True"
    );
    // Implicit __context__ during handling; no explicit cause.
    assert_eq!(
        g(
            "try:\n    try:\n        raise ValueError('v')\n    except ValueError:\n        raise TypeError('t')\nexcept TypeError as t:\n    x = (type(t.__context__).__name__, t.__cause__)",
            "x"
        ),
        "('ValueError', None)"
    );
    // User exception class carries a chain via the side table.
    assert_eq!(
        g(
            "class E(Exception): pass\ntry:\n    raise E('x') from ValueError('c')\nexcept E as e:\n    x = type(e.__cause__).__name__",
            "x"
        ),
        "'ValueError'"
    );
}

#[test]
fn lazy_iterators() {
    // zip/map/filter/enumerate are lazy iterator objects, not eager lists.
    assert_eq!(g("x = type(zip([1],[2])).__name__", "x"), "'zip'");
    assert_eq!(g("x = type(map(str,[1])).__name__", "x"), "'map'");
    assert_eq!(g("x = type(filter(None,[1])).__name__", "x"), "'filter'");
    assert_eq!(g("x = type(enumerate([1])).__name__", "x"), "'enumerate'");
    // next() drives them; they exhaust once.
    assert_eq!(
        g(
            "z = zip([1,2],[3,4])\nx = (next(z), list(z), next(z, 'end'))",
            "x"
        ),
        "((1, 3), [(2, 4)], 'end')"
    );
    assert_eq!(g("x = list(map(lambda a: a*2, [1,2,3]))", "x"), "[2, 4, 6]");
    assert_eq!(
        g("x = list(filter(lambda a: a % 2, range(10)))", "x"),
        "[1, 3, 5, 7, 9]"
    );
    assert_eq!(
        g("x = list(enumerate('ab', start=5))", "x"),
        "[(5, 'a'), (6, 'b')]"
    );
    // reversed is a one-shot iterator, not a list.
    assert_eq!(
        g("r = reversed([1,2,3])\nx = (next(r), list(r))", "x"),
        "(3, [2, 1])"
    );
    // Infinite source never materializes (would hang if eager).
    assert_eq!(
        g(
            "def c():\n    i=0\n    while True:\n        yield i\n        i+=1\nx = list(zip(c(), ['a','b','c']))",
            "x"
        ),
        "[(0, 'a'), (1, 'b'), (2, 'c')]"
    );
}

#[test]
fn frozenset_type() {
    assert_eq!(g("x = frozenset([1,2,2])", "x"), "frozenset({1, 2})");
    assert_eq!(g("x = frozenset()", "x"), "frozenset()");
    assert_eq!(g("x = type(frozenset()).__name__", "x"), "'frozenset'");
    // Hashable: usable as a dict key and a set member.
    assert_eq!(
        g("d = {frozenset([1,2]): 'a'}\nx = d[frozenset([2,1])]", "x"),
        "'a'"
    );
    assert_eq!(
        g(
            "x = len({frozenset([1,2]), frozenset([2,1]), frozenset([3])})",
            "x"
        ),
        "2"
    );
    // Set algebra: result type follows the left operand.
    assert_eq!(
        g("x = type(frozenset([1,2]) | {3}).__name__", "x"),
        "'frozenset'"
    );
    assert_eq!(g("x = type({1,2} | frozenset([3])).__name__", "x"), "'set'");
    assert_eq!(
        g("x = frozenset([1,2,3]) & frozenset([2,3,4])", "x"),
        "frozenset({2, 3})"
    );
    // isinstance: frozenset is not a set and vice versa.
    assert_eq!(
        g("x = (isinstance(frozenset(), frozenset), isinstance(frozenset(), set), isinstance({1}, frozenset))", "x"),
        "(True, False, False)"
    );
    // set == frozenset by membership.
    assert_eq!(g("x = frozenset([1,2]) == {1,2}", "x"), "True");
}

#[test]
fn set_ops_and_comparisons() {
    // Subset partial-order operators.
    assert_eq!(
        g("x = ({1,2} <= {1,2,3}, {1,2} < {1,2})", "x"),
        "(True, False)"
    );
    assert_eq!(
        g("x = ({1,2} < {3,4}, {1,2} > {3,4})", "x"),
        "(False, False)"
    );
    assert_eq!(g("x = {1,2,3} > {1,2}", "x"), "True");
    // isdisjoint and the *_update mutators (accept any iterable).
    assert_eq!(g("x = {1,2}.isdisjoint([3,4])", "x"), "True");
    assert_eq!(g("x = {1,2}.isdisjoint([2,3])", "x"), "False");
    assert_eq!(
        g("s = {1,2,3}\ns.intersection_update([2,3,4])\nx = s", "x"),
        "{2, 3}"
    );
    assert_eq!(
        g("s = {1,2,3}\ns.difference_update([2])\nx = s", "x"),
        "{1, 3}"
    );
    assert_eq!(
        g(
            "s = {1,2,3}\ns.symmetric_difference_update([3,4])\nx = s",
            "x"
        ),
        "{1, 2, 4}"
    );
    assert_eq!(g("x = {1,2,3}.issubset([1,2,3,4])", "x"), "True");
}

#[test]
fn dict_views_and_merge() {
    // Views are live view objects, not list snapshots.
    assert_eq!(
        g("d = {1:2,3:4}\nx = type(d.keys()).__name__", "x"),
        "'dict_keys'"
    );
    assert_eq!(g("d = {1:2,3:4}\nx = d.keys()", "x"), "dict_keys([1, 3])");
    assert_eq!(
        g("d = {1:2,3:4}\nx = d.items()", "x"),
        "dict_items([(1, 2), (3, 4)])"
    );
    // Live update: a view reflects later mutation.
    assert_eq!(
        g("d = {1:2}\nk = d.keys()\nd[3] = 4\nx = sorted(k)", "x"),
        "[1, 3]"
    );
    // View set-ops return a set.
    assert_eq!(g("d = {1:2}\nx = d.keys() | {3}", "x"), "{1, 3}");
    assert_eq!(g("d = {1:2,3:4}\nx = d.items() & {(1,2)}", "x"), "{(1, 2)}");
    // fromkeys, dict merge, update variants.
    assert_eq!(
        g("x = dict.fromkeys([1,2,3])", "x"),
        "{1: None, 2: None, 3: None}"
    );
    assert_eq!(g("x = dict.fromkeys([1,2], 0)", "x"), "{1: 0, 2: 0}");
    assert_eq!(g("x = {1:2} | {3:4}", "x"), "{1: 2, 3: 4}");
    assert_eq!(g("d = {1:2}\nd |= {3:4}\nx = d", "x"), "{1: 2, 3: 4}");
    assert_eq!(
        g("d = {}\nd.update(a=1, b=2)\nx = d", "x"),
        "{'a': 1, 'b': 2}"
    );
    assert_eq!(
        g("d = {}\nd.update([(1,2),(3,4)])\nx = d", "x"),
        "{1: 2, 3: 4}"
    );
}

#[test]
fn range_methods_and_equality() {
    assert_eq!(g("x = range(10)[2:8:2]", "x"), "range(2, 8, 2)");
    assert_eq!(g("x = list(range(10)[2:8:2])", "x"), "[2, 4, 6]");
    assert_eq!(g("x = range(10)[::-1]", "x"), "range(9, -1, -1)");
    assert_eq!(g("x = range(10).index(4)", "x"), "4");
    assert_eq!(g("x = range(0,20,2).index(6)", "x"), "3");
    assert_eq!(
        g("x = (range(10).count(4), range(10).count(99))", "x"),
        "(1, 0)"
    );
    assert_eq!(g("x = range(10) == range(0, 10)", "x"), "True");
    assert_eq!(g("x = range(0) == range(5, 5)", "x"), "True");
    assert_eq!(g("x = range(0,10,2) == range(0,11,2)", "x"), "False");
    assert_eq!(g("x = range(0,10,2) == range(0,9,2)", "x"), "True");
}

#[test]
fn slice_assignment_and_del() {
    assert_eq!(g("x = [1,2,3,4,5]\nx[1:3] = [9]\n", "x"), "[1, 9, 4, 5]");
    assert_eq!(
        g("x = [1,2,3,4,5]\nx[1:1] = [8,9]\n", "x"),
        "[1, 8, 9, 2, 3, 4, 5]"
    );
    assert_eq!(
        g("x = [1,2,3,4,5,6]\nx[::2] = [7,8,9]\n", "x"),
        "[7, 2, 8, 4, 9, 6]"
    );
    assert_eq!(g("x = [1,2,3]\nx[:] = [9,9,9,9]\n", "x"), "[9, 9, 9, 9]");
    assert_eq!(g("x = [1,2,3,4,5]\nx[1:4] = []\n", "x"), "[1, 5]");
    assert_eq!(g("x = [1,2,3,4,5]\ndel x[1:3]\n", "x"), "[1, 4, 5]");
    assert_eq!(g("x = [1,2,3,4,5,6]\ndel x[::2]\n", "x"), "[2, 4, 6]");
    // A generator RHS is materialized without a borrow panic.
    assert_eq!(
        g("x = [1,2,3]\nx[1:2] = (i for i in [7,8])\n", "x"),
        "[1, 7, 8, 3]"
    );
}

#[test]
fn str_methods_tier5() {
    assert_eq!(g("x = 'a.b.c'.partition('.')", "x"), "('a', '.', 'b.c')");
    assert_eq!(g("x = 'a.b.c'.rpartition('.')", "x"), "('a.b', '.', 'c')");
    assert_eq!(g("x = 'x'.partition('.')", "x"), "('x', '', '')");
    assert_eq!(g("x = 'abcb'.rindex('b')", "x"), "3");
    assert_eq!(
        g("x = ('123'.isnumeric(), 'abc'.isnumeric())", "x"),
        "(True, False)"
    );
    assert_eq!(
        g("x = ('1'.isdecimal(), '\u{00bd}'.isdecimal())", "x"),
        "(True, False)"
    );
    assert_eq!(
        g("x = ('Hello World'.istitle(), 'hello'.istitle())", "x"),
        "(True, False)"
    );
    assert_eq!(
        g("x = ('abc'.isidentifier(), '1a'.isidentifier())", "x"),
        "(True, False)"
    );
    assert_eq!(g("x = 'a\\tbc'.expandtabs(4)", "x"), "'a   bc'");
    assert_eq!(g("x = 'abc'.translate({97:98})", "x"), "'bbc'");
    assert_eq!(
        g("x = 'hello'.translate(str.maketrans('lo','LO'))", "x"),
        "'heLLO'"
    );
    assert_eq!(g("x = str.maketrans('ab','xy')", "x"), "{97: 120, 98: 121}");
    assert_eq!(g("x = '{a:.2f}'.format_map({'a':3.14159})", "x"), "'3.14'");
}

#[test]
fn repr_escaping_and_ascii_and_octal() {
    // repr escapes C0 controls; ascii escapes non-ASCII. `g` reprs the string
    // global, so these are the double-repr forms python3 also produces.
    assert_eq!(g(r#"x = repr("a\x00b\x1f")"#, "x"), r#""'a\\x00b\\x1f'""#);
    assert_eq!(g("x = ascii('caf\u{00e9}')", "x"), r#""'caf\\xe9'""#);
    // Octal string escape.
    assert_eq!(g(r#"x = "\101\102\103""#, "x"), "'ABC'");
    // Printable Unicode is kept verbatim in repr.
    assert_eq!(g("x = repr('\u{00e9}')", "x"), "\"'\u{00e9}'\"");
}

#[test]
fn three_arg_type_and_posonly() {
    // Dynamic class creation via 3-arg type().
    assert_eq!(
        g("C = type('C', (), {'x': 5})\nx = (C.x, C.__name__)", "x"),
        "(5, 'C')"
    );
    assert_eq!(
        g(
            "C = type('C', (), {'m': lambda self: 42})\nx = C().m()",
            "x"
        ),
        "42"
    );
    assert_eq!(
        g(
            "class B:\n    def f(self): return 7\nD = type('D', (B,), {})\nx = D().f()",
            "x"
        ),
        "7"
    );
    // Positional-only enforcement.
    assert_eq!(
        g("def f(a, b, /, c): return a+b+c\nx = f(1, 2, c=3)", "x"),
        "6"
    );
    assert_eq!(
        g("def f(a, /, **kw): return (a, kw)\nx = f(1, a=2)", "x"),
        "(1, {'a': 2})"
    );
    assert_eq!(
        g(
            "def f(a, b, /): return a+b\ntry:\n    f(a=1, b=2)\nexcept TypeError:\n    x = 'rejected'",
            "x"
        ),
        "'rejected'"
    );
}

#[test]
fn named_unicode_escapes() {
    // \N{NAME} resolves to the codepoint, in normal strings and f-strings.
    // Expected values match CPython 3.14 byte for byte.
    assert_eq!(g("x = '\\N{LATIN SMALL LETTER E WITH ACUTE}'", "x"), "'é'");
    assert_eq!(
        g("x = '\\N{GREEK SMALL LETTER ALPHA}\\N{BULLET}'", "x"),
        "'α•'"
    );
    assert_eq!(g("x = len('\\N{ROCKET}')", "x"), "1");
    assert_eq!(g("x = ord('\\N{SNOWMAN}')", "x"), "9731");
    // Case-insensitive name matching (CPython accepts lowercase).
    assert_eq!(g("x = '\\N{bullet}'", "x"), "'•'");
    // f-string: the escape's braces are not a replacement field.
    assert_eq!(g("x = f'a\\N{BULLET}b {1+1}'", "x"), "'a•b 2'");
    assert_eq!(g("x = f'\\N{ROCKET}{7}'", "x"), "'🚀7'");
    // An escaped backslash means \N is literal, not an escape.
    assert_eq!(g("x = '\\\\N{BULLET}'", "x"), "'\\\\N{BULLET}'");
}

#[test]
fn named_unicode_escape_errors() {
    // Unknown name (CPython's exact unicodeescape error, byte-identical payload).
    let e = eval_str("x = '\\N{NOT A REAL NAME}'").unwrap_err();
    assert!(
        e.contains(
            "(unicode error) 'unicodeescape' codec can't decode bytes in position 0-18: unknown Unicode character name"
        ),
        "got: {e}"
    );
    // Position offset accounts for a leading char.
    let e = eval_str("x = 'x\\N{BOGUS NAME HERE}'").unwrap_err();
    assert!(
        e.contains("position 1-19: unknown Unicode character name"),
        "got: {e}"
    );
    // Empty braces -> malformed.
    let e = eval_str("x = '\\N{}'").unwrap_err();
    assert!(
        e.contains("position 0-2: malformed \\N character escape"),
        "got: {e}"
    );
    // Missing brace -> malformed.
    let e = eval_str("x = '\\Nfoo'").unwrap_err();
    assert!(
        e.contains("position 0-1: malformed \\N character escape"),
        "got: {e}"
    );
    // Unterminated brace -> malformed, spans to end of literal.
    let e = eval_str("x = '\\N{FOO'").unwrap_err();
    assert!(
        e.contains("position 0-5: malformed \\N character escape"),
        "got: {e}"
    );
    // CPython matches case-insensitively but NOT loosely: stray whitespace or
    // underscore-for-space must fail.
    assert!(eval_str("x = '\\N{ SPACE}'").is_err());
    assert!(eval_str("x = '\\N{GREEK_SMALL_LETTER_ALPHA}'").is_err());
    // f-string unknown name also errors.
    assert!(eval_str("x = f'\\N{NOPE}'").is_err());
}

#[test]
fn decode_escapes_named_unicode_unit() {
    use pythonrs::lexer::decode_escapes;
    assert_eq!(decode_escapes("\\N{BULLET}", false).unwrap(), "•");
    assert_eq!(
        decode_escapes("\\N{LATIN SMALL LETTER E WITH ACUTE}", false).unwrap(),
        "é"
    );
    // Raw strings keep the escape literal.
    assert_eq!(decode_escapes("\\N{BULLET}", true).unwrap(), "\\N{BULLET}");
    assert!(decode_escapes("\\N{ SPACE}", false).is_err());
}

#[test]
fn set_repr_cpython_hash_order() {
    // A set/frozenset of machine ints iterates and reprs in CPython's
    // open-addressing table order, not insertion order. `set(iterable)` builds
    // incrementally, exactly as pythonrs does, so these match byte-for-byte.
    assert_eq!(g("x = set([3, 1, 2])", "x"), "{1, 2, 3}");
    assert_eq!(g("x = set([10, 5, 1, 2, 3])", "x"), "{1, 2, 3, 5, 10}");
    assert_eq!(g("x = set([-1, -5, 3])", "x"), "{3, -5, -1}");
    assert_eq!(g("x = set([100, 1, 50])", "x"), "{1, 50, 100}");
    assert_eq!(g("x = frozenset([3, 1, 2])", "x"), "frozenset({1, 2, 3})");
    // Colliding ints beyond the initial table (drives a resize + linear probing).
    assert_eq!(g("x = set([9, 1, 17, 25, 33])", "x"), "{33, 1, 9, 17, 25}");
    // Iteration follows the same order.
    assert_eq!(
        g("x = list(set([10, 5, 1, 2, 3]))", "x"),
        "[1, 2, 3, 5, 10]"
    );
    // `1`, `1.0`, `True` unify to one element (int key), repr uses the first.
    assert_eq!(g("x = set([2.0, 1])", "x"), "{1, 2.0}");
}

#[test]
fn metaclasses() {
    // `class A(metaclass=M)` runs `M.__new__`/`M.__init__`; `type(A) is M`.
    let base = "class M(type):\n    def __new__(mcls, name, bases, ns):\n        ns['injected'] = 99\n        return super().__new__(mcls, name, bases, ns)\n    def __init__(cls, name, bases, ns):\n        cls.tag = name.lower()\n        super().__init__(name, bases, ns)\nclass A(metaclass=M):\n    pass\n";
    assert_eq!(
        g(&format!("{base}x = (A.injected, A.tag, type(A) is M)"), "x"),
        "(99, 'a', True)"
    );
    // A subclass inherits the metaclass (no explicit `metaclass=`).
    assert_eq!(
        g(
            &format!("{base}class B(A): pass\nx = (type(B) is M, B.injected)"),
            "x"
        ),
        "(True, 99)"
    );
    // A metaclass method is callable on the class, bound to the class.
    assert_eq!(
        g("class M(type):\n    def kind(cls): return cls.__name__ + '!'\nclass A(metaclass=M): pass\nx = A.kind()", "x"),
        "'A!'"
    );
    // Metaclass `__call__` controls instantiation (singleton pattern).
    let singleton = "class S(type):\n    _i = {}\n    def __call__(cls, *a):\n        if cls not in cls._i:\n            cls._i[cls] = super().__call__(*a)\n        return cls._i[cls]\nclass DB(metaclass=S):\n    def __init__(self): self.v = 7\n";
    assert_eq!(
        g(
            &format!("{singleton}a = DB()\nb = DB()\nx = (a is b, a.v)"),
            "x"
        ),
        "(True, 7)"
    );
    // 3-arg `type(name, bases, ns)` builds an ordinary class (`type` metaclass).
    assert_eq!(
        g(
            "D = type('D', (), {'v': 5})\nx = (D.v, type(D) is type)",
            "x"
        ),
        "(5, True)"
    );
    // A class object is usable as a dict key (identity by name).
    assert_eq!(g("x = {int: 'i', str: 's'}[int]", "x"), "'i'");
}

#[test]
fn instance_hash_dict_set_keys() {
    // A class with `__hash__` + `__eq__` gives value-equal instances one dict/set
    // slot; lookups with an equal-but-distinct instance find the entry.
    const C: &str = "class C:\n    def __init__(s, v): s.v = v\n    def __hash__(s): return s.v\n    def __eq__(s, o): return isinstance(o, C) and s.v == o.v\n";
    assert_eq!(
        g(
            &format!("{C}d = {{C(1): 'a', C(2): 'b'}}\nx = d[C(1)]"),
            "x"
        ),
        "'a'"
    );
    // Value-equal keys collapse; a re-store updates in place.
    assert_eq!(
        g(
            &format!("{C}d = {{C(1): 'a'}}\nd[C(1)] = 'z'\nx = (len(d), d[C(1)])"),
            "x"
        ),
        "(1, 'z')"
    );
    // Set membership + dedup of equal instances.
    assert_eq!(
        g(
            &format!("{C}s = {{C(1), C(2), C(1)}}\nx = (len(s), C(1) in s, C(9) in s)"),
            "x"
        ),
        "(2, True, False)"
    );
    // `hash()` returns the `__hash__` result verbatim.
    assert_eq!(g(&format!("{C}x = hash(C(42))"), "x"), "42");
    // A bare class (no `__hash__`/`__eq__`) is hashable by identity.
    assert_eq!(
        g(
            "class B: pass\nb = B()\nd = {b: 1}\nx = (d[b], B() in d)",
            "x"
        ),
        "(1, False)"
    );
    // `__eq__` without `__hash__` (and `__hash__ = None`) makes it unhashable.
    for body in ["def __eq__(s, o): return True", "__hash__ = None"] {
        let src = format!("class U:\n    {body}\ntry:\n    _ = {{U()}}\n    x = 'hashable'\nexcept TypeError:\n    x = 'unhashable'");
        assert_eq!(g(&src, "x"), "'unhashable'");
    }
}

#[test]
fn walrus_in_comprehension_leaks() {
    // A `:=` target inside a comprehension binds in the enclosing scope (PEP 572),
    // not the hidden comprehension function; the result is unaffected.
    assert_eq!(
        g("r = range(3)\nres = [y for x in r if (y := x)]", "res"),
        "[1, 2]"
    );
    assert_eq!(g("r = range(3)\n_ = [y for x in r if (y := x)]", "y"), "2");
    // Walrus in the element, over a list.
    assert_eq!(g("_ = [(z := i) + z for i in [1, 2, 3]]", "z"), "3");
    // Set and dict comprehensions leak their walrus target too.
    assert_eq!(g("_ = {(k := x) for x in range(4)}", "k"), "3");
    assert_eq!(g("_ = {(m := x): x for x in range(2)}", "m"), "1");
    // Inside a function the target is nonlocal to that function, not global; the
    // function exposes it via its return so we can read it back at module scope.
    assert_eq!(
        g(
            "def f():\n    t = -1\n    out = [t for x in range(3) if (t := x * 2)]\n    return out, t\nres = f()",
            "res"
        ),
        "([2, 4], 4)"
    );
}

#[test]
fn user_exception_str_repr_args() {
    // A user Exception subclass inherits BaseException's args/str/repr: str is
    // the message ('' / str(arg) / repr(tuple)), repr is `Class(arg, …)`.
    assert_eq!(
        g("class E(Exception): pass\ns = str(E('boom'))", "s"),
        "'boom'"
    );
    assert_eq!(g("class E(Exception): pass\ns = str(E())", "s"), "''");
    assert_eq!(
        g("class E(Exception): pass\ns = str(E('a', 'b'))", "s"),
        "\"('a', 'b')\""
    );
    assert_eq!(
        g("class E(Exception): pass\nr = repr(E('a', 'b'))", "r"),
        "\"E('a', 'b')\""
    );
    assert_eq!(g("class E(Exception): pass\nr = repr(E())", "r"), "'E()'");
    assert_eq!(
        g("class E(Exception): pass\na = E('x', 1).args", "a"),
        "('x', 1)"
    );
    assert_eq!(g("class E(Exception): pass\na = E().args", "a"), "()");
    // isinstance across the builtin hierarchy + user subclass chain.
    assert_eq!(
        g(
            "class A(Exception): pass\nclass B(A): pass\nb = isinstance(B('m'), A) and isinstance(B('m'), Exception)",
            "b"
        ),
        "True"
    );
    // A user __init__ that calls super().__init__ overrides args; a custom
    // __str__ still leaves the default repr = `Class(args…)`.
    assert_eq!(
        g(
            "class E(Exception):\n    def __init__(self, k):\n        super().__init__('missing ' + k)\n        self.k = k\ne = E('id')\nres = (str(e), e.args, e.k)",
            "res"
        ),
        "('missing id', ('missing id',), 'id')"
    );
    assert_eq!(
        g(
            "class E(Exception):\n    def __str__(self): return 'custom'\nres = (str(E('z')), repr(E('z')))",
            "res"
        ),
        "('custom', \"E('z')\")"
    );
    // Caught user exception: `e` and `e.args` are usable in the handler.
    assert_eq!(
        g(
            "out = None\nclass E(Exception): pass\ntry:\n    raise E('bang')\nexcept E as e:\n    out = (str(e), e.args)",
            "out"
        ),
        "('bang', ('bang',))"
    );
}

#[test]
fn super_in_property_accessor() {
    // A zero-arg super() inside a property getter resolves self + the defining
    // class, so both super().<method>() and super().<property> work.
    assert_eq!(
        g(
            "class A:\n    def base(self): return 10\nclass B(A):\n    @property\n    def v(self): return super().base() + 1\nx = B().v",
            "x"
        ),
        "11"
    );
    assert_eq!(
        g(
            "class A:\n    @property\n    def v(self): return 10\nclass B(A):\n    @property\n    def v(self): return super().v + 5\nx = B().v",
            "x"
        ),
        "15"
    );
}

#[test]
fn fstring_ascii_conversion() {
    // `!a` ascii-escapes non-ASCII in the repr (previously passed repr through).
    // Built via chr() so the expected value has no backslash-escaping ambiguity:
    // ascii(chr(233)) == "'" + "\\" + "xe9" + "'".
    assert_eq!(
        g("b = f'{chr(233)!a}' == chr(39)+chr(92)+'xe9'+chr(39)", "b"),
        "True"
    );
    assert_eq!(
        g(
            "b = f'{chr(1000)!a}' == chr(39)+chr(92)+'u03e8'+chr(39)",
            "b"
        ),
        "True"
    );
    // The `ascii()` builtin agrees with `!a`.
    assert_eq!(
        g("b = ascii(chr(233)) == chr(39)+chr(92)+'xe9'+chr(39)", "b"),
        "True"
    );
    // `!r` leaves non-ASCII intact: repr(chr(233)) == "'é'".
    assert_eq!(
        g("b = f'{chr(233)!r}' == chr(39)+chr(233)+chr(39)", "b"),
        "True"
    );
}

#[test]
fn str_percent_format_native_authoritative() {
    // `str % obj` is native formatting (str.__mod__), authoritative over any
    // right-operand __rmod__: a %s/%r of an exception instance uses its message.
    assert_eq!(
        g("class E(Exception): pass\ns = '%s' % E('boom')", "s"),
        "'boom'"
    );
    assert_eq!(
        g("class E(Exception): pass\ns = '%r' % E('x', 1)", "s"),
        "\"E('x', 1)\""
    );
    // A right operand with `__rmod__` never intercepts `str %` — str formatting
    // wins, so a mismatched arg count raises rather than calling __rmod__.
    let e = eval_str("class V:\n    def __rmod__(self, o): return 'nope'\nx = 'lit' % V()")
        .unwrap_err();
    assert!(
        e.contains("not all arguments converted"),
        "unexpected error: {e}"
    );
    // Plain-value %-format (tuples, %r) is unaffected.
    assert_eq!(g("s = '%s=%r' % ('k', (1, 2))", "s"), "'k=(1, 2)'");
}

#[test]
fn init_subclass_hook() {
    // PEP 487: the parent's __init_subclass__ fires with the new class and the
    // class-header keywords.
    assert_eq!(
        g(
            "class P:\n    def __init_subclass__(cls, /, tag=None, **kw):\n        cls.tag = tag\nclass C(P, tag='x'): pass\nt = C.tag",
            "t"
        ),
        "'x'"
    );
    // An explicit @classmethod form and no-keyword default both work.
    assert_eq!(
        g(
            "seen = []\nclass P:\n    @classmethod\n    def __init_subclass__(cls, **kw):\n        seen.append(cls.__name__)\nclass C(P): pass\nout = seen",
            "out"
        ),
        "['C']"
    );
    // Extra keywords with only object's default hook is a TypeError.
    let e = eval_str("class P: pass\nclass C(P, tag='x'): pass").unwrap_err();
    assert!(
        e.contains("__init_subclass__() takes no keyword arguments"),
        "unexpected error: {e}"
    );
}

#[test]
fn format_spec_sign_aware_zero_pad() {
    // The `0` flag / `=` align inserts fill AFTER the sign and any radix prefix.
    assert_eq!(g("s = f'{5:+05d}'", "s"), "'+0005'");
    assert_eq!(g("s = f'{-3:05d}'", "s"), "'-0003'");
    assert_eq!(g("s = f'{5: 05d}'", "s"), "' 0005'");
    assert_eq!(g("s = f'{255:#08x}'", "s"), "'0x0000ff'");
    assert_eq!(g("s = f'{-255:#08x}'", "s"), "'-0x000ff'");
    assert_eq!(g("s = f'{3.14:+08.2f}'", "s"), "'+0003.14'");
    assert_eq!(g("s = f'{-42:=8d}'", "s"), "'-     42'");
    // A `+`/space sign flag prefixes a non-negative value.
    assert_eq!(g("s = f'{5: d}'", "s"), "' 5'");
    assert_eq!(g("s = f'{7:>6d}'", "s"), "'     7'");
}

// ── async / await / asyncio (native fusevm event loop) ───────────────────────

#[test]
fn async_def_returns_coroutine() {
    // Calling an `async def` returns a coroutine object; the body does NOT run.
    assert_eq!(
        g(
            "async def f():\n    return 1\nc = f()\nt = type(c).__name__\nimport asyncio\nasyncio.run(c)",
            "t"
        ),
        "'coroutine'"
    );
}

#[test]
fn asyncio_run_awaits_result() {
    assert_eq!(
        g(
            "import asyncio\nasync def main():\n    await asyncio.sleep(0)\n    return 7\nr = asyncio.run(main())",
            "r"
        ),
        "7"
    );
}

#[test]
fn asyncio_gather_ordered_results() {
    assert_eq!(
        g(
            "import asyncio\nasync def sq(n):\n    await asyncio.sleep(0)\n    return n*n\nasync def main():\n    return await asyncio.gather(sq(1), sq(2), sq(3))\nr = asyncio.run(main())",
            "r"
        ),
        "[1, 4, 9]"
    );
}

#[test]
fn asyncio_create_task_and_future() {
    // A Task sets a Future's result; the main coroutine awaits the Future.
    assert_eq!(
        g(
            "import asyncio\nasync def setter(fut):\n    await asyncio.sleep(0)\n    fut.set_result(99)\nasync def main():\n    fut = asyncio.Future()\n    asyncio.create_task(setter(fut))\n    return await fut\nr = asyncio.run(main())",
            "r"
        ),
        "99"
    );
}

#[test]
fn await_exception_propagates() {
    assert_eq!(
        g(
            "import asyncio\nasync def boom():\n    await asyncio.sleep(0)\n    raise ValueError('nope')\nasync def main():\n    try:\n        await boom()\n    except ValueError as e:\n        return str(e)\nr = asyncio.run(main())",
            "r"
        ),
        "'nope'"
    );
}

#[test]
fn asyncio_sleep_timer_ordering() {
    // Timers fire in virtual-clock order regardless of scheduling order.
    assert_eq!(
        g(
            "import asyncio\nout = []\nasync def t(name, d):\n    await asyncio.sleep(d)\n    out.append(name)\nasync def main():\n    await asyncio.gather(t('slow', 0.2), t('fast', 0.1), t('mid', 0.15))\nasyncio.run(main())",
            "out"
        ),
        "['fast', 'mid', 'slow']"
    );
}

#[test]
fn async_for_custom_aiterator() {
    let src = "import asyncio\n\
class R:\n    def __init__(self, n):\n        self.n = n\n        self.i = 0\n    def __aiter__(self):\n        return self\n    async def __anext__(self):\n        if self.i >= self.n:\n            raise StopAsyncIteration\n        self.i += 1\n        await asyncio.sleep(0)\n        return self.i\n\
out = []\n\
async def main():\n    async for x in R(3):\n        out.append(x)\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[1, 2, 3]");
}

#[test]
fn async_with_context_manager() {
    let src = "import asyncio\n\
log = []\n\
class CM:\n    async def __aenter__(self):\n        log.append('enter')\n        return 5\n    async def __aexit__(self, *a):\n        log.append('exit')\n        return False\n\
async def main():\n    async with CM() as r:\n        log.append(r)\n\
asyncio.run(main())";
    assert_eq!(g(src, "log"), "['enter', 5, 'exit']");
}

#[test]
fn async_comprehension_list() {
    let src = "import asyncio\n\
class R:\n    def __init__(self, n):\n        self.n = n\n        self.i = 0\n    def __aiter__(self):\n        return self\n    async def __anext__(self):\n        if self.i >= self.n:\n            raise StopAsyncIteration\n        self.i += 1\n        await asyncio.sleep(0)\n        return self.i\n\
async def main():\n    return [x * x async for x in R(4)]\n\
r = asyncio.run(main())";
    assert_eq!(g(src, "r"), "[1, 4, 9, 16]");
}

#[test]
fn async_comprehension_filter_and_dict() {
    let src = "import asyncio\n\
class R:\n    def __init__(self, n):\n        self.n = n\n        self.i = 0\n    def __aiter__(self):\n        return self\n    async def __anext__(self):\n        if self.i >= self.n:\n            raise StopAsyncIteration\n        self.i += 1\n        return self.i\n\
async def main():\n    return {x: x * x async for x in R(4) if x % 2 == 0}\n\
r = asyncio.run(main())";
    assert_eq!(g(src, "r"), "{2: 4, 4: 16}");
}

#[test]
fn asyncio_event_wait_set() {
    let src = "import asyncio\n\
async def waiter(ev, out):\n    await ev.wait()\n    out.append('woke')\n\
out = []\n\
async def main():\n    ev = asyncio.Event()\n    t = asyncio.create_task(waiter(ev, out))\n    await asyncio.sleep(0)\n    out.append('set')\n    ev.set()\n    await t\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "['set', 'woke']");
}

#[test]
fn asyncio_lock_mutual_exclusion() {
    let src = "import asyncio\n\
out = []\n\
async def worker(lock, n):\n    async with lock:\n        out.append('in ' + str(n))\n        await asyncio.sleep(0)\n        out.append('out ' + str(n))\n\
async def main():\n    lock = asyncio.Lock()\n    await asyncio.gather(worker(lock, 1), worker(lock, 2))\n\
asyncio.run(main())";
    // The lock serializes the critical sections: 1 fully then 2 fully.
    assert_eq!(g(src, "out"), "['in 1', 'out 1', 'in 2', 'out 2']");
}

#[test]
fn asyncio_queue_producer_consumer() {
    let src = "import asyncio\n\
out = []\n\
async def producer(q):\n    for i in range(3):\n        await q.put(i)\n\
async def consumer(q):\n    for _ in range(3):\n        out.append(await q.get())\n\
async def main():\n    q = asyncio.Queue()\n    await asyncio.gather(producer(q), consumer(q))\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[0, 1, 2]");
}

#[test]
fn async_generator_comprehension() {
    let src = "import asyncio\n\
async def ag(n):\n    for i in range(n):\n        await asyncio.sleep(0)\n        yield i * i\n\
async def main():\n    return [x async for x in ag(4)]\n\
r = asyncio.run(main())";
    assert_eq!(g(src, "r"), "[0, 1, 4, 9]");
}

#[test]
fn async_generator_type_and_async_for() {
    let src = "import asyncio\n\
async def ag(n):\n    for i in range(n):\n        await asyncio.sleep(0)\n        yield i * 10\n\
out = []\n\
async def main():\n    async for v in ag(3):\n        out.append(v)\n    return type(ag(1)).__name__\n\
tn = asyncio.run(main())";
    assert_eq!(g(src, "out"), "[0, 10, 20]");
    assert_eq!(g(src, "tn"), "'async_generator'");
}

#[test]
fn task_cancel_caught_inside_coroutine() {
    // Cancelling a suspended Task injects CancelledError at its await point; the
    // coroutine's try/except runs, and returning normally leaves it un-cancelled.
    let src = "import asyncio\n\
out = []\n\
async def worker():\n    try:\n        await asyncio.sleep(10)\n        return 'no'\n    except asyncio.CancelledError:\n        return 'caught'\n\
async def main():\n    t = asyncio.create_task(worker())\n    await asyncio.sleep(0)\n    c = t.cancel()\n    r = await t\n    out.append(c)\n    out.append(r)\n    out.append(t.cancelled())\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[True, 'caught', False]");
}

#[test]
fn task_cancel_propagates_and_marks_cancelled() {
    // A coroutine that does not catch CancelledError becomes a cancelled Task:
    // awaiting it raises, and cancelled() is True.
    let src = "import asyncio\n\
out = []\n\
async def worker():\n    await asyncio.sleep(10)\n    return 'no'\n\
async def main():\n    t = asyncio.create_task(worker())\n    await asyncio.sleep(0)\n    t.cancel()\n    try:\n        await t\n        out.append('no-raise')\n    except asyncio.CancelledError:\n        out.append('raised')\n    out.append(t.cancelled())\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "['raised', True]");
}

#[test]
fn async_generator_asend_roundtrip() {
    // `asend(v)` resumes the body, `v` becoming the value of the `yield`
    // expression; exhaustion raises StopAsyncIteration.
    let src = "import asyncio\n\
async def ag():\n    a = yield 1\n    b = yield a + 1\n    yield b + 1\n\
out = []\n\
async def main():\n    g = ag()\n    out.append(await g.asend(None))\n    out.append(await g.asend(10))\n    out.append(await g.asend(20))\n    try:\n        await g.asend(0)\n    except StopAsyncIteration:\n        out.append('stop')\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[1, 11, 21, 'stop']");
}

#[test]
fn async_generator_athrow_caught() {
    // `athrow(exc)` raises at the current `yield`; a body that catches it and
    // yields again returns that next value.
    let src = "import asyncio\n\
out = []\n\
async def ag():\n    try:\n        while True:\n            yield 1\n    except ValueError:\n        yield 2\n\
async def main():\n    g = ag()\n    out.append(await g.asend(None))\n    out.append(await g.athrow(ValueError))\n    await g.aclose()\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[1, 2]");
}

#[test]
fn async_generator_aclose_finishes() {
    // `aclose()` raises GeneratorExit and drives the body to completion; a later
    // `asend` on the closed generator raises StopAsyncIteration.
    let src = "import asyncio\n\
out = []\n\
async def ag():\n    try:\n        yield 1\n        yield 2\n    finally:\n        out.append('cleanup')\n\
async def main():\n    g = ag()\n    out.append(await g.asend(None))\n    await g.aclose()\n    try:\n        await g.asend(None)\n    except StopAsyncIteration:\n        out.append('stop')\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[1, 'cleanup', 'stop']");
}

#[test]
fn asyncio_wait_for_timeout_and_success() {
    // `wait_for` raises TimeoutError past the deadline, and returns the result
    // when the awaitable finishes in time.
    let src = "import asyncio\n\
out = []\n\
async def slow():\n    await asyncio.sleep(10)\n    return 'slow'\n\
async def fast():\n    await asyncio.sleep(0)\n    return 'fast'\n\
async def main():\n    try:\n        await asyncio.wait_for(slow(), timeout=1)\n        out.append('no')\n    except asyncio.TimeoutError:\n        out.append('timeout')\n    out.append(await asyncio.wait_for(fast(), timeout=5))\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "['timeout', 'fast']");
}

#[test]
fn asyncio_bounded_queue_backpressure() {
    // A bounded Queue blocks `put` while full; the consumer drains it in order.
    let src = "import asyncio\n\
out = []\n\
async def main():\n    q = asyncio.Queue(maxsize=2)\n    async def prod():\n        for i in range(5):\n            await q.put(i)\n        await q.put(-1)\n    async def cons():\n        while True:\n            v = await q.get()\n            if v == -1:\n                break\n            out.append(v)\n            await asyncio.sleep(0)\n    await asyncio.gather(prod(), cons())\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[0, 1, 2, 3, 4]");
}

#[test]
fn asyncio_wait_first_completed() {
    // `wait(return_when=FIRST_COMPLETED)` settles as soon as one task finishes,
    // leaving the slower one pending.
    let src = "import asyncio\n\
out = []\n\
async def f(v, d):\n    await asyncio.sleep(d)\n    return v\n\
async def main():\n    t1 = asyncio.create_task(f(1, 3))\n    t2 = asyncio.create_task(f(2, 1))\n    done, pending = await asyncio.wait([t1, t2], return_when=asyncio.FIRST_COMPLETED)\n    out.append(len(done))\n    out.append(len(pending))\n    await asyncio.wait([t1, t2])\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[1, 1]");
}

/// `str.splitlines`: the full CPython line-boundary set (`\n \r \r\n \v \f \x1c
/// \x1d \x1e \x85    `), `\r\n` as one break, no trailing empty line,
/// and `keepends` retaining the boundary characters.
#[test]
fn str_splitlines_boundaries_and_keepends() {
    assert_eq!(g("x = 'a\\nb\\r\\nc'.splitlines()", "x"), "['a', 'b', 'c']");
    assert_eq!(
        g("x = 'a\\nb\\n'.splitlines(True)", "x"),
        "['a\\n', 'b\\n']"
    );
    assert_eq!(
        g("x = 'a\\rb\\r\\nc\\n'.splitlines(True)", "x"),
        "['a\\r', 'b\\r\\n', 'c\\n']"
    );
    // Vertical tab, form feed, and the C1/Unicode separators are all breaks.
    assert_eq!(
        g(
            "x = 'a\\x0bb\\x0cc\\x1cd\\x1ee\\x85f\\u2028g'.splitlines()",
            "x"
        ),
        "['a', 'b', 'c', 'd', 'e', 'f', 'g']"
    );
    // No trailing empty element for a terminal boundary; interior blank stays.
    assert_eq!(g("x = 'a\\n\\nb'.splitlines()", "x"), "['a', '', 'b']");
    assert_eq!(g("x = ''.splitlines()", "x"), "[]");
}

/// `str.casefold`: full Unicode folding, not just simple lowercasing — the
/// multi-character folds (`ß`->`ss`, titlecase digraphs) that `str.lower` misses.
#[test]
fn str_casefold_full_folding() {
    assert_eq!(g("x = 'Straße'.casefold()", "x"), "'strasse'");
    assert_eq!(g("x = 'ǅ'.casefold()", "x"), "'ǆ'"); // U+01C5 -> U+01C6
    assert_eq!(g("x = 'ﬀ'.casefold()", "x"), "'ff'"); // U+FB00 LATIN SMALL LIGATURE FF
                                                      // Ordinary text folds identically to lowercasing.
    assert_eq!(g("x = 'HELLO World'.casefold()", "x"), "'hello world'");
    // `lower` must NOT gain the full folds (ß stays ß).
    assert_eq!(g("x = 'Straße'.lower()", "x"), "'straße'");
}

/// `int.bit_count` / `int.bit_length` for native and bignum ints (ones and bit
/// width of the magnitude).
#[test]
fn int_bit_count_and_length() {
    assert_eq!(g("x = (255).bit_count()", "x"), "8");
    assert_eq!(g("x = (0).bit_count()", "x"), "0");
    assert_eq!(g("x = (-7).bit_count()", "x"), "3"); // magnitude of -7 is 0b111
    assert_eq!(g("x = (2**64 - 1).bit_count()", "x"), "64");
    assert_eq!(g("x = (2**100).bit_count()", "x"), "1");
    assert_eq!(g("x = (2**100).bit_length()", "x"), "101");
    assert_eq!(g("x = (0).bit_length()", "x"), "0");
}

/// `int.to_bytes` / `int.from_bytes`: byteorder, `signed` two's complement, the
/// default length/byteorder, and a bignum round-trip.
#[test]
fn int_to_from_bytes() {
    assert_eq!(g("x = (10).to_bytes(2, 'big')", "x"), "b'\\x00\\n'");
    assert_eq!(g("x = (258).to_bytes(2, 'little')", "x"), "b'\\x02\\x01'");
    assert_eq!(g("x = (5).to_bytes()", "x"), "b'\\x05'"); // defaults: length 1, big
    assert_eq!(g("x = (0).to_bytes(0, 'big')", "x"), "b''");
    assert_eq!(
        g("x = (-1).to_bytes(2, 'big', signed=True)", "x"),
        "b'\\xff\\xff'"
    );
    assert_eq!(g("x = int.from_bytes(b'\\x01\\x02', 'big')", "x"), "258");
    assert_eq!(
        g("x = int.from_bytes(b'\\xff\\xff', 'big', signed=True)", "x"),
        "-1"
    );
    assert_eq!(g("x = int.from_bytes([1, 0], 'big')", "x"), "256");
    // Bignum round-trips through its own byte width.
    assert_eq!(
        g(
            "n = 2**100\nx = int.from_bytes(n.to_bytes(13, 'big'), 'big') == n",
            "x"
        ),
        "True"
    );
}

/// `int.to_bytes` overflow / bad-argument errors match CPython's messages.
#[test]
fn int_to_bytes_errors() {
    let e = |src: &str| eval_str(src).unwrap_err();
    assert!(e("(-1).to_bytes(2, 'big')").contains("can't convert negative int to unsigned"));
    assert!(e("(256).to_bytes(1, 'big')").contains("int too big to convert"));
    assert!(e("(128).to_bytes(1, 'big', signed=True)").contains("int too big to convert"));
    assert!(e("(5).to_bytes(2, 'middle')").contains("byteorder must be either 'little' or 'big'"));
}

/// `float.as_integer_ratio` (exact rational) and `int.as_integer_ratio`.
#[test]
fn as_integer_ratio_exact() {
    assert_eq!(g("x = (0.5).as_integer_ratio()", "x"), "(1, 2)");
    assert_eq!(g("x = (0.0).as_integer_ratio()", "x"), "(0, 1)");
    assert_eq!(g("x = (-2.5).as_integer_ratio()", "x"), "(-5, 2)");
    assert_eq!(g("x = (10).as_integer_ratio()", "x"), "(10, 1)");
    // 0.1 is not exactly a tenth — its true binary ratio surfaces here.
    assert_eq!(
        g("x = (0.1).as_integer_ratio()", "x"),
        "(3602879701896397, 36028797018963968)"
    );
}

/// `float.hex` / `float.fromhex`: exact hex formatting and a bit-exact round trip.
#[test]
fn float_hex_and_fromhex() {
    assert_eq!(g("x = (3.14).hex()", "x"), "'0x1.91eb851eb851fp+1'");
    assert_eq!(g("x = (1.0).hex()", "x"), "'0x1.0000000000000p+0'");
    assert_eq!(g("x = (0.0).hex()", "x"), "'0x0.0p+0'");
    assert_eq!(g("x = (-0.0).hex()", "x"), "'-0x0.0p+0'");
    // Smallest positive subnormal.
    assert_eq!(g("x = (5e-324).hex()", "x"), "'0x0.0000000000001p-1022'");
    assert_eq!(g("x = float.fromhex('0x1.8p+1')", "x"), "3.0");
    assert_eq!(g("x = float.fromhex('  0X1P4  ')", "x"), "16.0"); // no dot, uppercase, ws
    assert_eq!(g("x = float.fromhex('-inf')", "x"), "-inf");
    // Round-trip preserves the exact bits.
    assert_eq!(g("x = float.fromhex((0.1).hex()) == 0.1", "x"), "True");
}

#[test]
fn numeric_dunder_methods_int() {
    // The round-2 gap: numeric dunders are now callable bound methods on int.
    assert_eq!(g("x = (5).__index__()", "x"), "5");
    assert_eq!(g("x = (-3).__abs__()", "x"), "3");
    assert_eq!(g("x = (7).__floordiv__(2)", "x"), "3");
    assert_eq!(g("x = (1).__add__(2)", "x"), "3");
    assert_eq!(g("x = (5).__mul__(3)", "x"), "15");
    assert_eq!(g("x = (5).__mod__(3)", "x"), "2");
    assert_eq!(g("x = (5).__pow__(3)", "x"), "125");
    assert_eq!(g("x = (5).__neg__()", "x"), "-5");
    assert_eq!(g("x = (5).__invert__()", "x"), "-6");
    assert_eq!(g("x = (5).__divmod__(3)", "x"), "(1, 2)");
    assert_eq!(g("x = (5).__and__(3)", "x"), "1");
    assert_eq!(g("x = (5).__lshift__(2)", "x"), "20");
    assert_eq!(g("x = (10).__truediv__(4)", "x"), "2.5");
    assert_eq!(g("x = (5).__int__()", "x"), "5");
    assert_eq!(g("x = (3).__float__()", "x"), "3.0");
    assert_eq!(g("x = (5).__round__(1)", "x"), "5");
    assert_eq!(g("x = (123).__round__(-1)", "x"), "120");
    assert_eq!(g("x = (5).__bool__()", "x"), "True");
    assert_eq!(g("x = (0).__bool__()", "x"), "False");
    // Reflected dunders compute `other OP self`.
    assert_eq!(g("x = (5).__radd__(2)", "x"), "7");
    assert_eq!(g("x = (5).__rsub__(2)", "x"), "-3");
    assert_eq!(g("x = (5).__rfloordiv__(2)", "x"), "0");
    // bool inherits int's dunders and normalizes to int.
    assert_eq!(g("x = True.__index__()", "x"), "1");
    assert_eq!(g("x = True.__add__(1)", "x"), "2");
}

#[test]
fn numeric_dunder_methods_float_and_notimplemented() {
    assert_eq!(g("x = (2.0).__round__()", "x"), "2");
    assert_eq!(g("x = (3.14159).__round__(2)", "x"), "3.14");
    assert_eq!(g("x = (5.0).__floordiv__(2)", "x"), "2.0");
    assert_eq!(g("x = (3.7).__floor__()", "x"), "3");
    assert_eq!(g("x = (3.7).__ceil__()", "x"), "4");
    assert_eq!(g("x = (3.5).__int__()", "x"), "3");
    // int declines a float operand (returns NotImplemented, not TypeError);
    // float accepts an int operand.
    assert_eq!(g("x = (5).__add__(2.0)", "x"), "NotImplemented");
    assert_eq!(g("x = (1).__eq__('x')", "x"), "NotImplemented");
    assert_eq!(g("x = (5).__eq__(5.0)", "x"), "NotImplemented");
    assert_eq!(g("x = (2.0).__lt__(3)", "x"), "True");
    assert_eq!(g("x = (2.0).__lt__('x')", "x"), "NotImplemented");
    assert_eq!(g("x = (1).__eq__(1)", "x"), "True");
    // A dunder that hits a zero divisor raises, mirroring the operator.
    let e = eval_str("x = (5).__mod__(0)").unwrap_err();
    assert!(
        e.contains("ZeroDivisionError: division by zero"),
        "got: {e}"
    );
}

#[test]
fn zero_division_messages_match_314() {
    // CPython 3.14 unified all these to the bare "division by zero".
    for expr in [
        "5 // 0",
        "5 % 0",
        "5.0 // 0.0",
        "5.0 % 0.0",
        "1 / 0",
        "divmod(5, 0)",
    ] {
        let e = eval_str(&format!("x = {expr}")).unwrap_err();
        assert!(
            e.contains("ZeroDivisionError: division by zero"),
            "{expr} -> {e}"
        );
    }
    // Zero to a negative power (int and float base word it identically in 3.14).
    let e = eval_str("x = 0 ** -1").unwrap_err();
    assert!(e.contains("zero to a negative power"), "got: {e}");
    let e = eval_str("x = 0.0 ** -1").unwrap_err();
    assert!(e.contains("zero to a negative power"), "got: {e}");
}

#[test]
fn sequence_index_and_concat_error_messages() {
    // Index-out-of-range names the sequence type (except bytes, which is bare).
    let e = eval_str("x = [][5]").unwrap_err();
    assert!(e.contains("list index out of range"), "got: {e}");
    let e = eval_str("x = (1, 2)[5]").unwrap_err();
    assert!(e.contains("tuple index out of range"), "got: {e}");
    let e = eval_str("x = bytearray(b'ab')[9]").unwrap_err();
    assert!(e.contains("bytearray index out of range"), "got: {e}");
    let e = eval_str("x = b'ab'[9]").unwrap_err();
    assert!(
        e.contains("IndexError: index out of range") && !e.contains("bytes index"),
        "got: {e}"
    );
    // Concatenating a sequence with a wrong-typed operand uses the type-specific
    // concat message, not the generic "unsupported operand type(s)" one.
    let e = eval_str("x = 'a' + 1").unwrap_err();
    assert!(
        e.contains("can only concatenate str (not \"int\") to str"),
        "got: {e}"
    );
    let e = eval_str("x = [1] + (2,)").unwrap_err();
    assert!(
        e.contains("can only concatenate list (not \"tuple\") to list"),
        "got: {e}"
    );
    let e = eval_str("x = (1,) + [2]").unwrap_err();
    assert!(
        e.contains("can only concatenate tuple (not \"list\") to tuple"),
        "got: {e}"
    );
    let e = eval_str("x = b'a' + 1").unwrap_err();
    assert!(e.contains("can't concat int to bytes"), "got: {e}");
    let e = eval_str("x = bytearray(b'a') + 1").unwrap_err();
    assert!(e.contains("can't concat int to bytearray"), "got: {e}");
    // A non-sequence left operand keeps the generic operand message.
    let e = eval_str("x = 5 + 'x'").unwrap_err();
    assert!(
        e.contains("unsupported operand type(s) for +: 'int' and 'str'"),
        "got: {e}"
    );
}

/// Collection literals whose stack-slot count exceeds the `CallBuiltin` u8 argc
/// cap (a 174-key dict literal in a real script raised "too many arguments
/// (>255) for one call"). The compiler now builds them in ≤255-slot chunks via
/// the `EXTEND_*` ops; verify each container type is correct at and around the
/// chunk boundaries (list/tuple/set/str-parts spill at >255, dict pairs at
/// >127). Values checked against CPython.
#[test]
fn large_collection_literals_exceed_u8_argc() {
    // 300-element list (spills once past the 255 mk-chunk).
    let lst = (0..300)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        g(
            &format!("a = [{lst}]\nx = (len(a), sum(a), a[0], a[-1])"),
            "x"
        ),
        "(300, 44850, 0, 299)"
    );

    // 300-element tuple (EXTEND_TUPLE rebuilds each chunk).
    assert_eq!(
        g(&format!("a = ({lst},)\nx = (len(a), sum(a), a[-1])"), "x"),
        "(300, 44850, 299)"
    );

    // 300-key dict literal — 600 stack slots, dict pairs spill past 127.
    let pairs = (0..300)
        .map(|i| format!("{i}: {}", i * i))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        g(
            &format!("d = {{{pairs}}}\nx = (len(d), sum(d.values()), d[0], d[299])"),
            "x"
        ),
        "(300, 8955050, 0, 89401)"
    );

    // Set literal with cross-chunk duplicates -> deduped (EXTEND_SET keying).
    let st = (0..300)
        .map(|i| (i % 250).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        g(&format!("s = {{{st}}}\nx = (len(s), sum(s))"), "x"),
        "(250, 31125)"
    );

    // f-string with 300 replacement fields spills EXTEND_STR; `{0}{1}...` are
    // integer-literal fields, so the result is "012...299".
    let fields = (0..300)
        .map(|i| format!("{{{i}}}"))
        .collect::<Vec<_>>()
        .concat();
    let expected: String = (0..300).map(|i| i.to_string()).collect();
    assert_eq!(
        g(&format!("x = f\"{fields}\""), "x"),
        format!("'{expected}'")
    );

    // Boundaries: exactly at, just over, and dict at its 127/128 pair edge.
    for n in [255usize, 256, 127, 128, 254] {
        let seq = (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
        let want = (n, n * (n.saturating_sub(1)) / 2);
        assert_eq!(
            g(&format!("a = [{seq}]\nx = (len(a), sum(a))"), "x"),
            format!("({}, {})", want.0, want.1),
            "list n={n}"
        );
        let dp = (0..n)
            .map(|i| format!("{i}: {i}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            g(&format!("d = {{{dp}}}\nx = (len(d), sum(d.values()))"), "x"),
            format!("({}, {})", want.0, want.1),
            "dict n={n}"
        );
    }
}

/// Attribute access directly on a float literal: `0.1.is_integer()` must lex as
/// `0.1` then `.is_integer` (a second `.` after the decimal point ends the
/// literal), not consume the dot into a malformed float. Regression for a
/// `SyntaxError: bad float` the lexer raised on this CPython-valid form.
#[test]
fn float_literal_attribute_access() {
    assert_eq!(g("x = 0.1.is_integer()", "x"), "False");
    assert_eq!(g("x = 2.0.is_integer()", "x"), "True");
    assert_eq!(g("x = 3.14.hex()", "x"), g("y = (3.14).hex()", "y"));
    // A float from an exponent also ends before a following dot.
    assert_eq!(g("x = 1e3.is_integer()", "x"), "True");
}

/// `type(x)` for values whose type is not a constructor builtin still reprs as
/// `<class '…'>`, not `<built-in function …>`. Regression: `type(None)` and
/// `type(len)` reported as built-in functions.
#[test]
fn type_object_repr() {
    assert_eq!(g("x = type(None)", "x"), "<class 'NoneType'>");
    assert_eq!(
        g("x = type(len)", "x"),
        "<class 'builtin_function_or_method'>"
    );
    assert_eq!(g("x = type(lambda: 0)", "x"), "<class 'function'>");
    assert_eq!(g("x = type(3)", "x"), "<class 'int'>");
    assert_eq!(g("x = type(int)", "x"), "<class 'type'>");
    assert_eq!(
        g("x = type(NotImplemented)", "x"),
        "<class 'NotImplementedType'>"
    );
    // A callable builtin still reprs as a function, not a class.
    assert_eq!(g("x = len", "x"), "<built-in function len>");
}

/// `sum()` uses Neumaier compensated summation for floats (CPython 3.12+), so
/// `sum([0.1]*10)` is exactly `1.0`, not `0.9999999999999999`. Also verifies the
/// exact integer prefix, mixed int/float, complex tail, and the str-start guard.
#[test]
fn sum_neumaier_and_paths() {
    assert_eq!(g("x = sum([0.1]*10)", "x"), "1.0");
    assert_eq!(g("x = sum([1e18, 1, -1e18])", "x"), "1.0");
    assert_eq!(g("x = sum([1, 2, 3])", "x"), "6");
    assert_eq!(g("x = sum([1, 2, 3.5])", "x"), "6.5");
    assert_eq!(g("x = sum([2**70, 1])", "x"), "1180591620717411303425");
    assert_eq!(g("x = sum([1, 2, complex(1, 1)])", "x"), "(4+1j)");
    let e = eval_str("x = sum(['a', 'b'], '')").unwrap_err();
    assert!(
        e.contains("sum() can't sum strings [use ''.join(seq) instead]"),
        "got: {e}"
    );
}

/// Non-finite floats format lowercase (`nan`/`inf`) for `f`/`e`/`g`/`%` and
/// uppercase (`NAN`/`INF`) for `F`/`E`/`G`, and still flow through width/sign/
/// zero-fill. Regression: `{nan:.2f}` rendered Rust's `NaN`.
#[test]
fn nonfinite_float_format() {
    assert_eq!(g("x = f'{float(\"nan\"):.2f}'", "x"), "'nan'");
    assert_eq!(g("x = f'{float(\"inf\"):f}'", "x"), "'inf'");
    assert_eq!(g("x = f'{float(\"-inf\"):.1f}'", "x"), "'-inf'");
    assert_eq!(g("x = f'{float(\"nan\"):.2F}'", "x"), "'NAN'");
    assert_eq!(g("x = f'{float(\"inf\"):E}'", "x"), "'INF'");
    assert_eq!(g("x = f'{float(\"nan\"):+g}'", "x"), "'+nan'");
    assert_eq!(g("x = f'{float(\"inf\"):%}'", "x"), "'inf%'");
    // Non-finite still honors width and zero-fill (CPython `00000inf`).
    assert_eq!(g("x = f'{float(\"inf\"):08.2f}'", "x"), "'00000inf'");
    assert_eq!(g("x = f'{float(\"nan\"):>8}'", "x"), "'     nan'");
}

/// A builtin exception class is a type object, so `repr(ValueError)` is
/// `<class 'ValueError'>`, not `<built-in function ValueError>`.
#[test]
fn exception_class_repr() {
    assert_eq!(g("x = ValueError", "x"), "<class 'ValueError'>");
    assert_eq!(g("x = KeyError", "x"), "<class 'KeyError'>");
    assert_eq!(g("x = Exception", "x"), "<class 'Exception'>");
    assert_eq!(g("x = type(ValueError)", "x"), "<class 'type'>");
}

/// Unbound builtin methods reached via a type object: `str.lower`, `list.append`,
/// `dict.get`. Callable with an explicit receiver (`str.lower("HI")`), usable as
/// a `key=`/`map` function, and repr as `<method '…' of '…' objects>`. Also the
/// bound-method `__name__`.
#[test]
fn unbound_builtin_methods() {
    assert_eq!(g("x = str.lower('HELLO')", "x"), "'hello'");
    assert_eq!(
        g("x = sorted(['B', 'a', 'C'], key=str.lower)", "x"),
        "['a', 'B', 'C']"
    );
    assert_eq!(g("x = list(map(str.upper, ['a', 'b']))", "x"), "['A', 'B']");
    assert_eq!(g("x = list.count([1, 1, 2], 1)", "x"), "2");
    assert_eq!(g("x = dict.get({'a': 1}, 'a')", "x"), "1");
    assert_eq!(g("x = str.upper", "x"), "<method 'upper' of 'str' objects>");
    // A bad attribute on a type object is still an AttributeError.
    assert!(eval_str("x = str.nonesuch").is_err());
    // Bound builtin method dunders.
    assert_eq!(g("x = [].append.__name__", "x"), "'append'");
    assert_eq!(g("x = [].append.__qualname__", "x"), "'list.append'");
}
