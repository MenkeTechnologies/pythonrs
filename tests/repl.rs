//! Interactive REPL displayhook echo (CPython "single" mode / `sys.displayhook`).
//!
//! Drives the built `python` binary in `--repl` over a piped (non-TTY) stdin —
//! the analogue of CPython's `python3 -i < file`. Each top-level expression
//! statement echoes `repr(value)` for a non-`None` result and binds the module
//! global `_`; assignments, `None` results, and nested-scope (class/def body)
//! expression statements do not echo. Runs headlessly in CI (no terminal, no
//! `python3`); the expected stdout is exactly what CPython 3.14 `python3 -i`
//! prints for the same input.

use std::io::Write;
use std::process::{Command, Stdio};

/// Feed `input` to `python --repl` on stdin and return captured stdout.
fn repl(input: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_python"))
        .arg("--repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn python --repl");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn echoes_non_none_expressions_and_binds_underscore() {
    // 1+1 -> 2, str repr is quoted, assignment is silent, bare name echoes,
    // print's None return is not echoed, `None` is not echoed, `_` holds the
    // last echoed value.
    let input = "1+1\n\"hi\"\nx=5\nx\nprint(\"p\")\nNone\n_\n";
    assert_eq!(repl(input), "2\n'hi'\n5\np\n5\n");
}

#[test]
fn multiple_expression_statements_on_one_line_each_echo() {
    // CPython single mode echoes every top-level expression statement.
    assert_eq!(repl("1;2\nx=10; x\n"), "1\n2\n10\n");
}

#[test]
fn container_reprs_match_cpython() {
    assert_eq!(
        repl("[1, 2, 3]\n{\"a\": 1}\n(1, 2)\n{1, 2, 3}\n"),
        "[1, 2, 3]\n{'a': 1}\n(1, 2)\n{1, 2, 3}\n"
    );
}

#[test]
fn module_level_control_flow_bodies_echo() {
    // `if`/`try` bodies run in the module frame (nestlevel <= 1) and echo.
    assert_eq!(
        repl("if True:\n    5\n\ntry:\n    7\nexcept: pass\n"),
        "5\n7\n"
    );
}

#[test]
fn nested_scope_bodies_do_not_echo() {
    // A class body and a function body are separate code objects (nestlevel > 1);
    // their expression statements are discarded, and `f()` returns None.
    assert_eq!(repl("class C:\n    11\n\ndef f():\n    9\n\nf()\n"), "");
}

#[test]
fn instance_repr_dunder_dispatches() {
    // displayhook uses repr(), which honors a user `__repr__`.
    let input = "class P:\n    def __repr__(self):\n        return \"P!\"\n\nP()\n";
    assert_eq!(repl(input), "P!\n");
}
