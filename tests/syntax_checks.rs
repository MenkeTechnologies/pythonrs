//! Programs CPython refuses at compile time that pythonrs used to run, and
//! valid ones it used to refuse. Each source is compiled and the outcome
//! (`'OK'` or `(type, msg, lineno, offset, end_offset)`) is compared with
//! CPython 3.14.8's. No `python3` is needed.

use pythonrs::{eval_str, host};

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

/// The outcome of compiling each source as a module.
fn outcomes(sources: &[&str]) -> String {
    let list = sources
        .iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let src = format!(
        "def outcome(s):
    try:
        compile(s, '<string>', 'exec')
    except SyntaxError as e:
        return (type(e).__name__, e.msg, e.lineno, e.offset, e.end_offset)
    return 'OK'
r = [outcome(s) for s in [{list}]]
"
    );
    g(&src, "r")
}

#[test]
fn starred_targets_and_values_are_checked() {
    assert_eq!(
        outcomes(&[
            "a, *b, *c = 1",
            "*a = 1",
            "for *a in b: pass",
            "a = *b",
            "del *a",
            "a, (*b, *c) = 1",
            "a, *b = 1, 2",
            "*a, = 1, 2",
            "for a, *b in c: pass",
        ]),
        "[('SyntaxError', 'multiple starred expressions in assignment', 1, 1, 10), \
('SyntaxError', 'starred assignment target must be in a list or tuple', 1, 1, 3), \
('SyntaxError', 'starred assignment target must be in a list or tuple', 1, 5, 7), \
('SyntaxError', \"can't use starred expression here\", 1, 5, 7), \
('SyntaxError', 'cannot delete starred', 1, 5, 7), \
('SyntaxError', 'multiple starred expressions in assignment', 1, 4, 12), \
'OK', 'OK', 'OK']"
    );
}

#[test]
fn unpacking_in_a_comprehension_or_call_is_refused() {
    assert_eq!(
        outcomes(&[
            "[*a for a in b]",
            "(*a for a in b)",
            "{*a for a in b}",
            "{**a for a in b}",
            "print(*)",
            "[*a, *b]",
        ]),
        "[('SyntaxError', 'iterable unpacking cannot be used in comprehension', 1, 2, 4), \
('SyntaxError', 'iterable unpacking cannot be used in comprehension', 1, 2, 4), \
('SyntaxError', 'iterable unpacking cannot be used in comprehension', 1, 2, 4), \
('SyntaxError', 'dict unpacking cannot be used in dict comprehension', 1, 2, 4), \
('SyntaxError', 'Invalid star expression', 1, 8, 9), 'OK']"
    );
}

#[test]
fn import_star_is_module_level_only() {
    assert_eq!(
        outcomes(&[
            "def f():\n    from x import *",
            "class C:\n    from x import *",
            "from x import *",
        ]),
        "[('SyntaxError', 'import * only allowed at module level', 2, 19, 20), \
('SyntaxError', 'import * only allowed at module level', 2, 19, 20), 'OK']"
    );
}

#[test]
fn handler_order_and_except_star_jumps_are_checked() {
    assert_eq!(
        outcomes(&[
            "try: pass\nexcept: pass\nexcept ValueError: pass",
            "try: pass\nexcept* A:\n    break",
            "for x in y:\n    try: pass\n    except* A:\n        for z in w:\n            break",
            "for x in y:\n    try: pass\n    except* A:\n        break",
        ]),
        "[('SyntaxError', \"default 'except:' must be last\", 2, 1, 13), \
('SyntaxError', \"'break', 'continue' and 'return' cannot appear in an except* block\", 3, 5, 10), \
'OK', \
('SyntaxError', \"'break', 'continue' and 'return' cannot appear in an except* block\", 4, 9, 14)]"
    );
}

#[test]
fn match_patterns_follow_the_grammar() {
    assert_eq!(
        outcomes(&[
            "match x:\n    case [*a, *b]: pass",
            "match x:\n    case -x: pass",
            "match x:\n    case 1+2: pass",
            "match x:\n    case f'a': pass",
            "match x:\n    case 1j | -1 | 1-2j | -1.5+2j: pass",
        ]),
        "[('SyntaxError', 'multiple starred names in sequence pattern', 2, 10, 18), \
('SyntaxError', 'invalid syntax', 2, 11, 12), \
('SyntaxError', 'imaginary number required in complex literal', 2, 12, 13), \
('SyntaxError', 'patterns may only match literals and attribute lookups', 2, 10, 14), 'OK']"
    );
    let src = "
def kind(v):
    match v:
        case 1-2j: return 'c1'
        case -1.5+2j: return 'c2'
        case -3: return 'neg'
        case _: return 'other'
r = (kind(1-2j), kind(-1.5+2j), kind(-3), kind(4))
";
    assert_eq!(g(src, "r"), "('c1', 'c2', 'neg', 'other')");
}

#[test]
fn a_number_runs_into_a_name_only_for_keywords() {
    assert_eq!(
        outcomes(&[
            "1if 1else 2",
            "x = 1e",
            "1.a",
            "x = [1for x in y]",
            "0x1for x in y"
        ]),
        "['OK', ('SyntaxError', 'invalid decimal literal', 1, 5, 5), \
('SyntaxError', 'invalid decimal literal', 1, 2, 2), 'OK', 'OK']"
    );
    assert_eq!(
        g("r = (1if 0else 2, [1for _ in 'ab'], 1e3, 1E-2)", "r"),
        "(2, [1, 1], 1000.0, 0.01)"
    );
}

#[test]
fn inconsistent_tabs_and_spaces_are_a_tab_error() {
    assert_eq!(
        outcomes(&[
            "def f():\n\tx = 1\n        y = 2",
            "if 1:\n\tx = 1\n\ty = 2\n",
            "if 1:\n        x = 1\n\ty = 2",
        ]),
        "[('TabError', 'inconsistent use of tabs and spaces in indentation', 3, 1, 0), 'OK', \
('TabError', 'inconsistent use of tabs and spaces in indentation', 3, 1, 0)]"
    );
}

#[test]
fn star_in_a_subscript_and_a_star_annotation_parse() {
    assert_eq!(
        outcomes(&[
            "x[*a]",
            "x[1, *a, 2]",
            "x[*a] = 1",
            "def f(*args: *Ts): pass",
            "del x[*a]"
        ]),
        "['OK', 'OK', 'OK', 'OK', 'OK']"
    );
    let src = "
class M:
    def __getitem__(s, i): return i
class U:
    def __iter__(s): yield 'unpacked'
m = M()
a = [1, 2]
def f(*args: *U()): pass
r = (m[*a], m[0, *a, 3], m[*()], f.__annotations__)
";
    assert_eq!(
        g(src, "r"),
        "((1, 2), (0, 1, 2, 3), (), {'args': 'unpacked'})"
    );
}

#[test]
fn an_fstring_field_may_run_over_lines_and_hold_comments() {
    let src = "
x = 5
def t(v): return v
r = (
    f\"{x +
       1}\",
    f\"{ # it's a comment with } and ' in it
      x }\",
    f'''{x # c
}''',
    f\"{t(
        x
    )}\",
)
";
    assert_eq!(g(src, "r"), "('6', '5', '5', '5')");
    let sources = ["f\"{x:\n}\"", "f\"a\nb{x}\""];
    assert_eq!(
        outcomes(&sources),
        "[('SyntaxError', 'f-string: newlines are not allowed in format specifiers for single quoted f-strings', 1, 6, 6), \
('SyntaxError', 'unterminated f-string literal (detected at line 1)', 1, 1, 1)]"
    );
}
