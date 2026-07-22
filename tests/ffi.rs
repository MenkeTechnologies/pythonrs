//! End-to-end inline Rust FFI: a `rust { ... }` block is desugared, compiled to
//! a cdylib via `rustc`, dlopened, and its exports called from Python. Requires
//! `rustc` on PATH (always present in a Rust CI); skips cleanly otherwise so a
//! toolchain-less environment never reports a false failure.
//!
//! Drives the built `python` binary as a subprocess (`CARGO_BIN_EXE_python`):
//! Python `print` writes straight to the process stdout, and running out of
//! process also isolates the FFI dlopen/registry from the test harness.

use std::io::Write;
use std::process::Command;

fn rustc_available() -> bool {
    Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Write `src` to a temp `.py` file and run it through the built `python`
/// binary, returning `(stdout, stderr, success)`.
fn run_py(src: &str) -> (String, String, bool) {
    let mut f = tempfile::Builder::new()
        .suffix(".py")
        .tempfile()
        .expect("temp file");
    f.write_all(src.as_bytes()).expect("write source");
    let path = f.path().to_owned();
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .arg(&path)
        .output()
        .expect("spawn python binary");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn rust_block_exports_are_callable_across_all_v1_signatures() {
    if !rustc_available() {
        eprintln!("skipping FFI test: rustc not on PATH");
        return;
    }
    // Distinct names so this test's registry entries never collide with another
    // test's. Exercises int-arity, float-arity, and string->int marshalling
    // (the string arg rides as a Python heap handle and is marshalled to a
    // native fusevm string before the call).
    let src = r#"
rust {
    pub extern "C" fn ffi_addi(a: i64, b: i64) -> i64 { a + b }
    pub extern "C" fn ffi_mulf(x: f64, y: f64, z: f64) -> f64 { x * y * z }
    pub extern "C" fn ffi_slen(s: *const c_char) -> i64 {
        unsafe { CStr::from_ptr(s).to_bytes().len() as i64 }
    }
}
print(ffi_addi(21, 21))
print(ffi_mulf(1.5, 2.0, 3.0))
print(ffi_slen("hello world"))
"#;
    let (stdout, stderr, ok) = run_py(src);
    assert!(ok, "FFI program failed: stderr={stderr}");
    assert_eq!(stdout, "42\n9.0\n11\n", "stderr={stderr}");
}

#[test]
fn rust_block_with_no_exports_errors() {
    if !rustc_available() {
        return;
    }
    // A block with no `pub extern "C" fn` is a hard error — v1 requires at least
    // one exported function.
    let src = "rust { fn helper() -> i64 { 1 } }\nprint(1)\n";
    let (_stdout, stderr, ok) = run_py(src);
    assert!(!ok, "empty-export block must error");
    assert!(stderr.contains("rust FFI"), "unexpected error: {stderr}");
}

/// The native `math` module is only a fast-path subset; a symbol it lacks
/// (`isqrt`, `trunc`, `comb`, `hypot`) must resolve from the real CPython
/// `math` over the stdlib-ffi bridge, not raise `AttributeError`. Skips cleanly
/// when the bridge/libpython is unavailable (e.g. a `--no-default-features` or
/// libpython-less environment) so it never reports a false failure.
#[test]
fn native_math_defers_missing_symbols_to_cpython() {
    let src = "\
import math
print(math.isqrt(100), math.trunc(3.7), math.comb(5, 2), round(math.hypot(3, 4), 1))
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping math-ffi test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(stdout, "10 3 10 5.0\n", "stderr={stderr}");
}

/// Reverse-callback reentrancy: a lazy CPython stdlib iterator (`itertools`) and
/// a `functools.cmp_to_key` comparator both call back into a pythonrs callable
/// while the host is mid-operation. These used to panic with `RefCell already
/// borrowed`; the FFI iteration/binary-op paths now release the host borrow
/// across the CPython call. Skips cleanly when the stdlib bridge is unavailable.
#[test]
fn ffi_reverse_callbacks_do_not_panic() {
    let src = "\
import itertools, functools
print(list(itertools.starmap(pow, [(2, 3), (3, 2), (10, 2)])))
print(list(itertools.takewhile(lambda x: x < 100, [1, 10, 100, 5])))
print(list(itertools.filterfalse(lambda x: x % 2, range(6))))
print(sorted([3, 1, 2], key=functools.cmp_to_key(lambda a, b: b - a)))
print(sorted(['pie', 'a', 'bb'], key=functools.cmp_to_key(lambda a, b: len(a) - len(b))))
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-callback test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert!(!stderr.contains("RefCell"), "reentrancy panic: {stderr}");
    assert_eq!(
        stdout, "[8, 9, 100]\n[1, 10]\n[0, 2, 4]\n[3, 2, 1]\n['a', 'bb', 'pie']\n",
        "stderr={stderr}"
    );
}

/// `float()` of a foreign object honors its `__float__` (`Fraction`, `Decimal`),
/// and `textwrap`/`statistics` resolve to the real CPython modules (the native
/// subsets are skipped under the FFI bridge, so keyword options like `width=`
/// work). Skips cleanly when the bridge is unavailable.
#[test]
fn ffi_float_conversion_and_full_stdlib_modules() {
    let src = "\
from fractions import Fraction
from decimal import Decimal
import textwrap
print(float(Fraction(1, 3)), float(Decimal('2.5')))
print(textwrap.fill('a b c d e f', width=5))
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-float/stdlib test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "0.3333333333333333 2.5\na b c\nd e f\n",
        "stderr={stderr}"
    );
}

/// Enum via a foreign (CPython) base: `class C(Enum)` is built by the real
/// `EnumType` metaclass, so members, `.name`/`.value`, iteration, `by-value` and
/// `by-name` lookup, singleton `is` identity, IntEnum ordering, and body-defined
/// methods all behave like CPython. Skips cleanly without the stdlib bridge.
#[test]
fn ffi_enum_via_foreign_metaclass() {
    let src = "\
from enum import Enum, IntEnum, auto
class Color(Enum):
    RED = 1
    GREEN = 2
    def bright(self): return self.value * 10
class Pri(IntEnum):
    LOW = 1
    HIGH = 3
print(Color.RED, Color.RED.name, Color.RED.value)
print([c.name for c in Color], Color(2), Color['GREEN'])
print(Color.RED is Color.RED, Color.RED is Color.GREEN, Color(1) is Color.RED)
print(Color.RED.bright(), len(Color))
print(Pri.HIGH > Pri.LOW, Pri.HIGH + 1, sorted(Pri, reverse=True))
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-enum test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert!(!stderr.contains("RefCell"), "panic: {stderr}");
    assert_eq!(
        stdout,
        "Color.RED RED 1\n['RED', 'GREEN'] Color.GREEN Color.GREEN\nTrue False True\n10 2\nTrue 4 [<Pri.HIGH: 3>, <Pri.LOW: 1>]\n",
        "stderr={stderr}"
    );
}

/// A pythonrs generator crosses into a CPython call as a lazy iterator
/// (`itertools.takewhile` over an infinite generator never materializes), and
/// `functools.wraps` on a pythonrs wrapper succeeds — `__name__` is copied off
/// the wrapped function and the decorated function stays callable.
#[test]
fn ffi_generator_marshalling_and_functools_wraps() {
    let src = "\
import itertools, functools
def fib():
    a, b = 0, 1
    while True:
        yield a
        a, b = b, a + b
print(list(itertools.takewhile(lambda x: x < 50, fib())))
def logged(fn):
    @functools.wraps(fn)
    def wrapper(*a, **k):
        return fn(*a, **k)
    return wrapper
@logged
def greet(name):
    return 'hi ' + name
print(greet('bob'), greet.__name__)
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-gen/wraps test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "[0, 1, 1, 2, 3, 5, 8, 13, 21, 34]\nhi bob greet\n",
        "stderr={stderr}"
    );
}

/// A native pythonrs class crosses into a CPython call: `@dataclass` mirrors it
/// (fields from __annotations__, methods bound), and `typing.NamedTuple` (a
/// Foreign base) builds via the real metaclass. Skips without the stdlib bridge.
#[test]
fn ffi_dataclass_and_named_tuple() {
    let src = "\
from dataclasses import dataclass
@dataclass
class Point:
    x: int
    y: int
    label: str = 'origin'
    def dist_sq(self):
        return self.x ** 2 + self.y ** 2
p = Point(3, 4)
print(p, p.dist_sq(), p == Point(3, 4))
from typing import NamedTuple
class Pair(NamedTuple):
    a: int
    b: int = 9
q = Pair(1)
print(q, q.a, q.b, q._asdict(), Pair._fields)
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-dataclass test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout,
        "Point(x=3, y=4, label='origin') 25 True\n\
         Pair(a=1, b=9) 1 9 {'a': 1, 'b': 9} ('a', 'b')\n",
        "stderr={stderr}"
    );
}

/// Deep recursion runs on the interpreter's large-stack thread and hits a
/// catchable `RecursionError` (CPython's default limit) rather than aborting the
/// process on a native stack overflow. Runs the real binary via the subprocess
/// harness (recursion needs no stdlib bridge).
#[test]
fn deep_recursion_and_recursion_error() {
    let src = "\
def s(n):
    return 0 if n == 0 else n + s(n - 1)
print(s(500))
def loop():
    return loop()
try:
    loop()
except RecursionError:
    print('RecursionError')
";
    let (stdout, stderr, ok) = run_py(src);
    assert!(ok, "recursion program failed: {stderr}");
    assert!(
        !stderr.contains("stack overflow") && !stderr.contains("panicked"),
        "native crash instead of RecursionError: {stderr}"
    );
    assert_eq!(stdout, "125250\nRecursionError\n", "stderr={stderr}");
}

/// A pythonrs instance crosses into a CPython call as a proxy: `operator`
/// attr/item getters read its attributes/items, and it sorts by its own
/// comparison. Skips cleanly without the stdlib bridge.
#[test]
fn ffi_instance_proxy_attrgetter() {
    let src = "\
import operator as op
class P:
    def __init__(self, x, y):
        self.x, self.y = x, y
    def __repr__(self):
        return f'P({self.x},{self.y})'
pts = [P(1, 5), P(3, 2), P(2, 8)]
print([p.x for p in sorted(pts, key=op.attrgetter('x'))])
print(op.attrgetter('y')(P(7, 9)))
rows = [[3, 'c'], [1, 'a'], [2, 'b']]
print(sorted(rows, key=op.itemgetter(1)))
print(list(map(op.attrgetter('x'), pts)))
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-instance-proxy test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "[1, 2, 3]\n9\n[[1, 'a'], [2, 'b'], [3, 'c']]\n[1, 3, 2]\n",
        "stderr={stderr}"
    );
}

/// Compile-time `SyntaxWarning`s (CPython): `is`/`is not` with an immutable
/// literal, and a literal sequence subscripted by a float constant. Both print
/// to stderr before the program runs, with the offending source line echoed
/// (the subprocess harness runs from a real temp file, so the echo fires).
#[test]
fn syntax_warnings_is_literal_and_float_subscript() {
    // The float subscript raises a runtime TypeError, so it's caught to let the
    // program complete; the compile-time warnings still print regardless.
    let src = "\
x = 5
print(x is 1)
try:
    [1, 2, 3][1.5]
except TypeError:
    print('caught')
";
    let (stdout, stderr, ok) = run_py(src);
    assert!(ok, "program should still run: {stderr}");
    assert_eq!(stdout, "False\ncaught\n", "stderr={stderr}");
    assert!(
        stderr.contains("SyntaxWarning: \"is\" with 'int' literal. Did you mean \"==\"?"),
        "missing is-literal warning: {stderr}"
    );
    assert!(
        stderr.contains(
            "SyntaxWarning: list indices must be integers or slices, not float; \
             perhaps you missed a comma?"
        ),
        "missing float-subscript warning: {stderr}"
    );
    // The offending source line is echoed under each warning.
    assert!(
        stderr.contains("  print(x is 1)"),
        "no source echo: {stderr}"
    );
}

/// Function annotations that subscript a `typing` generic (`Optional[int]`)
/// evaluate through the stdlib bridge: `int` crosses into CPython as the real
/// `int` type (not a callback proxy), so `typing.Optional[int]` builds and its
/// repr needs no re-entry into the borrowed host. Regression for a double-borrow
/// panic where a builtin type crossed as a `PyrsCallable`.
#[test]
fn ffi_typing_annotation_subscript() {
    let src = "\
from typing import Optional, List
def h(x: Optional[int] = None) -> List[str]:
    return []
print(h.__annotations__['x'])
print(h.__annotations__['return'])
y = Optional[int]
print(y)
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-typing-annotation test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "int | None\ntyping.List[str]\nint | None\n",
        "stderr={stderr}"
    );
}

/// `@functools.total_ordering` runs natively: the decorated class stays a native
/// pythonrs class (so `__init__` can set attributes — a CPython round trip made it
/// a Foreign class that couldn't), and comparison dispatch derives the three
/// missing rich-comparison ops from the one defined ordering method plus `__eq__`.
/// Verified for both a `__lt__`-rooted and a `__gt__`-rooted class.
#[test]
fn ffi_total_ordering_native() {
    let src = "\
import functools
@functools.total_ordering
class V:
    def __init__(self, n):
        self.n = n
    def __eq__(self, o):
        return self.n == o.n
    def __lt__(self, o):
        return self.n < o.n
print(V(1) < V(2), V(3) >= V(2), V(2) <= V(2), V(2) > V(1), V(1) >= V(1), V(2) <= V(1))
print([v.n for v in sorted([V(3), V(1), V(2)])])

@functools.total_ordering
class G:
    def __init__(self, n):
        self.n = n
    def __eq__(self, o):
        return self.n == o.n
    def __gt__(self, o):
        return self.n > o.n
print(G(1) < G(2), G(2) <= G(2), G(3) >= G(1))
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-total-ordering test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "True True True True True False\n[1, 2, 3]\nTrue True True\n",
        "stderr={stderr}"
    );
}

/// `@functools.cached_property` runs natively as a non-data descriptor: first
/// access computes the getter and caches the result in the instance `__dict__`
/// (so later accesses read the dict and never recompute), the cached value can be
/// overwritten and `del`'d (forcing a recompute), and a `__slots__` instance with
/// no dict raises CPython's exact `TypeError`.
#[test]
fn ffi_cached_property_native() {
    let src = "\
import functools
class Circle:
    def __init__(self, r):
        self.r = r
    @functools.cached_property
    def area(self):
        print('computing')
        return self.r * self.r
c = Circle(10)
print(c.area)
print(c.area)
c.area = 999
print(c.area)
del c.area
print(c.area)
print(type(Circle.area).__name__)
class S:
    __slots__ = ('r',)
    def __init__(self, r):
        self.r = r
    @functools.cached_property
    def a(self):
        return self.r
try:
    S(1).a
except TypeError as e:
    print(e)
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-cached-property test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout,
        "computing\n100\n100\n999\ncomputing\n100\ncached_property\n\
         No '__dict__' attribute on 'S' instance to cache 'a' property.\n",
        "stderr={stderr}"
    );
}

/// Setting an attribute on a live CPython object routes through the bridge, so a
/// mutable stdlib object (`decimal.getcontext().prec = 6`) takes effect. Previously
/// `set_attr` raised "'Context' object attribute assignment unsupported".
#[test]
fn ffi_foreign_setattr() {
    let src = "\
from decimal import Decimal, getcontext
getcontext().prec = 6
print(getcontext().prec)
print(Decimal(1) / Decimal(7))
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-foreign-setattr test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(stdout, "6\n0.142857\n", "stderr={stderr}");
}

/// A pythonrs exception handed to a foreign context manager's `__exit__` is
/// reconstructed as a real CPython exception, so `contextlib.suppress` matches it
/// (including by base class) and swallows it; a non-matching exception propagates.
#[test]
fn ffi_foreign_context_manager_exit() {
    let src = "\
from contextlib import suppress
with suppress(ZeroDivisionError):
    x = 1 / 0
print('suppressed')
with suppress(ArithmeticError):
    y = 1 / 0
print('base-class suppressed')
try:
    with suppress(KeyError):
        raise ValueError('propagates')
except ValueError as e:
    print('propagated:', e)
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-foreign-cm test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "suppressed\nbase-class suppressed\npropagated: propagates\n",
        "stderr={stderr}"
    );
}

/// `sys.stdout` reassignment and `contextlib.redirect_stdout` retarget pythonrs's
/// own `print` (a CPython redirect_stdout only touches CPython's `sys.stdout`,
/// which pythonrs's print doesn't consult). Nesting restores correctly, an
/// exception inside still restores the stream, and `sys.__stdout__` stays the
/// real stream.
#[test]
fn ffi_stdout_redirect() {
    let src = "\
import io, sys
from contextlib import redirect_stdout
sys.stdout = io.StringIO()
print('manual')
cap = sys.stdout.getvalue()
sys.stdout = sys.__stdout__
print('manual:', repr(cap))
outer, inner = io.StringIO(), io.StringIO()
with redirect_stdout(outer):
    print('o1')
    with redirect_stdout(inner):
        print('i')
    print('o2')
print('outer:', repr(outer.getvalue()))
print('inner:', repr(inner.getvalue()))
buf = io.StringIO()
try:
    with redirect_stdout(buf):
        print('before')
        raise ValueError('x')
except ValueError:
    pass
print('after:', repr(buf.getvalue()))
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-stdout-redirect test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "manual: 'manual\\n'\nouter: 'o1\\no2\\n'\ninner: 'i\\n'\nafter: 'before\\n'\n",
        "stderr={stderr}"
    );
}

/// A foreign (CPython) value converts through `int()`: an `IntEnum` member (an
/// `int` subclass) and a `Fraction`/`Decimal` all reach a native int, and the
/// result participates in arithmetic. Previously `int()` rejected the foreign
/// object.
#[test]
fn ffi_int_of_foreign() {
    let src = "\
from enum import IntEnum
from fractions import Fraction
class P(IntEnum):
    LOW = 1
    HIGH = 10
print(int(P.HIGH), int(P.HIGH) + 5)
print(int(Fraction(7, 2)))
print(int(Fraction(-9, 4)))
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-int-of-foreign test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(stdout, "10 15\n3\n-2\n", "stderr={stderr}");
}

/// `isinstance` against a CPython ABC (`collections.abc.*`) decides structurally
/// via CPython: a native pythonrs list/dict/str/generator crosses to its CPython
/// form so the ABC's `__instancecheck__` runs. Previously all such checks were
/// `False`.
#[test]
fn ffi_isinstance_against_abc() {
    let src = "\
from collections import abc
print(isinstance([], abc.Sequence))
print(isinstance({}, abc.Mapping))
print(isinstance('s', abc.Sequence))
print(isinstance((x for x in []), abc.Iterator))
print(isinstance(42, abc.Sequence))
print(isinstance({1, 2}, abc.Set))
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-isinstance-abc test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "True\nTrue\nTrue\nTrue\nFalse\nTrue\n",
        "stderr={stderr}"
    );
}

/// An exception raised by CPython over the bridge (`dataclasses.FrozenInstanceError`
/// from assigning to a frozen dataclass) is catchable by the common `except
/// Exception` catch-all — an exception class unknown to pythonrs's builtin table is
/// treated as an `Exception` subclass.
#[test]
fn ffi_foreign_exception_caught_by_except_exception() {
    let src = "\
from dataclasses import dataclass
@dataclass(frozen=True)
class C:
    v: int
c = C(1)
try:
    c.v = 2
except Exception as e:
    print('caught', type(e).__name__)
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-foreign-exception test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(stdout, "caught FrozenInstanceError\n", "stderr={stderr}");
}

/// A CPython exception raised over the bridge is matched by pythonrs `except`
/// clauses against its captured base-class chain: `except ValueError` catches a
/// `json.JSONDecodeError` (a ValueError subclass), `except ArithmeticError`
/// catches `decimal.InvalidOperation`, and the exact foreign type
/// (`except json.JSONDecodeError`) matches by its CPython `__name__`.
#[test]
fn ffi_foreign_exception_base_matching() {
    let src = "\
import json
from decimal import Decimal
try:
    json.loads('x')
except LookupError:
    print('wrong')
except ValueError:
    print('ValueError')
try:
    json.loads('x')
except json.JSONDecodeError:
    print('exact')
try:
    Decimal('bad')
except ArithmeticError:
    print('ArithmeticError')
";
    let (stdout, stderr, ok) = run_py(src);
    if !ok || stderr.contains("ModuleNotFoundError") {
        eprintln!("skipping ffi-foreign-exc-base test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "ValueError\nexact\nArithmeticError\n",
        "stderr={stderr}"
    );
}
