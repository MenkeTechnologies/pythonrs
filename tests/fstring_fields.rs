//! f-string replacement fields: a field's text is split on the `:`/`!`/`=`
//! outside any string literal or bracket, malformed fields are `SyntaxError`s
//! positioned as CPython 3.14.8 positions them, and a runtime error in a field
//! carries the field's own line and caret. Expectations are frozen from that
//! CPython; no `python3` is needed.

use pythonrs::{eval_str, host};
use std::process::Command;

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

/// `(msg, lineno, offset, end_offset)` of the SyntaxError each source raises
/// when compiled as an expression.
fn errors(sources: &[&str]) -> String {
    let list = sources
        .iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let src = format!(
        "def err(s):
    try:
        compile(s, '<string>', 'eval')
    except SyntaxError as e:
        return (e.msg, e.lineno, e.offset, e.end_offset)
    return None
r = [err(s) for s in [{list}]]
"
    );
    g(&src, "r")
}

#[test]
fn quotes_in_a_field_hide_its_delimiters() {
    let src = r#"
x = [1, 2]
d = {'a': 1}
w = 5
r = [
    f"{':'.join(map(str, x))}",
    f"{'!r'}",
    f"{'a=b'}",
    f"{d['a']:>{w}}",
    f"{x!r:>10}",
    f"{x=!s}",
    f"{'{'}{'}'}",
    f'{"{}"}',
    f"{(lambda: 1)()}",
    f"{x[1:]}",
    f"{1 if x else 2}",
    f"{3.14159:.{2}f}",
    f"{w:{w}}",
    f"{w:#x}",
    f"{1!r }",
]
"#;
    assert_eq!(
        g(src, "r"),
        "['1:2', '!r', 'a=b', '    1', '    [1, 2]', 'x=[1, 2]', '{}', '{}', '1', '[2]', '1', '3.14', '    5', '0x5', '1']"
    );
}

#[test]
fn a_single_closing_brace_is_refused() {
    assert_eq!(
        errors(&[r#"f"}""#, r#"f"{{}""#, r#"f"{1}}""#, r#"f"a}b""#, r#"f"{1:}}""#]),
        "[(\"f-string: single '}' is not allowed\", 1, 3, 3), (\"f-string: single '}' is not allowed\", 1, 5, 5), (\"f-string: single '}' is not allowed\", 1, 6, 6), (\"f-string: single '}' is not allowed\", 1, 4, 4), (\"f-string: single '}' is not allowed\", 1, 7, 7)]"
    );
}

#[test]
fn an_empty_expression_is_refused() {
    assert_eq!(
        errors(&[r#"f"{}""#, r#"f"{ }""#, r#"f"{!r}""#, r#"f"{:x}""#, r#"f"{=}""#, r#"f"{1:{}}""#]),
        "[(\"f-string: valid expression required before '}'\", 1, 4, 5), (\"f-string: valid expression required before '}'\", 1, 5, 6), (\"f-string: valid expression required before '!'\", 1, 4, 5), (\"f-string: valid expression required before ':'\", 1, 4, 5), (\"f-string: valid expression required before '='\", 1, 4, 5), (\"f-string: valid expression required before '}'\", 1, 7, 8)]"
    );
}

#[test]
fn a_bad_conversion_is_refused() {
    assert_eq!(
        errors(&[r#"f"{1!x}""#, r#"f"{1!}""#, r#"f"{1! r}""#, r#"f"{1!rx}""#, r#"f"{1=!z}""#]),
        "[(\"f-string: invalid conversion character 'x': expected 's', 'r', or 'a'\", 1, 6, 7), ('f-string: missing conversion character', 1, 6, 7), ('f-string: conversion type must come right after the exclamation mark', 1, 5, 8), (\"f-string: invalid conversion character 'rx': expected 's', 'r', or 'a'\", 1, 6, 8), (\"f-string: invalid conversion character 'z': expected 's', 'r', or 'a'\", 1, 7, 8)]"
    );
}

#[test]
fn an_unterminated_field_is_expecting_a_brace() {
    assert_eq!(
        errors(&[r#"f"{""#, r#"f"{ ""#, r#"f"{1+""#, r#"f"{'a'""#, r#"f"abc{1""#, r#"f"{x:""#]),
        "[(\"f-string: expecting '}'\", 1, 4, 4), (\"f-string: expecting '}'\", 1, 5, 5), (\"f-string: expecting '}'\", 1, 6, 6), (\"f-string: expecting '}'\", 1, 7, 7), (\"f-string: expecting '}'\", 1, 8, 8), (\"f-string: expecting '}', or format specs\", 1, 6, 7)]"
    );
}

#[test]
fn field_expression_errors_are_positioned_in_the_source() {
    assert_eq!(
        errors(&[
            r#"f"{lambda x: 1}""#,
            r#"f"{1)}""#,
            r#"f"{a b}""#,
            r#"f"{(1}""#,
            r#"f"{1:{2:{3:{4}}}}""#,
        ]),
        "[('f-string: lambda expressions are not allowed without parentheses', 1, 4, 13), (\"f-string: unmatched ')'\", 1, 5, 5), ('invalid syntax. Perhaps you forgot a comma?', 1, 4, 7), (\"closing parenthesis '}' does not match opening parenthesis '('\", 1, 6, 6), ('f-string: expressions nested too deeply', 1, 11, 11)]"
    );
}

#[test]
fn a_positional_argument_after_a_keyword_points_at_the_closing_paren() {
    assert_eq!(
        errors(&["f(a=1, 2)"]),
        "[('positional argument follows keyword argument', 1, 9, 10)]"
    );
}

/// Run `src` as a script and return its stderr with the script path as `F`.
fn stderr_of(src: &str) -> String {
    let tmp = tempfile::tempdir().expect("temp dir");
    let path = tmp.path().canonicalize().unwrap().join("t.py");
    std::fs::write(&path, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .arg(&path)
        .env("PYTHONRS_CACHE", "0")
        .output()
        .expect("spawn python");
    String::from_utf8_lossy(&out.stderr).replace(path.to_str().unwrap(), "F")
}

#[test]
fn a_runtime_error_in_a_field_carries_its_line_and_caret() {
    assert_eq!(
        stderr_of("x = [1]\nprint(f\"{x[5]}\")\n"),
        "Traceback (most recent call last):\n  File \"F\", line 2, in <module>\n    print(f\"{x[5]}\")\n             ~^^^\nIndexError: list index out of range\n"
    );
    assert_eq!(
        stderr_of("print(f\"\"\"\n{\n  1/0\n}\"\"\")\n"),
        "Traceback (most recent call last):\n  File \"F\", line 3, in <module>\n    1/0\n    ~^~\nZeroDivisionError: division by zero\n"
    );
}

#[test]
fn a_bad_format_spec_carries_the_field_line_and_caret() {
    assert_eq!(
        stderr_of("w = 5\nprint(1,\n      f\"{w:.{2}}\")\n"),
        "Traceback (most recent call last):\n  File \"F\", line 3, in <module>\n    f\"{w:.{2}}\")\n      ^^^^^^^^\nValueError: Precision not allowed in integer format specifier\n"
    );
}

/// The conversion is one NAME token: anything after it but blanks is where
/// the grammar expected `:` or `}`.
#[test]
fn text_after_the_conversion_name_expects_a_colon_or_brace() {
    assert_eq!(
        errors(&[r#"f"{1!r=}""#, r#"f"{1!r!s}""#, r#"f"{1!a b}""#, r#"f"{1!s  }""#]),
        "[(\"f-string: expecting ':' or '}'\", 1, 7, 8), (\"f-string: expecting ':' or '}'\", 1, 7, 8), (\"f-string: expecting ':' or '}'\", 1, 8, 9), None]"
    );
}
