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

/// Whether `rustc` is genuinely ABSENT — as opposed to present and failing.
///
/// The old spelling was `.output().map(|o| o.status.success()).unwrap_or(false)`,
/// so EVERY failure mode collapsed into "not available": a `rustc` that exists
/// but exits non-zero, a broken toolchain, a spawn refused by the OS. Each of
/// those turned the two `rust { … }` tests below into a silent pass with zero
/// assertions executed — the same vacuous-pass shape round 5 removed from the
/// stdlib-bridge guard. Only `NotFound` is a skip now; anything else is a
/// toolchain the tests are entitled to depend on.
fn rustc_available() -> bool {
    let out = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
        .arg("--version")
        .output();
    match out {
        Ok(o) => {
            assert!(
                o.status.success(),
                "`rustc --version` exited {:?} — the toolchain is present but broken, \
                 which is a failure rather than a reason to skip\nstderr:\n{}",
                o.status.code(),
                String::from_utf8_lossy(&o.stderr)
            );
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => panic!("spawning rustc failed with {e:?}, which is not `not found`"),
    }
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

/// Whether the CPython stdlib bridge is genuinely ABSENT — as opposed to present
/// and broken.
///
/// Every bridge test in this file guards itself so a libpython-less environment
/// reports a skip rather than a false failure. That guard used to read
/// `!ok || stderr.contains("ModuleNotFoundError")`. `ok` is `status.success()`,
/// so the `||` made ANY non-zero exit a skip: an uncaught `TypeError` from a
/// regressed conversion, a wrong-arity marshalling error, a segfault — each
/// reported green. Worst of all, the `assert!(!stderr.contains("RefCell"))`
/// lines written specifically to catch a double-borrow panic in the bridge could
/// never fire, because the panicking child exits non-zero and the guard returned
/// before reaching them. If `int(Fraction(7, 2))` started raising, the test that
/// measures it passed.
///
/// A skip now requires BOTH a failure AND an import-shaped diagnostic. A bridge
/// that exists but misbehaves fails the test that measures it. The `SyntaxError`
/// arm is the native `--no-default-features` build, where a vendored `pylib/`
/// module that does not yet parse on pythonrs is the reason the import failed;
/// it is scoped to a diagnostic that names `pylib` so a syntax error in a test's
/// OWN source is still a failure.
fn bridge_unavailable(ok: bool, stderr: &str) -> bool {
    !ok && (stderr.contains("ModuleNotFoundError")
        || stderr.contains("ImportError")
        || (stderr.contains("SyntaxError") && stderr.contains("pylib")))
}

/// The bridge tests below may each skip themselves; this one may not. Without it
/// a change that broke the bridge outright would turn every test in this file
/// into a silent skip and the file would still report all-green.
///
/// Asserts the opposite of what the guards test for: on a `stdlib-ffi` build the
/// bridge IS available, so no test in this file is entitled to skip.
#[test]
#[cfg(feature = "stdlib-ffi")]
fn the_stdlib_bridge_is_available_so_the_guards_below_cannot_all_skip() {
    let src = "\
import itertools, functools, enum, dataclasses, textwrap, statistics
print('bridge', itertools.__name__, len(dataclasses.__name__))
";
    let (stdout, stderr, ok) = run_py(src);
    assert!(
        !bridge_unavailable(ok, &stderr),
        "stdlib-ffi build reports the bridge unavailable — every guarded test in \
         this file would skip and the file would still pass\nstderr:\n{stderr}"
    );
    assert!(ok, "bridge sentinel exited non-zero\nstderr:\n{stderr}");
    assert_eq!(stdout, "bridge itertools 11\n", "stderr={stderr}");
}

/// The two `rust { … }` tests below skip themselves when `rustc` is missing;
/// this one may not. Without it, a `rustc` that has gone missing turns both into
/// silent zero-assertion passes and the file still reports all-green — the exact
/// shape the round-5 bridge sentinel was added to prevent, left uncovered on the
/// toolchain guard.
///
/// `cargo test` cannot have compiled this binary without a Rust compiler, so an
/// uninvokable `rustc` is a broken environment, not a supported configuration.
/// Set `RUSTC` if the compiler is not on `PATH` under that name.
#[test]
fn the_rust_toolchain_is_present_so_the_two_guards_below_cannot_silently_skip() {
    assert!(
        rustc_available(),
        "`rustc` is not invokable, so `rust_block_exports_are_callable_across_all_v1_signatures` \
         and `rust_block_with_no_exports_errors` would both pass having asserted nothing. \
         Point RUSTC at the compiler that built this test binary."
    );
}

/// The mirror of `the_stdlib_bridge_is_available_so_the_guards_below_cannot_all_skip`
/// for the build that has no bridge. On `--no-default-features` that sentinel is
/// compiled out, so every `bridge_unavailable` guard in this file may skip and
/// nothing checks that the skips are DESERVED. This asserts the bridge is
/// genuinely absent — a native build that somehow imports `dataclasses` means
/// the guards are hiding real coverage rather than standing in for it.
#[test]
#[cfg(not(feature = "stdlib-ffi"))]
fn the_native_build_really_has_no_bridge_so_the_guards_below_are_deserved() {
    let src = "import dataclasses\nprint('bridge')\n";
    let (stdout, stderr, ok) = run_py(src);
    assert!(
        bridge_unavailable(ok, &stderr),
        "a --no-default-features build imported `dataclasses` (stdout={stdout:?}) — the \
         bridge guards in this file are skipping tests that could have run\nstderr:\n{stderr}"
    );
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping ffi-typing-annotation test: stdlib bridge unavailable ({stderr})");
        return;
    }
    // How `Optional[int]` reprs is a CPython detail, not something pythonrs
    // decides: 3.14 renders it `int | None`, 3.13 and earlier
    // `typing.Optional[int]`. The bridge passes whichever through verbatim, so
    // accept both spellings — what this test pins is that the annotation
    // survives the round trip at all, and that `List[str]` keeps its own repr.
    let optional = ["int | None", "typing.Optional[int]"]
        .into_iter()
        .find(|s| stdout.starts_with(s))
        .unwrap_or_else(|| panic!("unexpected Optional repr in {stdout:?} (stderr={stderr})"));
    assert_eq!(
        stdout,
        format!("{optional}\ntyping.List[str]\n{optional}\n"),
        "stderr={stderr}"
    );
}

/// A native PEP 585 alias (`tuple[int, bool]`) or PEP 604 union (`int | str`)
/// subscripting a CPython `typing` generic crosses the bridge as the real
/// `types.GenericAlias` / `typing.Union`. Regression: the conversion had no arm
/// for either, so `def f() -> Optional[tuple[int, int]]` died at definition time
/// with "cannot pass 'GenericAlias' to a CPython stdlib call".
#[test]
fn ffi_typing_subscript_native_generic_alias_and_union() {
    let src = "\
from typing import Optional, List
def f(a: dict[str, list[str]]) -> Optional[tuple[int, bool]]:
    return (1, True)
print(f({}))
print(f.__annotations__['return'])
print(List[tuple[int, int | None]])
print(Optional[int | str])
print(type(tuple[int, bool]))
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping ffi-generic-alias test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout,
        "(1, True)\n\
         tuple[int, bool] | None\n\
         typing.List[tuple[int, int | None]]\n\
         int | str | None\n\
         <class 'types.GenericAlias'>\n",
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
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
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping ffi-foreign-exc-base test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "ValueError\nexact\nArithmeticError\n",
        "stderr={stderr}"
    );
}

/// Setting an attribute on a frozen dataclass that also defines a method must
/// raise FrozenInstanceError, not panic. Regression: the ffi error path
/// registered the exception's base chain via a fresh host borrow while the
/// set_attr path already held it (double borrow).
#[test]
fn ffi_frozen_dataclass_setattr_no_panic() {
    let src = "\
from dataclasses import dataclass
@dataclass(frozen=True)
class RGB:
    r: int
    g: int
    def hex(self):
        return self.r
c = RGB(1, 2)
try:
    c.r = 9
    print('assigned')
except Exception as e:
    print('frozen', type(e).__name__)
";
    let (stdout, stderr, ok) = run_py(src);
    // This exercises the CPython `dataclasses` bridge (the `stdlib-ffi` build).
    // In the native `--no-default-features` build `dataclasses` resolves from the
    // vendored `pylib/dataclasses.py`, which does not yet fully parse/run on
    // pythonrs — so the import fails cleanly (ModuleNotFoundError / SyntaxError /
    // ImportError). Skip in that case: the frozen-setattr behavior needs a working
    // `dataclasses`, and the DEFAULT build (where dataclasses imports from real
    // CPython) still enforces the assertions below. The no-panic invariant is
    // still checked whenever the class is constructed.
    if bridge_unavailable(ok, &stderr) {
        eprintln!(
            "skipping ffi-frozen-setattr test: dataclasses unavailable on this build ({stderr})"
        );
        return;
    }
    assert!(!stderr.contains("RefCell"), "double-borrow panic: {stderr}");
    assert_eq!(stdout, "frozen FrozenInstanceError\n", "stderr={stderr}");
}

/// `match` class patterns match a `@dataclass` instance — the class is a foreign
/// (CPython) mirror, so the match must use CPython isinstance and read
/// `__match_args__` over the bridge. Both positional and keyword sub-patterns
/// (with literals and captures) work.
#[test]
fn ffi_match_dataclass_class_pattern() {
    let src = "\
from dataclasses import dataclass
@dataclass
class Point:
    x: int
    y: int
def classify(p):
    match p:
        case Point(0, 0):
            return 'origin'
        case Point(x=0, y=v):
            return f'y-axis {v}'
        case Point(a, b):
            return f'point {a},{b}'
        case _:
            return 'other'
print(classify(Point(0, 0)))
print(classify(Point(0, 7)))
print(classify(Point(2, 4)))
print(classify(42))
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping ffi-match-dataclass test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "origin\ny-axis 7\npoint 2,4\nother\n",
        "stderr={stderr}"
    );
}

// ── the exception boundary between user code and the stdlib ─────────────────
//
// Values crossing the bridge as EXCEPTIONS lose information that crossing as
// data does not. The bridge can only hand a fusevm abort a rendered
// `"Class: message"` line, so rebuilding the exception on the other side assumes
// `str(exc) == args[0]` and that `args` is all there is — neither holds in
// general. Each test below pins one such loss, and each asserts on `args` /
// attributes rather than on the message: the message is the part that already
// agreed while the exception was still wrong.

/// A `KeyError` raised by a stdlib mapping keeps the KEY in `args`, not the
/// key's repr. `KeyError.__str__` is `repr(args[0])`, so an implementation that
/// rebuilds the exception by re-parsing its own rendering (`KeyError: 'X'`)
/// produces `KeyError("'X'")`: `e.args` holds the repr and `str(e)` gains a
/// quote layer every time the exception makes the trip.
#[test]
fn ffi_stdlib_keyerror_keeps_the_key_in_args() {
    let src = "\
import os
try:
    os.environ['PYTHONRS_DEFINITELY_NOT_SET_XYZ']
except KeyError as e:
    print(type(e).__name__, e.args, repr(str(e)))
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping ffi-keyerror test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout,
        "KeyError ('PYTHONRS_DEFINITELY_NOT_SET_XYZ',) \"'PYTHONRS_DEFINITELY_NOT_SET_XYZ'\"\n",
        "stderr={stderr}"
    );
}

/// A stdlib exception's attributes beyond `args` survive the crossing.
/// `except json.JSONDecodeError as e: e.lineno` is the standard way to locate a
/// parse failure, and none of `lineno`/`colno`/`pos`/`msg`/`doc` appears in the
/// rendered line — reconstructing the exception from that line alone left the
/// idiom raising `AttributeError` inside the handler.
#[test]
fn ffi_stdlib_exception_keeps_attributes_outside_args() {
    let src = "\
import json
try:
    json.loads('{\"a\": }')
except ValueError as e:
    print(type(e).__name__, e.lineno, e.colno, e.pos, e.msg)
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping ffi-json-attrs test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "JSONDecodeError 1 7 6 Expecting value\n",
        "stderr={stderr}"
    );
}

/// `@contextlib.contextmanager` drives a pythonrs generator through the CPython
/// generator protocol: `__exit__` calls `gen.throw(exc)` to give the body's
/// exception to the `except` around the `yield`, and reads a `StopIteration`
/// back as "handled". Without `throw` on the wrapper, every `with cm():` whose
/// body raised died with `AttributeError: 'builtins.PyrsIterator' object has no
/// attribute 'throw'` — from inside `contextlib`, nowhere near the user's code.
#[test]
fn ffi_contextmanager_throws_into_a_pythonrs_generator() {
    let src = "\
import contextlib
@contextlib.contextmanager
def cm():
    print('enter')
    try:
        yield 7
    except ValueError as e:
        print('handled', e.args)
    finally:
        print('exit')
with cm() as v:
    print('body', v)
    raise ValueError('inner')
print('after')
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping ffi-contextmanager-throw test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "enter\nbody 7\nhandled ('inner',)\nexit\nafter\n",
        "stderr={stderr}"
    );
}

/// The other two outcomes of that same `gen.throw`: a context manager that does
/// NOT catch lets the exception through unchanged, and one that raises a
/// different exception replaces it — with the new class and args intact after
/// the round trip out through `contextlib` and back into pythonrs.
#[test]
fn ffi_contextmanager_propagates_and_translates_exceptions() {
    let src = "\
import contextlib
@contextlib.contextmanager
def passthrough():
    try:
        yield
    finally:
        print('cleanup')
@contextlib.contextmanager
def translate():
    try:
        yield
    except ValueError:
        raise KeyError('replaced')
try:
    with passthrough():
        raise ValueError('through')
except ValueError as e:
    print('propagated', e.args)
try:
    with translate():
        raise ValueError('original')
except KeyError as e:
    print('translated', e.args, repr(str(e)))
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!(
            "skipping ffi-contextmanager-translate test: stdlib bridge unavailable ({stderr})"
        );
        return;
    }
    assert_eq!(
        stdout, "cleanup\npropagated ('through',)\ntranslated ('replaced',) \"'replaced'\"\n",
        "stderr={stderr}"
    );
}

/// `send` and `close` on the same wrapper. `ExitStack`/`closing` call `close()`,
/// and a stdlib driver that pushes values in uses `send` — including the rule
/// that an exhausted generator's `return` value comes back as
/// `StopIteration.value`.
#[test]
fn ffi_stdlib_can_send_to_and_close_a_pythonrs_generator() {
    let src = "\
import contextlib
def echo():
    got = yield 'first'
    print('got', got)
    return 'done'
g = echo()
print(next(g))
try:
    g.send('pushed')
except StopIteration as e:
    print('stopped', e.value)
def closable():
    try:
        yield 1
    except GeneratorExit:
        print('closing')
        raise
with contextlib.closing(closable()) as c:
    print(next(c))
print('after')
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping ffi-generator-send-close test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "first\ngot pushed\nstopped done\n1\nclosing\nafter\n",
        "stderr={stderr}"
    );
}

/// `send`/`throw`/`close` belong to generators alone. A pythonrs `zip`/`map`
/// object reaches CPython through the SAME wrapper class as a generator, so the
/// wrapper has to refuse those methods for a non-generator target — with the
/// message the real `zip` gives. `contextlib.closing` asks CPython-side, which
/// is the only way to reach the wrapper's own attribute lookup: a pythonrs-side
/// `getattr` never leaves the host. A wrapper that answered `close()` for every
/// lazy iterator would make this block print nothing and exit 0.
#[test]
fn ffi_wrapped_non_generator_iterators_refuse_generator_methods() {
    let src = "\
import contextlib
try:
    with contextlib.closing(zip([1, 2], [3, 4])) as z:
        print(list(z))
except AttributeError as e:
    print('AE', e)
try:
    with contextlib.closing(map(str, [1, 2])) as m:
        print(list(m))
except AttributeError as e:
    print('AE', e)
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping ffi-iterator-methods test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout,
        "[(1, 3), (2, 4)]\nAE 'zip' object has no attribute 'close'\n\
         ['1', '2']\nAE 'map' object has no attribute 'close'\n",
        "stderr={stderr}"
    );
}

/// `isinstance`/`issubclass` against types that reach pythonrs from CPython or
/// as `collections.X` builtins: a `collections` container type matches its
/// instances, a bridged `types.FunctionType`/`GeneratorType` matches a NATIVE
/// function/generator (which crosses the bridge as a proxy CPython would
/// refuse), and a bridged namedtuple class is a class to `issubclass`.
#[test]
fn ffi_isinstance_bridged_and_collections_types() {
    let src = "\
import collections, types
od = collections.OrderedDict(); dd = collections.defaultdict(int)
print(isinstance(od, collections.OrderedDict), isinstance(dd, collections.defaultdict), isinstance(collections.deque(), collections.deque), isinstance(collections.Counter(), collections.Counter), isinstance(od, collections.Counter))
P = collections.namedtuple('P', 'a')
print(issubclass(P, tuple), issubclass(P, dict))
def gen(): yield
print(isinstance(lambda: 1, types.FunctionType), isinstance(gen(), types.GeneratorType), isinstance(len, types.FunctionType))
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping ffi-isinstance test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "True True True True False\nTrue False\nTrue True False\n",
        "stderr={stderr}"
    );
}

/// `contextlib.redirect_stdout` captures what the CPython side writes too: the
/// embedded interpreter's `sys.stdout` IS the target inside the block (so
/// `functools.partial(print, …)` lands in the buffer, in order, and
/// `sys.stdout is buf`), a pythonrs writer object receives CPython's
/// piecewise `print` writes, and a `logging` handler that grabbed the real
/// stream before the block keeps writing there. Expected output is
/// python3.14's for the same script.
#[test]
fn redirect_stdout_captures_cpython_side_writes() {
    let src = "\
import contextlib, io, functools, sys, logging
with contextlib.redirect_stdout(io.StringIO()) as buf:
    print('a')
    functools.partial(print, 'x')()
    print('b')
    same = sys.stdout is buf
print(repr(buf.getvalue()), same)
class W:
    def __init__(self): self.parts = []
    def write(self, s): self.parts.append(s)
w = W()
with contextlib.redirect_stdout(w):
    functools.partial(print, 'y', 'z', sep='-')()
print(w.parts)
h = logging.StreamHandler(sys.stdout)
lg = logging.getLogger('t'); lg.addHandler(h); lg.propagate = False
with contextlib.redirect_stdout(io.StringIO()) as b2:
    lg.warning('kept')
print(repr(b2.getvalue()))
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping redirect test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "'a\\nx\\nb\\n' True\n['y', '-', 'z', '\\n']\nkept\n''\n",
        "stderr={stderr}"
    );
}

/// CPython code that reassigns the embedded interpreter's `sys.stdout`
/// redirects pythonrs's `print` too, since CPython has one `sys.stdout`:
/// `unittest`'s `buffer=True` runner swallows a passing test's output and
/// replays a failing one's (to the stream that was current, which may be a
/// pythonrs-side redirect), `mock.patch('sys.stdout', …)` captures and restores,
/// and `sys.stdout is sys.__stdout__` holds again afterwards. Expected stdout is
/// python3.14's for the same script.
#[test]
fn cpython_side_stdout_assignment_redirects_print() {
    let src = "\
import sys, io, unittest, contextlib
import unittest.mock
class T(unittest.TestCase):
    def test_ok(self):
        print('passing-output')
    def test_fail(self):
        print('failing-output')
        sys.stderr.write('err-output\\n')
        self.assertEqual(1, 2)
stream = io.StringIO()
r = unittest.TextTestRunner(buffer=True, stream=stream, verbosity=0)
res = r.run(unittest.defaultTestLoader.loadTestsFromTestCase(T))
out = stream.getvalue()
print(res.testsRun, len(res.failures), 'failing-output' in out, 'err-output' in out, 'passing-output' in out)
print(sys.stdout is sys.__stdout__, sys.stderr is sys.__stderr__)
class W:
    def __init__(self): self.parts = []
    def write(self, s): self.parts.append(s)
w = W()
with contextlib.redirect_stdout(w):
    unittest.TextTestRunner(buffer=True, stream=io.StringIO()).run(
        unittest.defaultTestLoader.loadTestsFromTestCase(T))
    print('after-run')
print(w.parts)
buf = io.StringIO()
with unittest.mock.patch('sys.stdout', new=buf):
    print('into-buf')
    same = sys.stdout is buf
print(repr(buf.getvalue()), same, sys.stdout is sys.__stdout__)
with unittest.mock.patch('sys.stdout', new=None):
    print('dropped')
print('back')
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping stdout-assignment test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout,
        "\nStdout:\nfailing-output\n2 1 True True False\nTrue True\n\
         ['\\nStdout:\\nfailing-output\\n', 'after-run', '\\n']\n\
         'into-buf\\n' True True\nback\n",
        "stderr={stderr}"
    );
}

/// A CPython call result keeps its identity when it is not fresh: the list
/// `catch_warnings(record=True)` returns is the one `warnings.warn` appends to,
/// an `lru_cache`d list is the cached object, and a call that returns one of
/// its own arguments returns the pythonrs original. CPython's builtin types
/// cross as pythonrs's (`fields(D)[0].type is int`, `type(handle) is list`),
/// and a PEP 604 union's type IS 3.14's `types.UnionType`/`typing.Union`.
/// Expected output is python3.14's for the same script.
#[test]
fn bridged_results_and_types_keep_their_identity() {
    let src = "\
import warnings, functools, types, typing, dataclasses
with warnings.catch_warnings(record=True) as w:
    warnings.simplefilter('always')
    warnings.warn('boom')
    warnings.warn('dep', DeprecationWarning)
print(len(w), [str(x.message) for x in w], [x.category.__name__ for x in w])
@functools.lru_cache
def cached():
    return [1]
cached().append(2)
print(cached(), cached() is cached())
lst = [3, 1, 2]
print(functools.reduce(lambda acc, x: acc, [], lst) is lst)
fresh = functools.reduce(lambda acc, x: acc + [x], [1, 2], [])
print(fresh, type(fresh) is list)
u = type(int | str)
print(u is types.UnionType, u is typing.Union, (int | str).__class__ is types.UnionType, u[int, None])
@dataclasses.dataclass
class D:
    a: int
print(dataclasses.fields(D)[0].type is int, type(types.SimpleNamespace(m=[]).m) is list)
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping identity test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout,
        "2 ['boom', 'dep'] ['UserWarning', 'DeprecationWarning']\n\
         [1, 2] True\n\
         True\n\
         [1, 2] True\n\
         True True True int | None\n\
         True True\n",
        "stderr={stderr}"
    );
}

/// Zero-arg `super()` in a class CPython built (any class with a foreign base,
/// here `abc.ABC` and `enum.Enum`) resolves through the class's `__class__` cell
/// to CPython's own `super`, in `__init__`, a classmethod, and across two levels
/// of overriding; an explicit `super(Cls, obj)` against such a class is
/// CPython's `super` too. A native method called with its receiver as a plain
/// argument (`Kid.g(obj)`, `map(Kid.g, …)`) reads `super()`'s instance from its
/// first argument, as CPython does. Expected output is python3.14's.
#[test]
fn zero_arg_super_in_a_class_with_a_foreign_base() {
    let src = "\
import abc, enum
class Shape(abc.ABC):
    def __init__(self, name): self.name = name
    @abc.abstractmethod
    def area(self): ...
    @classmethod
    def make(cls): return cls.__name__
    def describe(self): return f'{self.name}:{self.area()}'
class Sq(Shape):
    def __init__(self, s):
        super().__init__('sq')
        self.s = s
    def area(self): return self.s * self.s
    @classmethod
    def make(cls): return 'Sq+' + super().make()
    def describe(self): return '[' + super().describe() + ']'
class Sq2(Sq):
    def area(self): return super().area() + 1
q = Sq2(3)
print(q.describe(), Sq2.make(), q.name, isinstance(q, Shape))
print(type(super(Sq, q)).__name__)
class Color(enum.Enum):
    RED = 1
    def label(self): return 'c:' + super().__str__()
print(Color.RED.label())
class Base:
    def g(self): return 'Base.g'
class Kid(Base):
    def g(self): return 'Kid+' + super().g()
print(Kid.g(Kid()), list(map(Kid.g, [Kid()])))
class M:
    @staticmethod
    def s(): return super()
try: M.s()
except RuntimeError as e: print('RE', e)
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping foreign super test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout,
        "[sq:10] Sq+Sq2 sq True\n\
         super\n\
         c:Color.RED\n\
         Kid+Base.g ['Kid+Base.g']\n\
         RE super(): no arguments\n",
        "stderr={stderr}"
    );
}

/// An exception keeps its identity across the bridge, both ways: a pythonrs
/// exception — builtin or user-defined — thrown into a generator by CPython's
/// `contextlib` comes back as the same object (so `__exit__` sees `exc is
/// value` and declines, and the `with` re-raises it), and one a callback
/// raises inside CPython code (`json.dumps(default=…)`) is caught as itself,
/// with its class and attributes. One CPython raised has CPython's class as
/// its type, with that class's `__mro__`. Expected output is CPython 3.14's.
#[test]
fn exceptions_keep_identity_and_type_across_the_bridge() {
    let src = r#"
import contextlib, json, struct

class MyErr(ValueError):
    def __init__(self, msg, code):
        super().__init__(msg)
        self.code = code

@contextlib.contextmanager
def plain():
    yield

@contextlib.contextmanager
def reraising():
    try:
        yield
    except KeyError:
        raise

for cm in (plain, reraising):
    for exc in (KeyError('k'), MyErr('m', 7)):
        try:
            with cm():
                raise exc
        except (KeyError, MyErr) as caught:
            print(cm.__name__, repr(caught), caught is exc)

raised = None
def default(o):
    global raised
    raised = MyErr('bad', 3)
    raise raised
try:
    json.dumps(object(), default=default)
except MyErr as e:
    print('callback', repr(e), e.code, e is raised)
try:
    json.dumps(object(), default=default)
except ValueError as e:
    print('callback as ValueError', type(e).__name__)

try:
    json.loads('{bad')
except ValueError as e:
    t = type(e)
    print(t, t.__mro__, t is json.JSONDecodeError, e.__class__ is t, e.lineno)
try:
    struct.pack('i')
except struct.error as e:
    print(type(e), type(e).__mro__, type(e) is struct.error)
try:
    raise struct.error('direct')
except struct.error as e:
    print('raised', repr(e))
"#;
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping exception-identity test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout,
        r#"plain KeyError('k') True
plain MyErr('m') True
reraising KeyError('k') True
reraising MyErr('m') True
callback MyErr('bad') 3 True
callback as ValueError MyErr
<class 'json.decoder.JSONDecodeError'> (<class 'json.decoder.JSONDecodeError'>, <class 'ValueError'>, <class 'Exception'>, <class 'BaseException'>, <class 'object'>) True True 1
<class 'struct.error'> (<class 'struct.error'>, <class 'Exception'>, <class 'BaseException'>, <class 'object'>) True
raised error('direct')
"#,
        "stderr={stderr}"
    );
}

/// An uncaught exception CPython raised shows the frames it passed through
/// inside CPython after the pythonrs ones (`traceback.format_tb` of its own
/// traceback), and its last line names the type as CPython does, module
/// included (`struct.error`).
#[test]
fn uncaught_bridged_exceptions_show_their_cpython_frames_and_qualified_type() {
    let (_, stderr, ok) = run_py("import textwrap\ntextwrap.shorten('a b c', 4)\n");
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping bridged-traceback test: stdlib bridge unavailable ({stderr})");
        return;
    }
    let frames: Vec<&str> = stderr
        .lines()
        .filter(|l| l.contains("textwrap.py\", line "))
        .map(|l| l.rsplit(", in ").next().unwrap_or(""))
        .collect();
    assert_eq!(
        frames,
        ["shorten", "fill", "wrap", "_wrap_chunks"],
        "stderr={stderr}"
    );
    assert!(
        stderr.ends_with(
            "    raise ValueError(\"placeholder too large for max width\")\nValueError: placeholder too large for max width\n"
        ),
        "stderr={stderr}"
    );
    let (_, stderr, _) = run_py("import struct\nstruct.pack('i')\n");
    assert!(
        stderr.ends_with("\nstruct.error: pack expected 1 items for packing (got 0)\n"),
        "stderr={stderr}"
    );
}

/// A pythonrs object crosses into `pickle` (CPython's C pickler) as CPython
/// would hand it over: an instance's `__class__` is its class's mirror, which
/// `__main__` resolves by name (the embedded `__main__` answers from the
/// program's namespace), and its `__reduce_ex__` is the native one, so the bytes
/// are CPython's at every protocol. Loading rebuilds NATIVE objects: `type(p) is
/// P`, a class or function pickled by name is the very object, `__reduce__`
/// recipes and `__getstate__`/`__setstate__` run, a dataclass and an enum member
/// round-trip, and an object reached twice — or through itself — is one object.
/// Expected output is python3.14's for the same script.
#[test]
fn pickle_round_trips_native_classes_instances_and_functions() {
    let src = "\
import pickle, dataclasses, enum
class P:
    def __init__(self, x): self.x = x
    def __eq__(self, o): return type(o) is P and o.x == self.x
    def __repr__(self): return f'P({self.x!r})'
class R:
    def __init__(self, a, b): self.a, self.b = a, b
    def __reduce__(self): return (R, (self.a, self.b))
    def __repr__(self): return f'R({self.a}, {self.b})'
class S:
    def __init__(self): self.v = 1
    def __getstate__(self): return {'v': self.v * 10}
    def __setstate__(self, st): self.v = st['v'] + 1
class Outer:
    class Inner:
        def __init__(self): self.k = 'in'
def f(a): return a + 1
@dataclasses.dataclass
class D:
    a: int
    b: list
class Color(enum.Enum):
    RED = 1
for proto in range(6):
    b = pickle.dumps(P(3), proto)
    p = pickle.loads(b)
    print(proto, b, type(p).__name__, p.x, p == P(3), isinstance(p, P))
print(pickle.loads(pickle.dumps(R(1, [2]))))
s = pickle.loads(pickle.dumps(S())); print(type(s) is S, s.v)
i = pickle.loads(pickle.dumps(Outer.Inner())); print(type(i) is Outer.Inner, i.k)
print(pickle.loads(pickle.dumps(f)) is f, pickle.loads(pickle.dumps(f))(1))
print(pickle.loads(pickle.dumps(P)) is P, pickle.dumps(P))
d = pickle.loads(pickle.dumps(D(1, [2]))); print(d, d == D(1, [2]))
print(pickle.loads(pickle.dumps(Color.RED)) is Color.RED)
a = P(1); a.me = a; g = [a, a]
h = pickle.loads(pickle.dumps(g)); print(h[0] is h[1], h[0].me is h[0], h[0].x)
try: pickle.dumps(lambda: 1)
except Exception as e: print(type(e).__name__)
m = pickle.loads(pickle.dumps(P(5).__eq__)); print(m(P(5)))
print(pickle.dumps({'k': P([1, 2])}, 2))
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping pickle test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout,
        r#"
0 b'ccopy_reg\n_reconstructor\np0\n(c__main__\nP\np1\nc__builtin__\nobject\np2\nNtp3\nRp4\n(dp5\nVx\np6\nI3\nsb.' P 3 True True
1 b'ccopy_reg\n_reconstructor\nq\x00(c__main__\nP\nq\x01c__builtin__\nobject\nq\x02Ntq\x03Rq\x04}q\x05X\x01\x00\x00\x00xq\x06K\x03sb.' P 3 True True
2 b'\x80\x02c__main__\nP\nq\x00)\x81q\x01}q\x02X\x01\x00\x00\x00xq\x03K\x03sb.' P 3 True True
3 b'\x80\x03c__main__\nP\nq\x00)\x81q\x01}q\x02X\x01\x00\x00\x00xq\x03K\x03sb.' P 3 True True
4 b'\x80\x04\x95\x1f\x00\x00\x00\x00\x00\x00\x00\x8c\x08__main__\x94\x8c\x01P\x94\x93\x94)\x81\x94}\x94\x8c\x01x\x94K\x03sb.' P 3 True True
5 b'\x80\x05\x95\x1f\x00\x00\x00\x00\x00\x00\x00\x8c\x08__main__\x94\x8c\x01P\x94\x93\x94)\x81\x94}\x94\x8c\x01x\x94K\x03sb.' P 3 True True
R(1, [2])
True 11
True in
True 2
True b'\x80\x05\x95\x12\x00\x00\x00\x00\x00\x00\x00\x8c\x08__main__\x94\x8c\x01P\x94\x93\x94.'
D(a=1, b=[2]) True
True
True True 1
PicklingError
True
b'\x80\x02}q\x00X\x01\x00\x00\x00kq\x01c__main__\nP\nq\x02)\x81q\x03}q\x04X\x01\x00\x00\x00xq\x05]q\x06(K\x01K\x02esbs.'
"#
        .trim_start(),
        "stderr={stderr}"
    );
}

/// `object.__reduce_ex__` on a user instance is CPython's (`reduce_newobj`,
/// `object_getstate`, `copyreg._reduce_ex`): a class's own `__getstate__`,
/// `__reduce__`, `__getnewargs__` and `__getnewargs_ex__` are honoured, slot
/// values travel as `(state, slots)` under their mangled names, protocols 0 and
/// 1 refuse a slotted class without `__getstate__`, and a builtin subclass
/// reduces to ITS class with the list/dict items (or, below protocol 2, the
/// builtin base and its value). Expected output is python3.14's.
#[test]
fn object_reduce_ex_follows_copyreg_for_user_instances() {
    let src = "\
class S:
    def __init__(self): self.v = 1
    def __getstate__(self): return {'v': self.v * 10}
class T:
    __slots__ = ('a', 'b', '__c')
    def __init__(self): self.a = 1; self.__c = 3
class U(T):
    def __init__(self): super().__init__(); self.z = 2
class E: pass
class R:
    def __reduce__(self): return (R, ())
class NA:
    def __init__(self, x): self.x = x
    def __getnewargs__(self): return (self.x,)
class NE:
    def __getnewargs_ex__(self): return ((1,), {'k': 2})
class L(list): pass
class Dd(dict): pass
class I(int): pass
def show(o, protos=(0, 1, 2, 5)):
    for p in protos:
        try: r = o.__reduce_ex__(p)
        except Exception as e: r = f'{type(e).__name__}: {e}'
        print(type(o).__name__, p, r if isinstance(r, str) else (getattr(r[0], '__name__', r[0]),) + tuple(r[1:]))
show(S()); show(T()); show(U()); show(E()); show(R()); show(NA(4), (2,)); show(NE(), (2,))
l = L([1, 2]); l.q = 1
r = l.__reduce_ex__(2); print(r[0].__name__, r[1], r[2], list(r[3]), r[4])
d = Dd(a=1); r = d.__reduce_ex__(2); print(r[1], r[2], r[3], list(r[4]))
show(I(5), (2,)); show(L([1]), (0,))
print(T().__getstate__(), U().__getstate__(), E().__getstate__(), S().__getstate__())
class S2(set): pass
s2 = S2({1}); s2.t = 1
print(s2.__reduce_ex__(2)[0].__name__, s2.__reduce_ex__(2)[1:], S2().__reduce__())
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping reduce test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout,
        r#"
S 0 ('_reconstructor', (<class '__main__.S'>, <class 'object'>, None), {'v': 10})
S 1 ('_reconstructor', (<class '__main__.S'>, <class 'object'>, None), {'v': 10})
S 2 ('__newobj__', (<class '__main__.S'>,), {'v': 10}, None, None)
S 5 ('__newobj__', (<class '__main__.S'>,), {'v': 10}, None, None)
T 0 TypeError: a class that defines __slots__ without defining __getstate__ cannot be pickled
T 1 TypeError: a class that defines __slots__ without defining __getstate__ cannot be pickled
T 2 ('__newobj__', (<class '__main__.T'>,), (None, {'a': 1, '_T__c': 3}), None, None)
T 5 ('__newobj__', (<class '__main__.T'>,), (None, {'a': 1, '_T__c': 3}), None, None)
U 0 TypeError: a class that defines __slots__ without defining __getstate__ cannot be pickled
U 1 TypeError: a class that defines __slots__ without defining __getstate__ cannot be pickled
U 2 ('__newobj__', (<class '__main__.U'>,), ({'z': 2}, {'a': 1, '_T__c': 3}), None, None)
U 5 ('__newobj__', (<class '__main__.U'>,), ({'z': 2}, {'a': 1, '_T__c': 3}), None, None)
E 0 ('_reconstructor', (<class '__main__.E'>, <class 'object'>, None))
E 1 ('_reconstructor', (<class '__main__.E'>, <class 'object'>, None))
E 2 ('__newobj__', (<class '__main__.E'>,), None, None, None)
E 5 ('__newobj__', (<class '__main__.E'>,), None, None, None)
R 0 ('R', ())
R 1 ('R', ())
R 2 ('R', ())
R 5 ('R', ())
NA 2 ('__newobj__', (<class '__main__.NA'>, 4), {'x': 4}, None, None)
NE 2 ('__newobj_ex__', (<class '__main__.NE'>, (1,), {'k': 2}), None, None, None)
__newobj__ (<class '__main__.L'>,) {'q': 1} [1, 2] None
(<class '__main__.Dd'>,) None None [('a', 1)]
I 2 ('__newobj__', (<class '__main__.I'>, 5), None, None, None)
L 0 ('_reconstructor', (<class '__main__.L'>, <class 'list'>, [1]))
(None, {'a': 1, '_T__c': 3}) ({'z': 2}, {'a': 1, '_T__c': 3}) None {'v': 10}
S2 (([1],), {'t': 1}) (<class '__main__.S2'>, ([],), None)
"#
        .trim_start(),
        "stderr={stderr}"
    );
}

/// The object GRAPH crosses the bridge, not a tree: a list that contains itself
/// reaches `json` (which reports the cycle) and `pickle` (which round-trips it)
/// instead of recursing until the process aborts, and a list reached twice is
/// one CPython object, so `pickle` stores it once and loads it shared — in both
/// directions. Expected output is python3.14's.
#[test]
fn self_referential_and_shared_containers_cross_as_a_graph() {
    let src = "\
import json, pickle, copy
l = [1]; l.append(l)
try: json.dumps(l)
except ValueError as e: print('json', e)
d = {}; d['self'] = d
try: json.dumps(d)
except ValueError as e: print('json', e)
x = [1]
y = pickle.loads(pickle.dumps([x, x, (x,)]))
print(y, y[0] is y[1], y[2][0] is y[0])
z = pickle.loads(pickle.dumps(l))
print(z[1] is z, z)
import operator
print(operator.is_(l, l), l[1] is l)
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping graph test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout,
        "json Circular reference detected\n\
         json Circular reference detected\n\
         [[1], [1], ([1],)] True True\n\
         True [1, [...]]\n\
         True True\n",
        "stderr={stderr}"
    );
}

/// `import __main__` is the running program's own module — the namespace its
/// globals live in, registered in `sys.modules` before the body runs and
/// repr'd as CPython reprs a script's module — not the embedded interpreter's
/// empty `__main__`. Expected output is python3.14's.
#[test]
fn import_main_is_the_programs_own_module() {
    let src = "\
import sys
X = 1
import __main__
print(__main__.X, __import__('__main__').X, sys.modules['__main__'] is __main__, __main__.__dict__ is globals())
print(repr(__main__) == f\"<module '__main__' from {__file__!r}>\")
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping __main__ test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(stdout, "1 1 True True\nTrue\n", "stderr={stderr}");
}

/// A native class crosses as one CPython object and comes back as itself, so a
/// class named in an annotation is that class again on the far side:
/// `dataclasses.fields(F)[0].type is E`, `typing.get_type_hints(N)['e'] is E`,
/// and inside a generic alias. Expected output is python3.14's.
#[test]
fn a_native_class_in_an_annotation_round_trips_as_itself() {
    let src = "\
import typing, dataclasses, collections
class E: pass
class N(typing.NamedTuple):
    e: E
    i: int
print(N.__annotations__['e'] is E, typing.get_type_hints(N)['e'] is E, N(E(), 1).i)
@dataclasses.dataclass
class F:
    e: E
    g: list[E]
print(dataclasses.fields(F)[0].type is E, dataclasses.fields(F)[1].type, dataclasses.fields(F)[1].type.__args__[0] is E)
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping annotation identity test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "True True 1\nTrue list[__main__.E] True\n",
        "stderr={stderr}"
    );
}

/// Slot values pickle as `(state, slots)` and come back into the native class's
/// slot storage — including a slotted base's slots under an unslotted subclass,
/// whose mirror does not carry them — so the reloaded object pickles to the same
/// bytes. Expected output is python3.14's.
#[test]
fn pickle_round_trips_slot_values() {
    let src = "\
import pickle
class T:
    __slots__ = ('a', '__c')
    def __init__(self): self.a = 1; self.__c = 3
    def c(self): return self.__c
class U(T):
    pass
u = U(); u.z = 5
for o in (T(), u):
    for p in (2, 5):
        r = pickle.loads(pickle.dumps(o, p))
        print(type(r).__name__, r.a, r.c(), getattr(r, 'z', None), pickle.dumps(o, p) == pickle.dumps(r, p))
";
    let (stdout, stderr, ok) = run_py(src);
    if bridge_unavailable(ok, &stderr) {
        eprintln!("skipping slot pickle test: stdlib bridge unavailable ({stderr})");
        return;
    }
    assert_eq!(
        stdout, "T 1 3 None True\nT 1 3 None True\nU 1 3 5 True\nU 1 3 5 True\n",
        "stderr={stderr}"
    );
}
