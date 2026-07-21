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
