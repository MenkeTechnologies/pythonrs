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
    // A dedent matching no outer level is positioned past the end of its line,
    // and it wins over the parse error the parser would hit asking for the
    // token after it (`try:` whose body it ends wanted an `except`).
    (
        "try:\n        y: int\n    print(1)",
        r#"  File "<string>", line 3
    print(1)
            ^
IndentationError: unindent does not match any outer indentation level
"#,
        1,
    ),
    // A decorator that a dedent follows instead of a `def`/`class`: pegen's
    // generic failure on a DEDENT is `unexpected unindent`, positioned past the
    // line's end at the end of input, at the next statement's indent otherwise.
    (
        "try:\n    @dataclass",
        r#"  File "<string>", line 2
    @dataclass
              ^
IndentationError: unexpected unindent
"#,
        1,
    ),
    (
        "try:\n    @d\nx=1",
        r#"  File "<string>", line 3
    x=1
IndentationError: unexpected unindent
"#,
        1,
    ),
    (
        "@dataclass\n    y: int",
        r#"  File "<string>", line 2
    y: int
IndentationError: unexpected indent
"#,
        1,
    ),
    (
        "if 1:\n        y = 1\n    x",
        r#"  File "<string>", line 3
    x
     ^
IndentationError: unindent does not match any outer indentation level
"#,
        1,
    ),
    // A line break inside a string nested in an f-string field is counted once.
    (
        "x = f\"\"\"{f\"\"\"\n\"\"\"}\"\"\"\ny = )",
        r#"  File "<string>", line 3
    y = )
        ^
SyntaxError: unmatched ')'
"#,
        1,
    ),
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

/// The `for`, comprehension and `with` targets CPython's `invalid_for_target`,
/// `invalid_for_if_clause` and `invalid_with_item` name at the offending part
/// (the parenthesized group's contents for a group), `*iterable` after `**`
/// from its comma to the tokenizer's cursor, a string literal that does not
/// decode over the whole literal, and a positional class sub-pattern after a
/// keyword one. A span reaching a later line leaves `text` without its newline.
#[test]
fn for_with_targets_unpacking_and_literals_are_positioned() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args(["-c", r#"for s in ['f(**x, *y)', 'f(**x, *y, *z)', 'f(**x, *y, z)', 'f(a, **x, *y)', 'f(**x, a=1, *y)', 'f(**x,\n  *y)', 'class C(**k, *b): pass', "'\\N{bogus}'", "x = '''a\n\\N{bogus}'''", "x = b'\\x4'", "x = '\\U00110000'", 'for 1 in x: pass', 'for f() in x: pass', 'for x, 1 in y: pass', 'for (x, f()) in y: pass', 'for [*a, 1] in y: pass', 'for 1 x: pass', 'for a + b in x: pass', 'for None in x: pass', 'for (1) in x: pass', 'for a x: pass', 'for a: pass', '[x for 1 in y]', '{x for f() in y}', 'f(x for 1 in y)', '[x for (yield) in y]', '[x for a x in y]', '[x for a]', 'with a as 1: pass', 'with (a as 1, b as c): pass', 'with a as (b, 1): pass', 'with a as b.c, d as 1: pass', '(1) = 2', 'del (1)', 'match x:\n    case C(1, x=1, 2, 3): pass', 'match x:\n    case C(x=1, 2, y=3, 4): pass']:
  try: compile(s, '<s>', 'exec')
  except SyntaxError as e: print(repr(s), (e.msg, e.lineno, e.offset, e.end_lineno, e.end_offset, e.text))
  else: print(repr(s), 'OK')
"#])
        .output()
        .expect("spawn python");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        r#"'f(**x, *y)' ('iterable argument unpacking follows keyword argument unpacking', 1, 6, 1, 10, 'f(**x, *y)\n')
'f(**x, *y, *z)' ('iterable argument unpacking follows keyword argument unpacking', 1, 6, 1, 14, 'f(**x, *y, *z)\n')
'f(**x, *y, z)' ('iterable argument unpacking follows keyword argument unpacking', 1, 6, 1, 12, 'f(**x, *y, z)\n')
'f(a, **x, *y)' ('iterable argument unpacking follows keyword argument unpacking', 1, 9, 1, 13, 'f(a, **x, *y)\n')
'f(**x, a=1, *y)' ('iterable argument unpacking follows keyword argument unpacking', 1, 11, 1, 15, 'f(**x, a=1, *y)\n')
'f(**x,\n  *y)' ('iterable argument unpacking follows keyword argument unpacking', 1, 6, 2, 5, 'f(**x,')
'class C(**k, *b): pass' ('iterable argument unpacking follows keyword argument unpacking', 1, 12, 1, 16, 'class C(**k, *b): pass\n')
"'\\N{bogus}'" ("(unicode error) 'unicodeescape' codec can't decode bytes in position 0-8: unknown Unicode character name", 1, 1, 1, 12, "'\\N{bogus}'\n")
"x = '''a\n\\N{bogus}'''" ("(unicode error) 'unicodeescape' codec can't decode bytes in position 2-10: unknown Unicode character name", 1, 5, 2, 9, "x = '''a")
"x = b'\\x4'" ('(value error) invalid \\x escape at position 0', 1, 5, 1, 11, "x = b'\\x4'\n")
"x = '\\U00110000'" ("(unicode error) 'unicodeescape' codec can't decode bytes in position 0-9: illegal Unicode character", 1, 5, 1, 17, "x = '\\U00110000'\n")
'for 1 in x: pass' ('cannot assign to literal', 1, 5, 1, 6, 'for 1 in x: pass\n')
'for f() in x: pass' ('cannot assign to function call', 1, 5, 1, 8, 'for f() in x: pass\n')
'for x, 1 in y: pass' ('cannot assign to literal', 1, 8, 1, 9, 'for x, 1 in y: pass\n')
'for (x, f()) in y: pass' ('cannot assign to function call', 1, 9, 1, 12, 'for (x, f()) in y: pass\n')
'for [*a, 1] in y: pass' ('cannot assign to literal', 1, 10, 1, 11, 'for [*a, 1] in y: pass\n')
'for 1 x: pass' ('cannot assign to literal', 1, 5, 1, 6, 'for 1 x: pass\n')
'for a + b in x: pass' ('cannot assign to expression', 1, 5, 1, 10, 'for a + b in x: pass\n')
'for None in x: pass' ('cannot assign to None', 1, 5, 1, 9, 'for None in x: pass\n')
'for (1) in x: pass' ('cannot assign to literal', 1, 6, 1, 7, 'for (1) in x: pass\n')
'for a x: pass' ('invalid syntax', 1, 7, 1, 8, 'for a x: pass\n')
'for a: pass' ('invalid syntax', 1, 6, 1, 7, 'for a: pass\n')
'[x for 1 in y]' ('cannot assign to literal', 1, 8, 1, 9, '[x for 1 in y]\n')
'{x for f() in y}' ('cannot assign to function call', 1, 8, 1, 11, '{x for f() in y}\n')
'f(x for 1 in y)' ('cannot assign to literal', 1, 9, 1, 10, 'f(x for 1 in y)\n')
'[x for (yield) in y]' ('cannot assign to yield expression', 1, 9, 1, 14, '[x for (yield) in y]\n')
'[x for a x in y]' ("'in' expected after for-loop variables", 1, 10, 1, 11, '[x for a x in y]\n')
'[x for a]' ("'in' expected after for-loop variables", 1, 9, 1, 10, '[x for a]\n')
'with a as 1: pass' ('cannot assign to literal', 1, 11, 1, 12, 'with a as 1: pass\n')
'with (a as 1, b as c): pass' ('cannot assign to literal', 1, 12, 1, 13, 'with (a as 1, b as c): pass\n')
'with a as (b, 1): pass' ('cannot assign to literal', 1, 15, 1, 16, 'with a as (b, 1): pass\n')
'with a as b.c, d as 1: pass' ('cannot assign to literal', 1, 21, 1, 22, 'with a as b.c, d as 1: pass\n')
'(1) = 2' ("cannot assign to literal here. Maybe you meant '==' instead of '='?", 1, 2, 1, 3, '(1) = 2\n')
'del (1)' ('cannot delete literal', 1, 6, 1, 7, 'del (1)\n')
'match x:\n    case C(1, x=1, 2, 3): pass' ('positional patterns follow keyword patterns', 2, 20, 2, 24, '    case C(1, x=1, 2, 3): pass\n')
'match x:\n    case C(x=1, 2, y=3, 4): pass' ('positional patterns follow keyword patterns', 2, 17, 2, 18, '    case C(x=1, 2, y=3, 4): pass\n')
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

/// A compiler or symbol-table error is positioned in UTF-8 BYTE columns, as
/// `_PyCompile_Error` and `PyErr_RangedSyntaxLocationObject` report the
/// node's `col_offset + 1`: each `é` before the node counts two, `€` three
/// and `𝄞` four. A parser error, which pegen converts, stays in characters.
/// A traceback read from the file draws the caret where CPython draws it —
/// shifted right by the extra bytes.
#[test]
fn compiler_errors_are_positioned_in_utf8_bytes() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args([
            "-c",
            r#"cases = [
    "class C:\n    x = ('éé', (yield 1))",
    "class C:\n    x = ('éé', (yield from 1))",
    "x = ('éé', (await 1))",
    "def f():\n    x = ('éé', (await 1))",
    "x = ('éé', [(yield) for y in z])",
    "x = ('éé'); return 5",
    "x = ('é'); break",
    "x = ('€'); continue",
    "def f():\n  x = ('éé'); nonlocal q",
    "def f(é):\n  x = ('éé'); global é",
    "def f(é, é): pass",
    "'𝄞'; nonlocal x",
    "x = ('éé'; 1)",
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
        r#""class C:\n    x = ('éé', (yield 1))" ("'yield' outside function", ('<string>', 2, 19, None, 2, 26)) 2 19 2 26
"class C:\n    x = ('éé', (yield from 1))" ("'yield from' outside function", ('<string>', 2, 19, None, 2, 31)) 2 19 2 31
"x = ('éé', (await 1))" ("'await' outside function",) 1 15 1 22
"def f():\n    x = ('éé', (await 1))" ("'await' outside async function",) 2 19 2 26
"x = ('éé', [(yield) for y in z])" ("'yield' inside list comprehension",) 1 16 1 21
"x = ('éé'); return 5" ("'return' outside function", ('<string>', 1, 15, None, 1, 23)) 1 15 1 23
"x = ('é'); break" ("'break' outside loop", ('<string>', 1, 13, None, 1, 18)) 1 13 1 18
"x = ('€'); continue" ("'continue' not properly in loop", ('<string>', 1, 14, None, 1, 22)) 1 14 1 22
"def f():\n  x = ('éé'); nonlocal q" ("no binding for nonlocal 'q' found",) 2 17 2 27
"def f(é):\n  x = ('éé'); global é" ("name 'é' is parameter and global",) 2 17 2 26
'def f(é, é): pass' ("duplicate argument 'é' in function definition",) 1 11 1 13
"'𝄞'; nonlocal x" ('nonlocal declaration not allowed at module level',) 1 9 1 19
"x = ('éé'; 1)" ('invalid syntax', ('<string>', 1, 10, "x = ('éé'; 1)\n", 1, 11)) 1 10 1 11
"#
    );
    let dir = std::env::temp_dir().join(format!("pyrs-bytecol-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("y.py");
    std::fs::write(&script, "class C:\n    x = (\"éé\", (yield 1))\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_python")).arg(&script).output().expect("spawn python");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            r#"  File "{}", line 2
    x = ("éé", (yield 1))
                  ^^^^^^^
SyntaxError: 'yield' outside function
"#,
            script.display()
        )
    );
}

/// The compiler's pattern-matching errors, raised at the node CPython's
/// `codegen_pattern_*` passes to `_PyCompile_Error`: `offset`/`end_offset` are
/// UTF-8 byte columns plus one (`'éé'` counts four), and `args` carries the
/// position tuple with no text.
#[test]
fn pattern_errors_are_positioned_at_the_offending_pattern() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args([
            "-c",
            r#"cases = [
    "match x:\n case a | b:\n  pass",
    "match x:\n case _:\n  pass\n case 1:\n  pass",
    "match x:\n case [a, a]: pass",
    "match x:\n case {'k': a, **a}: pass",
    "match x:\n case (a as b) | c: pass",
    "match x:\n case [1, *a] as a: pass",
    "match x:\n case C(a=1, a=2): pass",
    "match x:\n case {'k': 1, 'k': 2}: pass",
    "match x:\n case {\n 1: a,\n 1: b}: pass",
    "match x:\n case [1, a] | [b, 2]: pass",
    "match x:\n case a, b, a,: pass",
    "match x:\n case ('\u00e9\u00e9', a, a): pass",
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
        r#"'match x:\n case a | b:\n  pass' ("name capture 'a' makes remaining patterns unreachable", ('<string>', 2, 7, None, 2, 8)) 2 7 2 8
'match x:\n case _:\n  pass\n case 1:\n  pass' ('wildcard makes remaining patterns unreachable', ('<string>', 2, 7, None, 2, 8)) 2 7 2 8
'match x:\n case [a, a]: pass' ("multiple assignments to name 'a' in pattern", ('<string>', 2, 11, None, 2, 12)) 2 11 2 12
"match x:\n case {'k': a, **a}: pass" ("multiple assignments to name 'a' in pattern", ('<string>', 2, 7, None, 2, 20)) 2 7 2 20
'match x:\n case (a as b) | c: pass' ("name capture 'a' makes remaining patterns unreachable", ('<string>', 2, 8, None, 2, 9)) 2 8 2 9
'match x:\n case [1, *a] as a: pass' ("multiple assignments to name 'a' in pattern", ('<string>', 2, 7, None, 2, 19)) 2 7 2 19
'match x:\n case C(a=1, a=2): pass' ('attribute name repeated in class pattern: a', ('<string>', 2, 16, None, 2, 17)) 2 16 2 17
"match x:\n case {'k': 1, 'k': 2}: pass" ("mapping pattern checks duplicate key ('k')", ('<string>', 2, 7, None, 2, 23)) 2 7 2 23
'match x:\n case {\n 1: a,\n 1: b}: pass' ('mapping pattern checks duplicate key (1)', ('<string>', 2, 7, None, 4, 7)) 2 7 4 7
'match x:\n case [1, a] | [b, 2]: pass' ('alternative patterns bind different names', ('<string>', 2, 7, None, 2, 22)) 2 7 2 22
'match x:\n case a, b, a,: pass' ("multiple assignments to name 'a' in pattern", ('<string>', 2, 13, None, 2, 14)) 2 13 2 14
"match x:\n case ('éé', a, a): pass" ("multiple assignments to name 'a' in pattern", ('<string>', 2, 19, None, 2, 20)) 2 19 2 20
"#
    );
}

/// `traceback`'s keyword-typo hint (`_find_keyword_typos`, 3.14): a bare
/// `invalid syntax` whose source compiles once a name is replaced by a keyword
/// it resembles points at that name and suggests the keyword. Each program is
/// run as `-c`; the expectation is CPython's stderr for it. Covered: a typo
/// fixed into incomplete input (`while x:` at the end, a `def` with no body),
/// the names in f-string replacement fields,
/// a candidate the full compile rejects (`yield` outside a function, so
/// `yiel` gets `del`), no fix that compiles (`whille (x:`, a compound
/// statement after `;`), the ten-name budget, a `Perhaps you forgot a comma`
/// message, a name written in full-width letters, and the hint inside the
/// traceback of an `exec`.
const KEYWORD_TYPOS: &[(&str, &str)] = &[
    (
        "whille x: pass",
        r#"  File "<string>", line 1
    whille x: pass
    ^^^^^^
SyntaxError: invalid syntax. Did you mean 'while'?
"#,
    ),
    (
        "x = 1\nwhille x:",
        r#"  File "<string>", line 2
    whille x:
    ^^^^^^
SyntaxError: invalid syntax. Did you mean 'while'?
"#,
    ),
    (
        "def f():\n  retrun 1",
        r#"  File "<string>", line 2
    retrun 1
    ^^^^^^
SyntaxError: invalid syntax. Did you mean 'return'?
"#,
    ),
    (
        "from os improt path",
        r#"  File "<string>", line 1
    from os improt path
            ^^^^^^
SyntaxError: invalid syntax. Did you mean 'import'?
"#,
    ),
    (
        "for x inn y: pass",
        r#"  File "<string>", line 1
    for x inn y: pass
          ^^^
SyntaxError: invalid syntax. Did you mean 'in'?
"#,
    ),
    (
        "if x:\n    pass\nelsee:\n    pass",
        r#"  File "<string>", line 3
    elsee:
    ^^^^^
SyntaxError: invalid syntax. Did you mean 'else'?
"#,
    ),
    (
        "x = 1 iff y else 2",
        r#"  File "<string>", line 1
    x = 1 iff y else 2
          ^^^
SyntaxError: invalid syntax. Did you mean 'if'?
"#,
    ),
    (
        "a = b c",
        r#"  File "<string>", line 1
    a = b c
          ^
SyntaxError: invalid syntax
"#,
    ),
    (
        "class A:\n  deff f(self):",
        r#"  File "<string>", line 2
    deff f(self):
    ^^^^
SyntaxError: invalid syntax. Did you mean 'def'?
"#,
    ),
    (
        "Whille x: pass",
        r#"  File "<string>", line 1
    Whille x: pass
    ^^^^^^
SyntaxError: invalid syntax. Did you mean 'while'?
"#,
    ),
    (
        "tru: pass",
        r#"  File "<string>", line 1
    tru: pass
    ^^^
SyntaxError: invalid syntax. Did you mean 'try'?
"#,
    ),
    (
        "yiel x",
        r#"  File "<string>", line 1
    yiel x
    ^^^^
SyntaxError: invalid syntax. Did you mean 'del'?
"#,
    ),
    (
        "x = Nonee 1",
        r#"  File "<string>", line 1
    x = Nonee 1
              ^
SyntaxError: invalid syntax
"#,
    ),
    (
        "whille (x:",
        r#"  File "<string>", line 1
    whille (x:
             ^
SyntaxError: invalid syntax
"#,
    ),
    (
        "f\"{a}\" ; whille x: pass",
        r#"  File "<string>", line 1
    f"{a}" ; whille x: pass
                    ^
SyntaxError: invalid syntax
"#,
    ),
    (
        "a b c d e f g h i j k l whille x: pass",
        r#"  File "<string>", line 1
    a b c d e f g h i j k l whille x: pass
      ^
SyntaxError: invalid syntax
"#,
    ),
    (
        "x = (1,\n2 3)",
        r#"  File "<string>", line 2
    2 3)
    ^^^
SyntaxError: invalid syntax. Perhaps you forgot a comma?
"#,
    ),
    (
        "ｗhille x: pass",
        r#"  File "<string>", line 1
    ｗhille x: pass
    ^^^^^^
SyntaxError: invalid syntax. Did you mean 'while'?
"#,
    ),
    // Since 3.12 `tokenize` yields the names in an f-string's replacement
    // fields (expression, `!conv`, nested spec fields, nested f-strings) as
    // NAME tokens, and they use up the ten-name budget.
    (
        "x = f\"{a}{b}{c}{d}{e}{f}{g}{h}{i}{j}\" foor y",
        r#"  File "<string>", line 1
    x = f"{a}{b}{c}{d}{e}{f}{g}{h}{i}{j}" foor y
                                          ^^^^
SyntaxError: invalid syntax
"#,
    ),
    (
        "x = f\"{a!r:>{b}}{c}{d}{e}{f}{g}{h}\" foor y",
        r#"  File "<string>", line 1
    x = f"{a!r:>{b}}{c}{d}{e}{f}{g}{h}" foor y
                                        ^^^^
SyntaxError: invalid syntax
"#,
    ),
    (
        "x = f\"{a!r:>{b}}{c}{d}{e}{f}{g}\" foor y",
        r#"  File "<string>", line 1
    x = f"{a!r:>{b}}{c}{d}{e}{f}{g}" foor y
                                     ^^^^
SyntaxError: invalid syntax. Did you mean 'or'?
"#,
    ),
    (
        "x = f\"\"\"{a}\n{b}{c}{f\"\"\"{d}\n{e}\"\"\"}{f}{g}{h}{i}\"\"\" foor y",
        r#"  File "<string>", line 3
    {e}"""}{f}{g}{h}{i}""" foor y
                           ^^^^
SyntaxError: invalid syntax
"#,
    ),
    (
        "exec(\"x=1\\nimprot os\")",
        r#"Traceback (most recent call last):
  File "<string>", line 1, in <module>
    exec("x=1\nimprot os")
    ~~~~^^^^^^^^^^^^^^^^^^
  File "<string>", line 2
    improt os
    ^^^^^^
SyntaxError: invalid syntax. Did you mean 'import'?
"#,
    ),
];

#[test]
fn misspelled_keywords_are_suggested_as_traceback_suggests_them() {
    for (src, stderr) in KEYWORD_TYPOS {
        assert_eq!(run_c(src), (stderr.to_string(), 1), "for {src:?}");
    }
}

/// `SyntaxError._metadata`: `(0, 0, source)` on an error the tokenizer or the
/// parser raised — the exec source newline-translated and newline-terminated,
/// the eval source as given (leading blanks stripped) — `None` on a compiler
/// or symbol-table error, and the seventh item of the details tuple when
/// constructed. `traceback.format_exception_only` (CPython's own, across the
/// bridge) reads it for the keyword hint.
#[test]
fn syntax_error_metadata_carries_the_parsed_source() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args(["-c", r#"
import traceback
def meta(f, *a):
    try:
        f(*a)
    except SyntaxError as e:
        return e._metadata
print(meta(eval, '1 +* 2\n'))
print(meta(eval, '  1 +* 2'))
print(meta(eval, '1 2'))
print(meta(exec, 'x = 1 +* 2\r\ny'))
print(meta(exec, 'x = "abc'))
print(meta(exec, 'match x:\n case a: pass\n case 1: pass'))
print(meta(exec, 'return 1'))
print(meta(compile, 'x = (1,\n2 3)', 'f', 'exec'))
print(meta(compile, '1 2', 'f', 'eval'))
print(meta(exec, 'def f():\n  global x\n  x = 1\n  nonlocal y'))
e = SyntaxError('m', ('f', 1, 1, 't', 1, 2, (0, 0, 'src')))
print(e._metadata, e.args)
print(SyntaxError('m')._metadata)
e = SyntaxError('invalid syntax', ('f', 1, 8, 'whille x: pass\n', 1, 9, (0, 0, 'whille x: pass\n')))
print(''.join(traceback.format_exception_only(e)), end='')
e = SyntaxError('invalid syntax', ('f', 1, 8, 'whille x: pass\n', 1, 9))
print(''.join(traceback.format_exception_only(e)), end='')
try:
    raise SyntaxError('invalid syntax', ('<x>', 1, 8, 'whille x: pass\n', 1, 9, (0, 0, None)))
except SyntaxError as e:
    print(''.join(traceback.format_exception_only(e)), end='')
"#])
        .output()
        .expect("spawn python");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        r#"(0, 0, '1 +* 2\n')
(0, 0, '1 +* 2')
(0, 0, '1 2')
(0, 0, 'x = 1 +* 2\ny\n')
(0, 0, 'x = "abc\n')
None
None
(0, 0, 'x = (1,\n2 3)\n')
(0, 0, '1 2')
None
(0, 0, 'src') ('m', ('f', 1, 1, 't', 1, 2, (0, 0, 'src')))
None
  File "f", line 1
    whille x: pass
    ^^^^^^
SyntaxError: invalid syntax. Did you mean 'while'?
  File "f", line 1
    whille x: pass
           ^
SyntaxError: invalid syntax
  File "<x>", line 1
    whille x: pass
    ^^^^^^
SyntaxError: invalid syntax. Did you mean 'while'?
"#
    );
}

/// A line continuation with nothing after it is the tokenizer's end of input
/// (`unexpected EOF while parsing`, just past the `\`, end -1) unless a
/// bracket is open; a `\` followed by more of its line is `unexpected
/// character after line continuation character`; and a bracket left open at
/// the end of an indented block is reported as never closed, as it is at
/// module level.
#[test]
fn continuation_and_unclosed_bracket_at_end_of_input() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args(["-c", r#"
for s in ['x = 1 +\\', 'x = 1\ny = 2 + \\', 'x = 1 + \\\n2 + \\', '\\', 'if x:\n    y = \\', 'x = 1 \\ 2', 'x = (1 + \\', 'x = 1 + \\\n\n', 'x = 1 + \\\r\n', 'def f():\n  retrun (1,', 'def f():\n  f(1,', 'class C:\n  def f(self):\n    x = [1,']:
    try:
        compile(s, 's', 'exec')
        print(repr(s), 'ok')
    except SyntaxError as e:
        print(repr(s), e.args)
"#])
        .output()
        .expect("spawn python");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        r#"'x = 1 +\\' ('unexpected EOF while parsing', ('s', 1, 9, 'x = 1 +\\\n', 1, -1))
'x = 1\ny = 2 + \\' ('unexpected EOF while parsing', ('s', 2, 10, 'y = 2 + \\\n', 2, -1))
'x = 1 + \\\n2 + \\' ('unexpected EOF while parsing', ('s', 2, 6, '2 + \\\n', 2, -1))
'\\' ('unexpected EOF while parsing', ('s', 1, 2, '\\\n', 1, -1))
'if x:\n    y = \\' ('unexpected EOF while parsing', ('s', 2, 10, '    y = \\\n', 2, -1))
'x = 1 \\ 2' ('unexpected character after line continuation character', ('s', 1, 8, 'x = 1 \\ 2\n', 1, 0))
'x = (1 + \\' ("'(' was never closed", ('s', 1, 5, 'x = (1 + \\\n', 1, 0))
'x = 1 + \\\n\n' ('invalid syntax', ('s', 2, 1, '\n', 2, 2))
'x = 1 + \\\r\n' ('unexpected EOF while parsing', ('s', 1, 10, 'x = 1 + \\\n', 1, -1))
'def f():\n  retrun (1,' ("'(' was never closed", ('s', 2, 10, '  retrun (1,\n', 2, 0))
'def f():\n  f(1,' ("'(' was never closed", ('s', 2, 4, '  f(1,\n', 2, 0))
'class C:\n  def f(self):\n    x = [1,' ("'[' was never closed", ('s', 3, 9, '    x = [1,\n', 3, 0))
"#
    );
}

/// PEP 758 (3.14): `except A, B:` and `except* A, B:` catch any of the listed
/// types without parentheses (a trailing comma allowed), while `as` still
/// requires them — CPython's `multiple exception types must be parenthesized
/// when using 'as'`, spanning the types through the bound name.
#[test]
fn except_accepts_unparenthesized_types() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args([
            "-c",
            r#"cases = [
    "try: pass\nexcept A, B as e: pass",
    "try: pass\nexcept* A, B as e: pass",
    "try: pass\nexcept A, B,: pass",
    "try: pass\nexcept A,: pass",
    "try: pass\nexcept (A), B: pass",
    "try: pass\nexcept A, B, as e: pass",
]
for s in cases:
    try:
        exec(s)
        print(repr(s), "ok")
    except SyntaxError as e:
        print(repr(s), e.args)
for exc in (KeyError, ValueError, TypeError):
    try:
        try:
            raise exc("m")
        except ValueError, KeyError:
            print("caught", exc.__name__)
    except TypeError:
        print("passed", exc.__name__)
try:
    raise ExceptionGroup("g", [OSError(1), ValueError(2)])
except* OSError, ValueError:
    print("star caught both")
"#,
        ])
        .output()
        .expect("spawn python");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        r#"'try: pass\nexcept A, B as e: pass' ("multiple exception types must be parenthesized when using 'as'", ('<string>', 2, 8, 'except A, B as e: pass\n', 2, 17))
'try: pass\nexcept* A, B as e: pass' ("multiple exception types must be parenthesized when using 'as'", ('<string>', 2, 9, 'except* A, B as e: pass\n', 2, 18))
'try: pass\nexcept A, B,: pass' ok
'try: pass\nexcept A,: pass' ok
'try: pass\nexcept (A), B: pass' ok
'try: pass\nexcept A, B, as e: pass' ("multiple exception types must be parenthesized when using 'as'", ('<string>', 2, 8, 'except A, B, as e: pass\n', 2, 18))
caught KeyError
caught ValueError
passed TypeError
star caught both
"#
    );
}

/// The symbol table's comprehension errors, positioned at the node as
/// CPython's symbol table positions them (`args == (msg,)`): an assignment
/// expression in an iterable (at the `:=` expression), one rebinding an
/// iteration variable or used in a class body's comprehension (at its target),
/// a later clause iterating over a name an earlier condition assigned (at
/// that clause's target), and an asynchronous comprehension outside an async
/// function (at the outermost comprehension of the nearest non-comprehension
/// scope, a span that may cover several lines, as a misplaced `yield` may).
/// An `await` in a generator expression makes it an asynchronous generator
/// expression, legal anywhere.
#[test]
fn symbol_table_comprehension_errors_are_positioned_at_the_node() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args([
            "-c",
            r##"cases = [
 '[i := 0 for i, j in x]',
 '[j for i in x if (j := 1) for j in y]',
 '[(j := 1) for i in x for j in y]',
 'class C:\n  [(z := 1) for x in y]',
 'class C:\n  [(x := 1) for x in y]',
 'def f():\n class C:\n  [(z:=1) for x in y]',
 'class C:\n  [lambda: (z := 1) for x in y]',
 'class C:\n  [x for x in (z := y)]',
 'class C:\n  [[(z := 1) for a in b] for x in y]',
 '[x for x in y if [(x := 1) for q in r]]',
 '[x for x in [(a := 1) for q in r]]',
 '[(a, b) for a in [1] for b in (lambda: (c := 2))()]',
 'x = [y for y in z for w in (q := 1)]',
 '{(k := 1): 2 for k in y}',
 '[x for x in y if (x\n := 1)]',
 '[[await y for y in z] for w in v]',
 'def f():\n  return [[await y for y in z] for w in v]',
 'def f():\n  return [x for x in [await y for y in z]]',
 'def f():\n  return (await y for y in z)',
 'def f():\n  return [(await y for y in z) for q in r]',
 'def f():\n  return [x for x in (await y for y in z)]',
 'def f():\n  return {a: b async for a in c}',
 'def f():\n  return {a async for a in c}',
 'def f():\n  return [a async for a in c\n   if 1]',
 'class C:\n  [await a for a in b]',
 '[x async for x in y]',
 'async def f():\n  def g():\n    [x async for x in y]',
 'async def f():\n  [[x async for x in y] for z in w]',
 'async def f():\n  lambda: [x async for x in y]',
 "def f():\n  (yield\n   1)\nclass C:\n  x = (yield\n 1)",
]
for s in cases:
    try:
        compile(s, '<s>', 'exec')
        print('ok', repr(s))
    except SyntaxError as e:
        print(repr(s), e.args[0], len(e.args), e.lineno, e.offset, e.end_lineno, e.end_offset)
"##,
        ])
        .output()
        .expect("spawn python");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        r##"'[i := 0 for i, j in x]' assignment expression cannot rebind comprehension iteration variable 'i' 1 1 2 1 3
'[j for i in x if (j := 1) for j in y]' comprehension inner loop cannot rebind assignment expression target 'j' 1 1 31 1 32
'[(j := 1) for i in x for j in y]' assignment expression cannot rebind comprehension iteration variable 'j' 1 1 3 1 4
'class C:\n  [(z := 1) for x in y]' assignment expression within a comprehension cannot be used in a class body 1 2 5 2 6
'class C:\n  [(x := 1) for x in y]' assignment expression cannot rebind comprehension iteration variable 'x' 1 2 5 2 6
'def f():\n class C:\n  [(z:=1) for x in y]' assignment expression within a comprehension cannot be used in a class body 1 3 5 3 6
ok 'class C:\n  [lambda: (z := 1) for x in y]'
'class C:\n  [x for x in (z := y)]' assignment expression cannot be used in a comprehension iterable expression 1 2 16 2 22
'class C:\n  [[(z := 1) for a in b] for x in y]' assignment expression within a comprehension cannot be used in a class body 1 2 6 2 7
'[x for x in y if [(x := 1) for q in r]]' assignment expression cannot rebind comprehension iteration variable 'x' 1 1 20 1 21
'[x for x in [(a := 1) for q in r]]' assignment expression cannot be used in a comprehension iterable expression 1 1 15 1 21
'[(a, b) for a in [1] for b in (lambda: (c := 2))()]' assignment expression cannot be used in a comprehension iterable expression 1 1 41 1 47
'x = [y for y in z for w in (q := 1)]' assignment expression cannot be used in a comprehension iterable expression 1 1 29 1 35
'{(k := 1): 2 for k in y}' assignment expression cannot rebind comprehension iteration variable 'k' 1 1 3 1 4
'[x for x in y if (x\n := 1)]' assignment expression cannot rebind comprehension iteration variable 'x' 1 1 19 1 20
'[[await y for y in z] for w in v]' asynchronous comprehension outside of an asynchronous function 1 1 1 1 34
'def f():\n  return [[await y for y in z] for w in v]' asynchronous comprehension outside of an asynchronous function 1 2 10 2 43
'def f():\n  return [x for x in [await y for y in z]]' asynchronous comprehension outside of an asynchronous function 1 2 22 2 42
ok 'def f():\n  return (await y for y in z)'
ok 'def f():\n  return [(await y for y in z) for q in r]'
ok 'def f():\n  return [x for x in (await y for y in z)]'
'def f():\n  return {a: b async for a in c}' asynchronous comprehension outside of an asynchronous function 1 2 10 2 33
'def f():\n  return {a async for a in c}' asynchronous comprehension outside of an asynchronous function 1 2 10 2 30
'def f():\n  return [a async for a in c\n   if 1]' asynchronous comprehension outside of an asynchronous function 1 2 10 3 9
'class C:\n  [await a for a in b]' asynchronous comprehension outside of an asynchronous function 1 2 3 2 23
'[x async for x in y]' asynchronous comprehension outside of an asynchronous function 1 1 1 1 21
'async def f():\n  def g():\n    [x async for x in y]' asynchronous comprehension outside of an asynchronous function 1 3 5 3 25
ok 'async def f():\n  [[x async for x in y] for z in w]'
'async def f():\n  lambda: [x async for x in y]' asynchronous comprehension outside of an asynchronous function 1 2 11 2 31
'def f():\n  (yield\n   1)\nclass C:\n  x = (yield\n 1)' 'yield' outside function 2 5 8 6 3
"##
    );
}

/// `except*` errors: a clause with no type and a `try` mixing `except` with
/// `except*` are the parser's (`invalid_except_star_stmt_indent`,
/// `invalid_try_stmt`), with the source line; a `break`/`continue`/`return`
/// leaving an `except*` body is the compiler's, at the statement. Uncaught
/// from `exec`, such an error renders the inner `File "<string>"` block.
#[test]
fn except_star_errors_are_positioned() {
    let out = Command::new(env!("CARGO_BIN_EXE_python"))
        .args([
            "-c",
            r##"cases = [
 'try:\n  pass\nexcept* ValueError:\n  pass\nexcept TypeError:\n  pass',
 'try:\n  pass\nexcept ValueError:\n  pass\nexcept* TypeError:\n  pass',
 'try:\n  pass\nexcept:\n  pass\nexcept *TypeError as e:\n  pass',
 'try:\n  pass\nexcept*:\n  pass',
 'try:\n  pass\nexcept*\n  pass',
 'try:\n  pass\nexcept* A:\n  pass\nexcept* :\n  pass',
 'try:\n  pass\nexcept ValueError:\n  pass\nexcept* :\n  pass',
 'for x in y:\n  try:\n    pass\n  except* E:\n    break',
 'for x in y:\n  try:\n    pass\n  except* E:\n    if x:\n      continue',
 'def f():\n  try:\n    pass\n  except* E:\n    for q in r:\n      return ("é", 1)',
 'def f():\n  try:\n    pass\n  except* E:\n    for q in r:\n      break\n    def g():\n      return 1',
]
for s in cases:
    try:
        compile(s, '<s>', 'exec')
        print('ok', repr(s))
    except SyntaxError as e:
        print(repr(s), e.args, e.lineno, e.offset, e.end_lineno, e.end_offset)
"##,
        ])
        .output()
        .expect("spawn python");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        r##"'try:\n  pass\nexcept* ValueError:\n  pass\nexcept TypeError:\n  pass' ("cannot have both 'except' and 'except*' on the same 'try'", ('<s>', 5, 1, 'except TypeError:\n', 5, 7)) 5 1 5 7
'try:\n  pass\nexcept ValueError:\n  pass\nexcept* TypeError:\n  pass' ("cannot have both 'except' and 'except*' on the same 'try'", ('<s>', 5, 1, 'except* TypeError:\n', 5, 8)) 5 1 5 8
'try:\n  pass\nexcept:\n  pass\nexcept *TypeError as e:\n  pass' ("cannot have both 'except' and 'except*' on the same 'try'", ('<s>', 5, 1, 'except *TypeError as e:\n', 5, 9)) 5 1 5 9
'try:\n  pass\nexcept*:\n  pass' ('expected one or more exception types', ('<s>', 3, 8, 'except*:\n', 3, 9)) 3 8 3 9
'try:\n  pass\nexcept*\n  pass' ('expected one or more exception types', ('<s>', 3, 8, 'except*\n', 3, 9)) 3 8 3 9
'try:\n  pass\nexcept* A:\n  pass\nexcept* :\n  pass' ('expected one or more exception types', ('<s>', 5, 9, 'except* :\n', 5, 10)) 5 9 5 10
'try:\n  pass\nexcept ValueError:\n  pass\nexcept* :\n  pass' ('invalid syntax', ('<s>', 5, 7, 'except* :\n', 5, 8)) 5 7 5 8
'for x in y:\n  try:\n    pass\n  except* E:\n    break' ("'break', 'continue' and 'return' cannot appear in an except* block", ('<s>', 5, 5, None, 5, 10)) 5 5 5 10
'for x in y:\n  try:\n    pass\n  except* E:\n    if x:\n      continue' ("'break', 'continue' and 'return' cannot appear in an except* block", ('<s>', 6, 7, None, 6, 15)) 6 7 6 15
'def f():\n  try:\n    pass\n  except* E:\n    for q in r:\n      return ("é", 1)' ("'break', 'continue' and 'return' cannot appear in an except* block", ('<s>', 6, 7, None, 6, 23)) 6 7 6 23
ok 'def f():\n  try:\n    pass\n  except* E:\n    for q in r:\n      break\n    def g():\n      return 1'
"##
    );
    assert_eq!(
        run_c("exec('for x in y:\\n  try:\\n    pass\\n  except* E:\\n    break')"),
        (
            r#"Traceback (most recent call last):
  File "<string>", line 1, in <module>
    exec('for x in y:\n  try:\n    pass\n  except* E:\n    break')
    ~~~~^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
  File "<string>", line 5
SyntaxError: 'break', 'continue' and 'return' cannot appear in an except* block
"#
            .to_string(),
            1
        )
    );
}
