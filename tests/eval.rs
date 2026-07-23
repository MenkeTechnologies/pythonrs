//! `eval` / `exec` / `globals` / `locals`. Drives the built `python` binary as a
//! subprocess so the full path — on-the-fly compile, re-entrant VM run, namespace
//! scoping, and value capture — is exercised end to end. Expected values are what
//! CPython 3.14 produces for the same program.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run a program and return `(stdout, exit_code)`.
fn run(src: &str) -> (String, i32) {
    let mut f = tempfile::Builder::new()
        .suffix(".py")
        .tempfile()
        .expect("temp file");
    f.write_all(src.as_bytes()).expect("write");
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .arg(f.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn python");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn stdout(src: &str) -> String {
    let (o, code) = run(src);
    assert_eq!(code, 0, "program exited {code}; stdout: {o}");
    o
}

#[test]
fn eval_expression_returns_value() {
    assert_eq!(stdout("print(eval('1 + 2 * 3'))"), "7\n");
    assert_eq!(stdout("print(eval('[i*i for i in range(4)]'))"), "[0, 1, 4, 9]\n");
    assert_eq!(stdout("print(eval('len(\"hello\")'))"), "5\n"); // builtins available
    assert_eq!(stdout("print(repr(eval('None')))"), "None\n");
}

#[test]
fn eval_reads_and_exec_writes_module_globals() {
    assert_eq!(stdout("x = 10\nprint(eval('x * 2'))"), "20\n");
    assert_eq!(stdout("exec('y = 42')\nprint(y)"), "42\n");
    // exec can define a function that later code (and eval) can call.
    assert_eq!(
        stdout("exec('def sq(n):\\n    return n*n')\nprint(sq(6), eval('sq(7)'))"),
        "36 49\n"
    );
}

#[test]
fn exec_returns_none() {
    assert_eq!(stdout("print(repr(exec('1 + 1')))"), "None\n");
}

#[test]
fn eval_rejects_statements_and_multiple_expressions() {
    // A statement, a series, or a bare newline after an operator is a SyntaxError,
    // matching CPython's single-expression eval contract.
    for bad in ["q = 1", "1+2\\n3+4", "import os", "1 +\\n2", "1+2;3+4"] {
        let src = format!(
            "try:\n    eval('{bad}')\n    print('NO ERROR')\nexcept SyntaxError:\n    print('SyntaxError')"
        );
        assert_eq!(stdout(&src), "SyntaxError\n", "eval({bad:?}) should be a SyntaxError");
    }
}

#[test]
fn eval_with_explicit_namespace_dicts() {
    assert_eq!(stdout("print(eval('a + b', {'a': 5, 'b': 3}))"), "8\n");
    // exec writes bindings back into the provided globals dict.
    assert_eq!(stdout("g = {'n': 4}\nexec('m = n * n', g)\nprint(g['m'])"), "16\n");
    assert_eq!(stdout("print(eval('2 ** 10', {}, {'z': 99}))"), "1024\n");
}

#[test]
fn eval_in_function_reads_locals_and_discards_writes() {
    // eval/exec inside a function read the caller's locals ...
    assert_eq!(
        stdout("def f():\n    k = 7\n    return eval('k + 1')\nprint(f())"),
        "8\n"
    );
    // ... but their assignments do not leak to globals (CPython semantics): the
    // module-level name stays unbound.
    assert_eq!(
        stdout(
            "def s():\n    exec('leaked = 99')\ns()\nprint('leaked' in globals())"
        ),
        "False\n"
    );
}

#[test]
fn nested_and_recursive_eval() {
    assert_eq!(stdout("print(eval(\"eval('3+4')\"))"), "7\n");
    // Recursion driven through eval resolves the function global and the local n.
    assert_eq!(
        stdout(
            "def fac(n):\n    return 1 if n < 2 else n * eval('fac(n-1)')\nprint(fac(5))"
        ),
        "120\n"
    );
}

#[test]
fn globals_and_locals_builtins() {
    assert_eq!(stdout("x = 5\nprint(globals()['x'])"), "5\n");
    assert_eq!(stdout("print(type(globals()).__name__)"), "dict\n");
    assert_eq!(
        stdout("def f():\n    a = 1\n    b = 2\n    return sorted(locals())\nprint(f())"),
        "['a', 'b']\n"
    );
    assert_eq!(
        stdout("def f():\n    n = 42\n    return eval('n*2', globals(), locals())\nprint(f())"),
        "84\n"
    );
}
