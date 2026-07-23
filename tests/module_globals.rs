//! An imported module's functions must resolve their globals against THEIR OWN
//! module namespace, not the importer's — CPython's `func.__globals__ is
//! module.__dict__`. Regression test for the bug where a single shared global
//! namespace meant a vendored stdlib function (or a user module's) raised
//! `NameError` for a name defined at its own module scope the moment it was
//! called from a different module.
//!
//! Drives the built `python` binary as a subprocess with `$PYTHONRS_LIB` pointed
//! at an isolated fixtures directory, so the fixture module is imported and run on
//! pythonrs's own interpreter. Gated to the no-libpython build; under the
//! `stdlib-ffi` bridge an imported module runs on CPython, which is not the code
//! path under test.
#![cfg(not(feature = "stdlib-ffi"))]

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

/// A process-unique scratch directory for one test's fixture module.
fn fixtures_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pythonrs-modglobals-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixtures dir");
    dir
}

/// Write `name.py` with `body` into a fresh fixtures dir, run `python -c code`
/// with `$PYTHONRS_LIB` pointed at it, and return trimmed stdout (panicking with
/// stderr on a non-zero exit so a `NameError` regression surfaces its traceback).
fn run_with_module(tag: &str, name: &str, body: &str, code: &str) -> String {
    let dir = fixtures_dir(tag);
    let mut f = std::fs::File::create(dir.join(format!("{name}.py"))).expect("write fixture");
    f.write_all(body.as_bytes()).expect("write fixture body");
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .env("PYTHONRS_LIB", &dir)
        .args(["-c", code])
        .output()
        .expect("spawn python binary");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "python exited {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn imported_function_sees_its_own_module_global() {
    let out = run_with_module(
        "fn",
        "modg_fn",
        "X = 42\ndef f():\n    return X\n",
        "import modg_fn; print(modg_fn.f())",
    );
    assert_eq!(out, "42");
}

#[test]
fn imported_method_and_class_body_see_module_global() {
    let out = run_with_module(
        "cls",
        "modg_cls",
        "X = 7\nclass C:\n    K = X\n    def m(self):\n        return X\n    def is_c(self, o):\n        return isinstance(o, C)\n",
        "import modg_cls as m; c = m.C(); print(m.C.K, c.m(), c.is_c(c))",
    );
    // Class-body reads `X`, the method reads `X`, and the method resolves its own
    // class name `C` (also a module global) for `isinstance`.
    assert_eq!(out, "7 7 True");
}

#[test]
fn imported_generator_sees_module_global_on_each_resume() {
    let out = run_with_module(
        "gen",
        "modg_gen",
        "BASE = 100\ndef g():\n    for i in range(3):\n        yield BASE + i\n",
        "import modg_gen; print(list(modg_gen.g()))",
    );
    // The generator suspends and resumes across the importer's frames; each resume
    // must restore the generator's module so `BASE` still resolves.
    assert_eq!(out, "[100, 101, 102]");
}

#[test]
fn imported_module_global_does_not_leak_into_importer() {
    // The fixture's module global `SECRET` must NOT become visible at the
    // importer's scope — a single shared namespace would have leaked it.
    let out = run_with_module(
        "leak",
        "modg_leak",
        "SECRET = 1\ndef reveal():\n    return SECRET\n",
        "import modg_leak\nprint(modg_leak.reveal())\nprint('SECRET' in dir())",
    );
    assert_eq!(out, "1\nFalse");
}
