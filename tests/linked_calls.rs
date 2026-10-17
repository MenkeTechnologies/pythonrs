//! Calls that run on the caller's own VM. A function the compiler linked into
//! the main chunk (`FuncDef::linked`) and called from module code is entered as
//! a fusevm frame instead of a VM run of its own, so everything a VM run used
//! to provide per call — the host frame, the return value, the recursion
//! limit, the traceback — now comes from that frame. Expected output is
//! CPython 3.15's for the same program, with its directory written as `DIR`.

use std::process::Command;

/// Run `src` as `main.py` from a fresh directory; `(stdout, stderr)` with the
/// directory written as `DIR`.
fn run(src: &str) -> (String, String) {
    let tmp = tempfile::tempdir().expect("temp dir");
    let dir = tmp.path().canonicalize().expect("canonical temp dir");
    std::fs::write(dir.join("main.py"), src).expect("write source");
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .arg(dir.join("main.py"))
        .env("PYTHONRS_CACHE", "0")
        .output()
        .expect("spawn python");
    let d = dir.to_string_lossy().into_owned();
    let clean = |b: &[u8]| String::from_utf8_lossy(b).replace(&d, "DIR");
    (clean(&out.stdout), clean(&out.stderr))
}

/// A linked callee sees its caller's frame (`sys._getframe`), calls whatever
/// function object the name holds when the call runs, reaches its class
/// through zero-argument `super()`, and returns a loop's result.
#[test]
fn a_linked_call_behaves_as_a_call() {
    let (out, err) = run("\
import sys
def who():
    f = sys._getframe(1)
    return (f.f_code.co_name, f.f_lineno)
def caller():
    return who()
def f():
    return 'first'
g = f
def f():
    return 'second'
class Base:
    def m(self, x):
        return ('base', x)
class Sub(Base):
    def m(self, x):
        return ('sub', super().m(x + 1))
def total(n):
    t = 0
    i = 0
    while i < n:
        t += i
        i += 1
    return t
print(caller(), g(), f(), Sub().m(1), total(1000))
");
    assert_eq!(err, "");
    assert_eq!(out, "('caller', 6) first second ('sub', ('base', 2)) 499500\n");
}

/// An exception raised two linked frames deep lists every frame, each at the
/// line and caret of the call it was making.
#[test]
fn an_exception_lists_each_linked_frame() {
    let (_, err) = run("\
def inner(d):
    return d['missing']
def outer(d):
    v = inner(d)
    return v
outer({})
");
    assert_eq!(
        err,
        "Traceback (most recent call last):\n  File \"DIR/main.py\", line 6, in <module>\n    outer({})\n    \
         ~~~~~^^^^\n  File \"DIR/main.py\", line 4, in outer\n    v = inner(d)\n  \
         File \"DIR/main.py\", line 2, in inner\n    return d['missing']\n           ~^^^^^^^^^^^\n\
         KeyError: 'missing'\n"
    );
}

/// Linked recursion stops at the recursion limit, where a frame per call used
/// to cost a VM run and a native stack frame each, and reports it as CPython
/// does.
#[test]
fn linked_recursion_stops_at_the_recursion_limit() {
    let (_, err) = run("\
def r(n):
    return r(n + 1)
def mid(x):
    return r(x) + 1
mid(0)
");
    let frame = "  File \"DIR/main.py\", line 2, in r\n    return r(n + 1)\n";
    assert_eq!(
        err,
        format!(
            "Traceback (most recent call last):\n  File \"DIR/main.py\", line 5, in <module>\n    mid(0)\n    \
             ~~~^^^\n  File \"DIR/main.py\", line 4, in mid\n    return r(x) + 1\n           ~^^^\n\
             {frame}{frame}{frame}  [Previous line repeated 995 more times]\n\
             RecursionError: maximum recursion depth exceeded\n"
        )
    );
}
