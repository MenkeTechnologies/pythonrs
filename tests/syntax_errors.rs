//! Syntax errors as CPython reports them: the `File`/source/caret block a
//! program that does not compile prints, and the `msg`, `lineno`, `offset`,
//! `text`, `end_lineno` and `end_offset` a `SyntaxError` from `exec`/`eval`
//! carries. Every expectation is the verbatim output of CPython 3.14.7 for
//! the same program.

use std::process::Command;

/// `python -c src`: `(stderr, exit status)`.
fn run_c(src: &str) -> (String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args(["-c", src])
        .output()
        .expect("spawn python");
    (
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// `(program, stderr, exit status)`.
const CASES: &[(&str, &str, i32)] = &[
    (
        r#"x = ("#,
        r#"  File "<string>", line 1
    x = (
        ^
SyntaxError: '(' was never closed
"#,
        1,
    ),
    (
        r#"if x
  pass"#,
        r#"  File "<string>", line 1
    if x
        ^
SyntaxError: expected ':'
"#,
        1,
    ),
    (
        r#"a b"#,
        r#"  File "<string>", line 1
    a b
      ^
SyntaxError: invalid syntax
"#,
        1,
    ),
    (
        r#"x = 1 +* 2"#,
        r#"  File "<string>", line 1
    x = 1 +* 2
           ^
SyntaxError: invalid syntax
"#,
        1,
    ),
    (
        r#"def f(:
  pass"#,
        r#"  File "<string>", line 1
    def f(:
          ^
SyntaxError: invalid syntax
"#,
        1,
    ),
    (
        r#"x = 1
  y = 2"#,
        r#"  File "<string>", line 2
    y = 2
IndentationError: unexpected indent
"#,
        1,
    ),
    (
        r#"s = 'abc"#,
        r#"  File "<string>", line 1
    s = 'abc
        ^
SyntaxError: unterminated string literal (detected at line 1)
"#,
        1,
    ),
    (
        r#"print "hi""#,
        r#"  File "<string>", line 1
    print "hi"
    ^^^^^^^^^^
SyntaxError: Missing parentheses in call to 'print'. Did you mean print(...)?
"#,
        1,
    ),
    (
        r#"x = [1,
 2)"#,
        r#"  File "<string>", line 2
    2)
     ^
SyntaxError: closing parenthesis ')' does not match opening parenthesis '[' on line 1
"#,
        1,
    ),
    (
        r#"if True:
pass"#,
        r#"  File "<string>", line 2
    pass
    ^^^^
IndentationError: expected an indented block after 'if' statement on line 1
"#,
        1,
    ),
    (
        r#"return 5"#,
        r#"  File "<string>", line 1
SyntaxError: 'return' outside function
"#,
        1,
    ),
    (
        r#"for x in y:
  def f():
    break"#,
        r#"  File "<string>", line 3
SyntaxError: 'break' outside loop
"#,
        1,
    ),
    (
        r#"class A:
  continue"#,
        r#"  File "<string>", line 2
SyntaxError: 'continue' not properly in loop
"#,
        1,
    ),
    (
        r#"x = 1
break
x = ("#,
        r#"  File "<string>", line 3
    x = (
        ^
SyntaxError: '(' was never closed
"#,
        1,
    ),
    (
        r#"x = )"#,
        r#"  File "<string>", line 1
    x = )
        ^
SyntaxError: unmatched ')'
"#,
        1,
    ),
    (
        r#"f(
1,
"#,
        r#"  File "<string>", line 1
    f(
     ^
SyntaxError: '(' was never closed
"#,
        1,
    ),
    (
        r#"x = 5 if y"#,
        r#"  File "<string>", line 1
    x = 5 if y
        ^^^^^^
SyntaxError: expected 'else' after 'if' expression
"#,
        1,
    ),
    (
        r#"s = f'abc"#,
        r#"  File "<string>", line 1
    s = f'abc
        ^
SyntaxError: unterminated f-string literal (detected at line 1)
"#,
        1,
    ),
    (
        r#"s = '''abc
def"#,
        r#"  File "<string>", line 1
    s = '''abc
        ^
SyntaxError: unterminated triple-quoted string literal (detected at line 2)
"#,
        1,
    ),
    (
        r#"exec("x = (")"#,
        r#"Traceback (most recent call last):
  File "<string>", line 1, in <module>
    exec("x = (")
    ~~~~^^^^^^^^^
  File "<string>", line 1
    x = (
        ^
SyntaxError: '(' was never closed
"#,
        1,
    ),
];

#[test]
fn a_program_that_does_not_compile_shows_the_line_and_a_caret() {
    for (src, stderr, status) in CASES {
        assert_eq!(run_c(src), (stderr.to_string(), *status), "for {src:?}");
    }
}

#[test]
fn syntax_error_attributes_from_exec_and_eval() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args(["-c", r#"for s in ["x = (", '"abc', "def f(:", "1 +", "if x\n  pass", "print 1", "a b", "x = [1, 2", "return", "break", "x = 1\n  y = 2", "if True:\npass", "x = )", "x = [1,\n 2)"]:
    try:
        exec(s, {})
    except SyntaxError as e:
        print(type(e).__name__, e.args, str(e))
for s in ["1 +", "(1", "a b"]:
    try:
        eval(s)
    except SyntaxError as e:
        print(e.args)
"#])
        .output()
        .expect("spawn python");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        r#"SyntaxError ("'(' was never closed", ('<string>', 1, 5, 'x = (\n', 1, 0)) '(' was never closed (<string>, line 1)
SyntaxError ('unterminated string literal (detected at line 1)', ('<string>', 1, 1, '"abc', 1, 1)) unterminated string literal (detected at line 1) (<string>, line 1)
SyntaxError ('invalid syntax', ('<string>', 1, 7, 'def f(:\n', 1, 8)) invalid syntax (<string>, line 1)
SyntaxError ('invalid syntax', ('<string>', 1, 4, '1 +\n', 1, 5)) invalid syntax (<string>, line 1)
SyntaxError ("expected ':'", ('<string>', 1, 5, 'if x\n', 1, 6)) expected ':' (<string>, line 1)
SyntaxError ("Missing parentheses in call to 'print'. Did you mean print(...)?", ('<string>', 1, 1, 'print 1\n', 1, 8)) Missing parentheses in call to 'print'. Did you mean print(...)? (<string>, line 1)
SyntaxError ('invalid syntax', ('<string>', 1, 3, 'a b\n', 1, 4)) invalid syntax (<string>, line 1)
SyntaxError ("'[' was never closed", ('<string>', 1, 5, 'x = [1, 2\n', 1, 0)) '[' was never closed (<string>, line 1)
SyntaxError ("'return' outside function", ('<string>', 1, 1, None, 1, 7)) 'return' outside function (<string>, line 1)
SyntaxError ("'break' outside loop", ('<string>', 1, 1, None, 1, 6)) 'break' outside loop (<string>, line 1)
IndentationError ('unexpected indent', ('<string>', 2, 2, '  y = 2\n', 2, -1)) unexpected indent (<string>, line 2)
IndentationError ("expected an indented block after 'if' statement on line 1", ('<string>', 2, 1, 'pass\n', 2, 5)) expected an indented block after 'if' statement on line 1 (<string>, line 2)
SyntaxError ("unmatched ')'", ('<string>', 1, 5, 'x = )', 1, 5)) unmatched ')' (<string>, line 1)
SyntaxError ("closing parenthesis ')' does not match opening parenthesis '[' on line 1", ('<string>', 2, 3, ' 2)', 2, 3)) closing parenthesis ')' does not match opening parenthesis '[' on line 1 (<string>, line 2)
('invalid syntax', ('<string>', 1, 0, '1 +', 1, 0))
("'(' was never closed", ('<string>', 1, 1, '(1', 1, 0))
('invalid syntax', ('<string>', 1, 3, 'a b', 1, 4))
"#
    );
}

/// The position, message and `args` of a broader set of syntax errors: the
/// parameter-list rules, imports, missing colons, a forgotten comma, a bare
/// generator argument, number literals, and a `try` with no handler.
#[test]
fn syntax_error_positions_across_the_grammar() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args([
            "-c",
            r#"cases = [
    "def f(*): pass", "lambda *: 1", "def f(a, a): pass", "lambda x, x: 1",
    "def f(a, b=1, c): pass", "def f(*a, *b): pass", "def f(**k, a): pass",
    "nonlocal x", "async x", "a := 1", "lambda: yield",
    "import", "from . import", "from x", "import a b",
    "while True print(1)", "lambda x x: 1", "else: pass", "try x", "def f() x: pass",
    "f(a b)", "[1 2]", "(a, b c)", "{1 2}", "x[a b]", "f(a, b=1 c)", "(print 1)",
    "f(x for x in y, 1)", "f(x for x in y,)", "x = {1: 2, 3}",
    "0777", "x = 007", "1_", "1__0", "0x", "0b2", "0o8", "0x_",
    "try:\n  pass", "try:\n  pass\nx = 1", "try: pass", "try:\n  x = 1\nelse:\n  pass",
]
for s in cases:
    try:
        compile_ok = exec(s, {})
    except SyntaxError as e:
        print(repr(s), type(e).__name__, e.args, e.lineno, e.offset, e.end_lineno, e.end_offset)
"#,
        ])
        .output()
        .expect("spawn python");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        r#"'def f(*): pass' SyntaxError ('named arguments must follow bare *', ('<string>', 1, 7, 'def f(*): pass\n', 1, 8)) 1 7 1 8
'lambda *: 1' SyntaxError ('named arguments must follow bare *', ('<string>', 1, 9, 'lambda *: 1\n', 1, 10)) 1 9 1 10
'def f(a, a): pass' SyntaxError ("duplicate argument 'a' in function definition",) 1 10 1 11
'lambda x, x: 1' SyntaxError ("duplicate argument 'x' in function definition",) 1 11 1 12
'def f(a, b=1, c): pass' SyntaxError ('parameter without a default follows parameter with a default', ('<string>', 1, 15, 'def f(a, b=1, c): pass\n', 1, 16)) 1 15 1 16
'def f(*a, *b): pass' SyntaxError ('* argument may appear only once', ('<string>', 1, 11, 'def f(*a, *b): pass\n', 1, 12)) 1 11 1 12
'def f(**k, a): pass' SyntaxError ('arguments cannot follow var-keyword argument', ('<string>', 1, 12, 'def f(**k, a): pass\n', 1, 13)) 1 12 1 13
'nonlocal x' SyntaxError ('nonlocal declaration not allowed at module level',) 1 1 1 11
'async x' SyntaxError ('invalid syntax', ('<string>', 1, 7, 'async x\n', 1, 8)) 1 7 1 8
'a := 1' SyntaxError ('invalid syntax', ('<string>', 1, 3, 'a := 1\n', 1, 5)) 1 3 1 5
'lambda: yield' SyntaxError ('invalid syntax', ('<string>', 1, 9, 'lambda: yield\n', 1, 14)) 1 9 1 14
'import' SyntaxError ("Expected one or more names after 'import'", ('<string>', 1, 7, 'import\n', 1, 7)) 1 7 1 7
'from . import' SyntaxError ("Expected one or more names after 'import'", ('<string>', 1, 14, 'from . import\n', 1, 14)) 1 14 1 14
'from x' SyntaxError ('invalid syntax', ('<string>', 1, 7, 'from x\n', 1, 8)) 1 7 1 8
'import a b' SyntaxError ('invalid syntax', ('<string>', 1, 10, 'import a b\n', 1, 11)) 1 10 1 11
'while True print(1)' SyntaxError ('invalid syntax', ('<string>', 1, 12, 'while True print(1)\n', 1, 17)) 1 12 1 17
'lambda x x: 1' SyntaxError ('invalid syntax', ('<string>', 1, 10, 'lambda x x: 1\n', 1, 11)) 1 10 1 11
'else: pass' SyntaxError ('invalid syntax', ('<string>', 1, 1, 'else: pass\n', 1, 5)) 1 1 1 5
'try x' SyntaxError ("expected ':'", ('<string>', 1, 5, 'try x\n', 1, 6)) 1 5 1 6
'def f() x: pass' SyntaxError ("expected ':'", ('<string>', 1, 9, 'def f() x: pass\n', 1, 10)) 1 9 1 10
'f(a b)' SyntaxError ('invalid syntax. Perhaps you forgot a comma?', ('<string>', 1, 3, 'f(a b)\n', 1, 6)) 1 3 1 6
'[1 2]' SyntaxError ('invalid syntax. Perhaps you forgot a comma?', ('<string>', 1, 2, '[1 2]\n', 1, 5)) 1 2 1 5
'(a, b c)' SyntaxError ('invalid syntax. Perhaps you forgot a comma?', ('<string>', 1, 5, '(a, b c)\n', 1, 8)) 1 5 1 8
'{1 2}' SyntaxError ('invalid syntax. Perhaps you forgot a comma?', ('<string>', 1, 2, '{1 2}\n', 1, 5)) 1 2 1 5
'x[a b]' SyntaxError ('invalid syntax. Perhaps you forgot a comma?', ('<string>', 1, 3, 'x[a b]\n', 1, 6)) 1 3 1 6
'f(a, b=1 c)' SyntaxError ('invalid syntax. Perhaps you forgot a comma?', ('<string>', 1, 8, 'f(a, b=1 c)\n', 1, 11)) 1 8 1 11
'(print 1)' SyntaxError ("Missing parentheses in call to 'print'. Did you mean print(...)?", ('<string>', 1, 2, '(print 1)\n', 1, 9)) 1 2 1 9
'f(x for x in y, 1)' SyntaxError ('Generator expression must be parenthesized', ('<string>', 1, 3, 'f(x for x in y, 1)\n', 1, 15)) 1 3 1 15
'f(x for x in y,)' SyntaxError ('Generator expression must be parenthesized', ('<string>', 1, 3, 'f(x for x in y,)\n', 1, 15)) 1 3 1 15
'x = {1: 2, 3}' SyntaxError ("':' expected after dictionary key", ('<string>', 1, 12, 'x = {1: 2, 3}\n', 1, 0)) 1 12 1 0
'0777' SyntaxError ('leading zeros in decimal integer literals are not permitted; use an 0o prefix for octal integers', ('<string>', 1, 1, '0777', 1, 2)) 1 1 1 2
'x = 007' SyntaxError ('leading zeros in decimal integer literals are not permitted; use an 0o prefix for octal integers', ('<string>', 1, 5, 'x = 007', 1, 7)) 1 5 1 7
'1_' SyntaxError ('invalid decimal literal', ('<string>', 1, 2, '1_', 1, 2)) 1 2 1 2
'1__0' SyntaxError ('invalid decimal literal', ('<string>', 1, 2, '1__0', 1, 2)) 1 2 1 2
'0x' SyntaxError ('invalid hexadecimal literal', ('<string>', 1, 2, '0x', 1, 2)) 1 2 1 2
'0b2' SyntaxError ("invalid digit '2' in binary literal", ('<string>', 1, 3, '0b2', 1, 3)) 1 3 1 3
'0o8' SyntaxError ("invalid digit '8' in octal literal", ('<string>', 1, 3, '0o8', 1, 3)) 1 3 1 3
'0x_' SyntaxError ('invalid hexadecimal literal', ('<string>', 1, 3, '0x_', 1, 3)) 1 3 1 3
'try:\n  pass' SyntaxError ("expected 'except' or 'finally' block", ('<string>', 2, 7, '  pass\n', 2, -1)) 2 7 2 -1
'try:\n  pass\nx = 1' SyntaxError ("expected 'except' or 'finally' block", ('<string>', 3, 1, 'x = 1\n', 3, 2)) 3 1 3 2
'try: pass' SyntaxError ("expected 'except' or 'finally' block", ('<string>', 1, 10, 'try: pass\n', 1, -1)) 1 10 1 -1
'try:\n  x = 1\nelse:\n  pass' SyntaxError ("expected 'except' or 'finally' block", ('<string>', 3, 1, 'else:\n', 3, 5)) 3 1 3 5
"#
    );
}

/// Assignment, augmented-assignment and `del` targets that are not targets:
/// CPython names the offending expression and, for a lone `x = y` whose
/// target is an ordinary expression, suggests `==`.
#[test]
fn invalid_targets_are_named_at_the_offending_expression() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args(["-c", r#"for s in ["1 = x", "f() = 1", "a + 1 = 2", "None = 1", "x.y() = 1", "[1] = x", "(a, 1) = x", "1 = x = y", "x = 1 = y", "f() += 1", "(a, b) += 1", "[a] += 1", "1 += 1", "del f()", "del 1", "del (a, 1)", "del a + b", "x() = y = 3", "\"s\" = 1", "a if b else c = 1", "lambda: 1 = 2", "not a = 1", "a < b = 1", "-a = 1", "{1: 2} = x", "{1} = x", "... = x", "True = 1", "f\"x\" = 1", "[x for x in y] = 1", "x = yield = 3", "a, (b, 2) = c", "*a, 1 = x", "del a, f()", "a = b", "a.b = c", "a[0], b = c", "del a, b", "a[1:2] = c", "(a) = 1", "((a, b)) = 1, 2", "[a, *b] = c"]:
  try: exec(s, {'b': [1,2], 'c': [1,2], 'a': [1,2,3]})
  except SyntaxError as e: print(repr(s), e.args)
  except Exception as e: print(repr(s), "RT", type(e).__name__)
  else: print(repr(s), "OK")
"#])
        .output()
        .expect("spawn python");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        r#"'1 = x' ("cannot assign to literal here. Maybe you meant '==' instead of '='?", ('<string>', 1, 1, '1 = x\n', 1, 2))
'f() = 1' ("cannot assign to function call here. Maybe you meant '==' instead of '='?", ('<string>', 1, 1, 'f() = 1\n', 1, 4))
'a + 1 = 2' ("cannot assign to expression here. Maybe you meant '==' instead of '='?", ('<string>', 1, 1, 'a + 1 = 2\n', 1, 6))
'None = 1' ('cannot assign to None', ('<string>', 1, 1, 'None = 1\n', 1, 5))
'x.y() = 1' ("cannot assign to function call here. Maybe you meant '==' instead of '='?", ('<string>', 1, 1, 'x.y() = 1\n', 1, 6))
'[1] = x' ('cannot assign to literal', ('<string>', 1, 2, '[1] = x\n', 1, 3))
'(a, 1) = x' ('cannot assign to literal', ('<string>', 1, 5, '(a, 1) = x\n', 1, 6))
'1 = x = y' ('cannot assign to literal', ('<string>', 1, 1, '1 = x = y\n', 1, 2))
'x = 1 = y' ('cannot assign to literal', ('<string>', 1, 5, 'x = 1 = y\n', 1, 6))
'f() += 1' ("'function call' is an illegal expression for augmented assignment", ('<string>', 1, 1, 'f() += 1\n', 1, 4))
'(a, b) += 1' ("'tuple' is an illegal expression for augmented assignment", ('<string>', 1, 1, '(a, b) += 1\n', 1, 7))
'[a] += 1' ("'list' is an illegal expression for augmented assignment", ('<string>', 1, 1, '[a] += 1\n', 1, 4))
'1 += 1' ("'literal' is an illegal expression for augmented assignment", ('<string>', 1, 1, '1 += 1\n', 1, 2))
'del f()' ('cannot delete function call', ('<string>', 1, 5, 'del f()\n', 1, 8))
'del 1' ('cannot delete literal', ('<string>', 1, 5, 'del 1\n', 1, 6))
'del (a, 1)' ('cannot delete literal', ('<string>', 1, 9, 'del (a, 1)\n', 1, 10))
'del a + b' ('cannot delete expression', ('<string>', 1, 5, 'del a + b\n', 1, 10))
'x() = y = 3' ('cannot assign to function call', ('<string>', 1, 1, 'x() = y = 3\n', 1, 4))
'"s" = 1' ("cannot assign to literal here. Maybe you meant '==' instead of '='?", ('<string>', 1, 1, '"s" = 1\n', 1, 4))
'a if b else c = 1' ('cannot assign to conditional expression', ('<string>', 1, 1, 'a if b else c = 1\n', 1, 14))
'lambda: 1 = 2' ('cannot assign to lambda', ('<string>', 1, 1, 'lambda: 1 = 2\n', 1, 10))
'not a = 1' ('cannot assign to expression', ('<string>', 1, 1, 'not a = 1\n', 1, 6))
'a < b = 1' ('cannot assign to comparison', ('<string>', 1, 1, 'a < b = 1\n', 1, 6))
'-a = 1' ("cannot assign to expression here. Maybe you meant '==' instead of '='?", ('<string>', 1, 1, '-a = 1\n', 1, 3))
'{1: 2} = x' ("cannot assign to dict literal here. Maybe you meant '==' instead of '='?", ('<string>', 1, 1, '{1: 2} = x\n', 1, 7))
'{1} = x' ("cannot assign to set display here. Maybe you meant '==' instead of '='?", ('<string>', 1, 1, '{1} = x\n', 1, 4))
'... = x' ("cannot assign to ellipsis here. Maybe you meant '==' instead of '='?", ('<string>', 1, 1, '... = x\n', 1, 4))
'True = 1' ('cannot assign to True', ('<string>', 1, 1, 'True = 1\n', 1, 5))
'f"x" = 1' ("cannot assign to f-string expression here. Maybe you meant '==' instead of '='?", ('<string>', 1, 1, 'f"x" = 1\n', 1, 5))
'[x for x in y] = 1' ("cannot assign to list comprehension here. Maybe you meant '==' instead of '='?", ('<string>', 1, 1, '[x for x in y] = 1\n', 1, 15))
'x = yield = 3' ('assignment to yield expression not possible', ('<string>', 1, 5, 'x = yield = 3\n', 1, 10))
'a, (b, 2) = c' ('cannot assign to literal', ('<string>', 1, 8, 'a, (b, 2) = c\n', 1, 9))
'*a, 1 = x' ("cannot assign to literal here. Maybe you meant '==' instead of '='?", ('<string>', 1, 5, '*a, 1 = x\n', 1, 6))
'del a, f()' ('cannot delete function call', ('<string>', 1, 8, 'del a, f()\n', 1, 11))
'a = b' OK
'a.b = c' RT AttributeError
'a[0], b = c' OK
'del a, b' OK
'a[1:2] = c' OK
'(a) = 1' OK
'((a, b)) = 1, 2' OK
'[a, *b] = c' OK
"#
    );
}

/// `compile()` — which pythonrs lacked — checks the source in its mode and
/// returns a code object `exec` and `eval` run as that mode under its
/// filename; and what `eval` accepts is one expression list and nothing after.
#[test]
fn compile_returns_code_that_exec_and_eval_run() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args(["-c", r#"c = compile("x = 1\ny = x + 1", "<mine>", "exec")
print(type(c).__name__, c.co_filename, c.co_name, c.co_firstlineno)
ns = {}
exec(c, ns)
print(ns["y"])
e = compile("1 + 2", "f", "eval")
print(eval(e), exec(e), eval(c))
s = compile("1+1", "<s>", "single"); exec(s)
tests = ["compile()", "compile(\"x\")", "compile(\"x\", \"f\")", "compile(1, \"f\", \"exec\")", "compile(\"x\", 1, \"exec\")", "compile(\"x\", \"f\", 1)", "compile(\"x\", \"f\", \"exec\", flags=\"a\")", "compile(b\"x=1\", \"f\", \"exec\").co_filename", "compile(\"x=1\", b\"f\", \"exec\").co_filename", "compile(\"x=1\\x00\", \"f\", \"exec\")", "compile(\"x = 1\", \"f\", \"eval\")", "compile(\"1\\n2\", \"f\", \"eval\")", "compile(\"\", \"f\", \"eval\")", "compile(\"x = (\", \"fn.py\", \"exec\")", "compile(\"x\", \"f\", \"bad\")", "type(compile(\"x=1\", \"f\", \"exec\", 0x400)).__name__", "compile(\"return\", \"r.py\", \"exec\")"]
for t in tests:
    try:
        print(t, "->", repr(eval(t))[:40])
    except SyntaxError as err:
        print(t, "->", type(err).__name__, err.args)
    except Exception as err:
        print(t, "->", type(err).__name__, err.args[0] if err.args else "")
try:
    exec(compile("x = (", "boom.py", "exec"))
except SyntaxError as err:
    print(err.filename, err.lineno)
for s in ["x = 1", "1\n2", "", "  ", "1;2", "1,2", "(1)\n", "\n\n1\n\n", "def f(): pass", "import os", "1 2"]:
    try:
        print(repr(s), eval(s))
    except SyntaxError as e:
        print(repr(s), e.args)
"#])
        .output()
        .expect("spawn python");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        r#"code <mine> <module> 1
2
3 None None
2
compile() -> TypeError compile() missing required argument 'source' (pos 1)
compile("x") -> TypeError compile() missing required argument 'filename' (pos 2)
compile("x", "f") -> TypeError compile() missing required argument 'mode' (pos 3)
compile(1, "f", "exec") -> TypeError compile() arg 1 must be a string, bytes or AST object
compile("x", 1, "exec") -> TypeError expected str, bytes or os.PathLike object, not int
compile("x", "f", 1) -> TypeError compile() argument 'mode' must be str, not int
compile("x", "f", "exec", flags="a") -> TypeError 'str' object cannot be interpreted as an integer
compile(b"x=1", "f", "exec").co_filename -> 'f'
compile("x=1", b"f", "exec").co_filename -> 'f'
compile("x=1\x00", "f", "exec") -> SyntaxError ('source code string cannot contain null bytes',)
compile("x = 1", "f", "eval") -> SyntaxError ('invalid syntax', ('f', 1, 3, 'x = 1', 1, 4))
compile("1\n2", "f", "eval") -> SyntaxError ('invalid syntax', ('f', 2, 1, '2', 2, 2))
compile("", "f", "eval") -> SyntaxError ('invalid syntax', ('f', 0, 0, '', 0, 0))
compile("x = (", "fn.py", "exec") -> SyntaxError ("'(' was never closed", ('fn.py', 1, 5, 'x = (\n', 1, 0))
compile("x", "f", "bad") -> ValueError compile() mode must be 'exec', 'eval' or 'single'
type(compile("x=1", "f", "exec", 0x400)).__name__ -> 'Module'
compile("return", "r.py", "exec") -> SyntaxError ("'return' outside function", ('r.py', 1, 1, None, 1, 7))
boom.py 1
'x = 1' ('invalid syntax', ('<string>', 1, 3, 'x = 1', 1, 4))
'1\n2' ('invalid syntax', ('<string>', 2, 1, '2', 2, 2))
'' ('invalid syntax', ('<string>', 0, 0, '', 0, 0))
'  ' ('invalid syntax', ('<string>', 0, 0, '', 0, 0))
'1;2' ('invalid syntax', ('<string>', 1, 2, '1;2', 1, 3))
'1,2' (1, 2)
'(1)\n' 1
'\n\n1\n\n' 1
'def f(): pass' ('invalid syntax', ('<string>', 1, 1, 'def f(): pass', 1, 4))
'import os' ('invalid syntax', ('<string>', 1, 1, 'import os', 1, 7))
'1 2' ('invalid syntax', ('<string>', 1, 3, '1 2', 1, 4))
"#
    );
}

/// The declaration checks of CPython's symbol table: a `global` or `nonlocal`
/// after the name was already a parameter, used, annotated or bound in the
/// same scope, a name declared both ways, and a `nonlocal` with nothing to
/// bind to — each positioned at the declaring statement, with `args == (msg,)`.
/// What a nested scope does (a comprehension's own names, a `lambda` body) and
/// what `import` binds do not count. A class body inside a function may bind
/// an enclosing function's name with `nonlocal`.
#[test]
fn declaration_errors_point_at_the_global_or_nonlocal_statement() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args([
            "-c",
            r#"cases = [
    "def g(x):\n    global x",
    "def g():\n    x = 1\n    print(x)\n    global x",
    "def g():\n    x: int\n    global x",
    "def g():\n    for x in y:\n        global x",
    "def g():\n    def h(a=x): pass\n    global x",
    "def g():\n    [(x := 1) for y in z]\n    global x",
    "def f():\n    x = 1\n    def g():\n        print(x)\n        nonlocal x",
    "def f():\n    x = 1\n    def g(x):\n        nonlocal x",
    "def g():\n    nonlocal x\n    global x",
    "def f():\n    nonlocal zz",
    "class C:\n    nonlocal q",
    "x = 1\nglobal x",
    "def g():\n    import x\n    global x",
    "def g():\n    [x for y in z]\n    lambda: x\n    global x",
]
for s in cases:
    try:
        exec(s, {})
        print(repr(s), "ok")
    except SyntaxError as e:
        print(repr(s), e.args, e.lineno, e.offset, e.end_lineno, e.end_offset)
def f():
    x = 1
    class C:
        nonlocal x
        x = 5
    return x
print(f())
"#,
        ])
        .output()
        .expect("spawn python");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        r#"'def g(x):\n    global x' ("name 'x' is parameter and global",) 2 5 2 13
'def g():\n    x = 1\n    print(x)\n    global x' ("name 'x' is used prior to global declaration",) 4 5 4 13
'def g():\n    x: int\n    global x' ("annotated name 'x' can't be global",) 3 5 3 13
'def g():\n    for x in y:\n        global x' ("name 'x' is assigned to before global declaration",) 3 9 3 17
'def g():\n    def h(a=x): pass\n    global x' ("name 'x' is used prior to global declaration",) 3 5 3 13
'def g():\n    [(x := 1) for y in z]\n    global x' ("name 'x' is assigned to before global declaration",) 3 5 3 13
'def f():\n    x = 1\n    def g():\n        print(x)\n        nonlocal x' ("name 'x' is used prior to nonlocal declaration",) 5 9 5 19
'def f():\n    x = 1\n    def g(x):\n        nonlocal x' ("name 'x' is parameter and nonlocal",) 4 9 4 19
'def g():\n    nonlocal x\n    global x' ("name 'x' is nonlocal and global",) 2 5 2 15
'def f():\n    nonlocal zz' ("no binding for nonlocal 'zz' found",) 2 5 2 16
'class C:\n    nonlocal q' ("no binding for nonlocal 'q' found",) 2 5 2 15
'x = 1\nglobal x' ("name 'x' is assigned to before global declaration",) 2 1 2 9
'def g():\n    import x\n    global x' ok
'def g():\n    [x for y in z]\n    lambda: x\n    global x' ok
5
"#
    );
}

/// A `yield` or `await` where it cannot run, positioned at the expression: the
/// compiler's `'yield' outside function` (a position tuple with no text), the
/// symbol table's `'await' outside (async) function` and `'yield' inside
/// <comprehension>` (`args == (msg,)`). A comprehension's first iterable and a
/// `lambda` body are not the comprehension's block.
#[test]
fn misplaced_yield_and_await_are_positioned() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args([
            "-c",
            r#"cases = [
    "x = 1\nyield 3",
    "(yield)",
    "class C:\n    yield 1",
    "x = yield from y",
    "await f()",
    "def f():\n    await g()",
    "x = [(yield z) for q in r]",
    "def f():\n    return {k: (yield from v) for k in r}",
    "def f():\n    return ((yield) for q in r)",
    "def f():\n    return [x for c in d if (yield)]",
    "def f():\n    return [x for x in (yield)]",
    "def f():\n    return [lambda: (yield) for c in d]",
]
for s in cases:
    try:
        exec(s)
        print(repr(s), "ok")
    except SyntaxError as e:
        print(repr(s), e.args, e.lineno, e.offset, e.end_lineno, e.end_offset)
"#,
        ])
        .output()
        .expect("spawn python");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        r#"'x = 1\nyield 3' ("'yield' outside function", ('<string>', 2, 1, None, 2, 8)) 2 1 2 8
'(yield)' ("'yield' outside function", ('<string>', 1, 2, None, 1, 7)) 1 2 1 7
'class C:\n    yield 1' ("'yield' outside function", ('<string>', 2, 5, None, 2, 12)) 2 5 2 12
'x = yield from y' ("'yield from' outside function", ('<string>', 1, 5, None, 1, 17)) 1 5 1 17
'await f()' ("'await' outside function",) 1 1 1 10
'def f():\n    await g()' ("'await' outside async function",) 2 5 2 14
'x = [(yield z) for q in r]' ("'yield' inside list comprehension",) 1 7 1 14
'def f():\n    return {k: (yield from v) for k in r}' ("'yield' inside dict comprehension",) 2 17 2 29
'def f():\n    return ((yield) for q in r)' ("'yield' inside generator expression",) 2 14 2 19
'def f():\n    return [x for c in d if (yield)]' ("'yield' inside list comprehension",) 2 30 2 35
'def f():\n    return [x for x in (yield)]' ok
'def f():\n    return [lambda: (yield) for c in d]' ok
"#
    );
}
