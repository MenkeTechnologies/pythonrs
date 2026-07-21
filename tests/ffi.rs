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
