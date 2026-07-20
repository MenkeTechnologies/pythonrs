//! Headless parity tests for `bytes`/`bytearray` operations and `str.format`
//! nested field specs. Each snippet binds a global whose `repr` is compared to
//! the value CPython 3.14 produces for the same program — no `python3` needed,
//! so these run in CI.

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

#[test]
fn bytes_slice_concat_repeat() {
    assert_eq!(g("x = b'hello'[1:3]", "x"), "b'el'");
    assert_eq!(g("x = b'hello'[::-1]", "x"), "b'olleh'");
    assert_eq!(g("x = b'abcdef'[::2]", "x"), "b'ace'");
    assert_eq!(g("x = b'ab' + b'cd'", "x"), "b'abcd'");
    assert_eq!(g("x = b'ab' * 3", "x"), "b'ababab'");
    assert_eq!(g("x = 2 * b'xy'", "x"), "b'xyxy'");
    // concat/repeat result type follows the sequence operand.
    assert_eq!(g("x = b'a' + bytearray(b'b')", "x"), "b'ab'");
    assert_eq!(g("x = bytearray(b'a') + b'b'", "x"), "bytearray(b'ab')");
    assert_eq!(g("x = bytearray(b'z') * 2", "x"), "bytearray(b'zz')");
    assert_eq!(g("x = b'abcdef'[1:]", "x"), "b'bcdef'");
    assert_eq!(g("x = bytearray(b'abcdef')[2:4]", "x"), "bytearray(b'cd')");
}

#[test]
fn bytes_membership() {
    assert_eq!(g("x = b'a' in b'abc'", "x"), "True");
    assert_eq!(g("x = b'bc' in b'abc'", "x"), "True");
    assert_eq!(g("x = b'x' in b'abc'", "x"), "False");
    assert_eq!(g("x = 97 in b'abc'", "x"), "True");
    assert_eq!(g("x = 120 in b'abc'", "x"), "False");
    assert_eq!(g("x = b'' in b'abc'", "x"), "True");
}

#[test]
fn bytes_split_join_partition() {
    assert_eq!(g("x = b'a,b,c'.split(b',')", "x"), "[b'a', b'b', b'c']");
    assert_eq!(g("x = b'  a  b  '.split()", "x"), "[b'a', b'b']");
    assert_eq!(
        g("x = b'a,b,c,d'.rsplit(b',', 2)", "x"),
        "[b'a,b', b'c', b'd']"
    );
    assert_eq!(g("x = b'aXXb'.split(b'X')", "x"), "[b'a', b'', b'b']");
    assert_eq!(g("x = b','.join([b'a', b'b', b'c'])", "x"), "b'a,b,c'");
    assert_eq!(
        g("x = bytearray(b'-').join([b'x', b'y'])", "x"),
        "bytearray(b'x-y')"
    );
    assert_eq!(
        g("x = b'a.b.c'.partition(b'.')", "x"),
        "(b'a', b'.', b'b.c')"
    );
    assert_eq!(
        g("x = b'a.b.c'.rpartition(b'.')", "x"),
        "(b'a.b', b'.', b'c')"
    );
    assert_eq!(g("x = b'foo'.partition(b'x')", "x"), "(b'foo', b'', b'')");
    assert_eq!(g("x = b'foo'.rpartition(b'x')", "x"), "(b'', b'', b'foo')");
}

#[test]
fn bytes_search_and_replace() {
    assert_eq!(g("x = b'abcabc'.find(b'bc')", "x"), "1");
    assert_eq!(g("x = b'abcabc'.rfind(b'bc')", "x"), "4");
    assert_eq!(g("x = b'abcabc'.find(b'z')", "x"), "-1");
    assert_eq!(g("x = b'abcabc'.index(b'c')", "x"), "2");
    assert_eq!(g("x = b'abcabc'.rindex(b'c')", "x"), "5");
    assert_eq!(g("x = b'ababab'.count(b'ab')", "x"), "3");
    assert_eq!(g("x = b'ababa'.count(b'a', 1, 4)", "x"), "1");
    assert_eq!(g("x = b'axbxc'.replace(b'x', b'YY')", "x"), "b'aYYbYYc'");
    assert_eq!(g("x = b'aaa'.replace(b'a', b'b', 2)", "x"), "b'bba'");
    assert_eq!(g("x = b'abc'.replace(b'', b'*')", "x"), "b'*a*b*c*'");
    assert_eq!(
        g("x = bytearray(b'hello').replace(b'l', b'L')", "x"),
        "bytearray(b'heLLo')"
    );
}

#[test]
fn bytes_startswith_strip_case() {
    assert_eq!(g("x = b'Hello'.startswith(b'He')", "x"), "True");
    assert_eq!(g("x = b'Hello'.startswith((b'X', b'He'))", "x"), "True");
    assert_eq!(g("x = b'test.py'.endswith((b'.pyc', b'.py'))", "x"), "True");
    assert_eq!(g("x = b'  hi \\t'.strip()", "x"), "b'hi'");
    assert_eq!(g("x = b'xxhelloxx'.strip(b'x')", "x"), "b'hello'");
    assert_eq!(g("x = b'--a'.lstrip(b'-')", "x"), "b'a'");
    assert_eq!(g("x = b'a--'.rstrip(b'-')", "x"), "b'a'");
    assert_eq!(g("x = b'AbC'.upper()", "x"), "b'ABC'");
    assert_eq!(g("x = b'AbC'.lower()", "x"), "b'abc'");
    assert_eq!(g("x = bytearray(b'AbC').upper()", "x"), "bytearray(b'ABC')");
}

#[test]
fn bytes_splitlines_prefix_suffix() {
    assert_eq!(
        g("x = b'a\\nb\\r\\nc\\rd'.splitlines()", "x"),
        "[b'a', b'b', b'c', b'd']"
    );
    assert_eq!(
        g("x = b'a\\nb\\n'.splitlines(True)", "x"),
        "[b'a\\n', b'b\\n']"
    );
    assert_eq!(g("x = b'abcdef'.removeprefix(b'abc')", "x"), "b'def'");
    assert_eq!(g("x = b'abcdef'.removesuffix(b'def')", "x"), "b'abc'");
    assert_eq!(g("x = b'abcdef'.removeprefix(b'xyz')", "x"), "b'abcdef'");
}

#[test]
fn bytes_construct_and_hex() {
    assert_eq!(g("x = bytes.fromhex('616263')", "x"), "b'abc'");
    assert_eq!(g("x = bytes.fromhex('61 62 63')", "x"), "b'abc'");
    assert_eq!(g("x = bytearray.fromhex('4142')", "x"), "bytearray(b'AB')");
    assert_eq!(g("x = bytes([65, 66, 67])", "x"), "b'ABC'");
    assert_eq!(g("x = bytes(3)", "x"), "b'\\x00\\x00\\x00'");
    assert_eq!(g("x = b'\\x00\\xff\\x80'.hex()", "x"), "'00ff80'");
    assert_eq!(g("x = list(b'abc')", "x"), "[97, 98, 99]");
}

#[test]
fn bytes_repr_quoting() {
    // Default single quote; switch to double quote when a `'` and no `"` present.
    assert_eq!(g("x = b\"a'b\"", "x"), "b\"a'b\"");
    assert_eq!(g("x = b'a\"b'", "x"), "b'a\"b'");
    assert_eq!(g("x = b'a\\'b\"c'", "x"), "b'a\\'b\"c'");
    // bytearray always escapes a single quote, even under a double quote.
    assert_eq!(g("x = bytearray(b\"a'b\")", "x"), "bytearray(b\"a\\'b\")");
    assert_eq!(
        g("x = b'\\x00\\x7f\\x80\\xff'", "x"),
        "b'\\x00\\x7f\\x80\\xff'"
    );
}

#[test]
fn bytearray_item_and_slice_assignment() {
    assert_eq!(
        g("ba = bytearray(b'abc')\nba[0] = 65\nx = ba", "x"),
        "bytearray(b'Abc')"
    );
    assert_eq!(
        g("ba = bytearray(b'abc')\nba[-1] = 90\nx = ba", "x"),
        "bytearray(b'abZ')"
    );
    assert_eq!(
        g("ba = bytearray(b'abc')\nba[1:2] = b'xy'\nx = ba", "x"),
        "bytearray(b'axyc')"
    );
    assert_eq!(
        g("ba = bytearray(b'abcdef')\nba[::2] = b'XYZ'\nx = ba", "x"),
        "bytearray(b'XbYdZf')"
    );
}

#[test]
fn bytes_comparison() {
    assert_eq!(g("x = b'abc' < b'abd'", "x"), "True");
    assert_eq!(g("x = b'abc' == bytearray(b'abc')", "x"), "True");
    assert_eq!(g("x = b'abc' <= b'abc'", "x"), "True");
    assert_eq!(
        g("x = sorted([b'c', b'a', b'b'])", "x"),
        "[b'a', b'b', b'c']"
    );
}

#[test]
fn format_nested_specs() {
    assert_eq!(g("x = '{:{}}'.format('hi', 10)", "x"), "'hi        '");
    assert_eq!(g("x = '{:.{}f}'.format(3.14159, 2)", "x"), "'3.14'");
    assert_eq!(
        g("x = '{:{}.{}f}'.format(3.14159, 8, 2)", "x"),
        "'    3.14'"
    );
    assert_eq!(
        g("x = '{:>{w}.{p}f}'.format(3.14159, w=10, p=2)", "x"),
        "'      3.14'"
    );
    assert_eq!(g("x = '{0:{1}}'.format('ab', 5)", "x"), "'ab   '");
    assert_eq!(g("x = '{:*^{}}'.format('z', 7)", "x"), "'***z***'");
}

#[test]
fn format_keyword_index_attr_fields() {
    assert_eq!(g("x = '{name}'.format(name='v')", "x"), "'v'");
    assert_eq!(g("x = '{a}={b}'.format(a='k', b=99)", "x"), "'k=99'");
    assert_eq!(g("x = '{0}-{1}-{0}'.format('a', 'b')", "x"), "'a-b-a'");
    assert_eq!(g("x = '{0[1]}'.format([10, 20, 30])", "x"), "'20'");
    assert_eq!(g("x = '{d[k]}'.format(d={'k': 'val'})", "x"), "'val'");
    assert_eq!(g("x = '{0.real}'.format(complex(3, 4))", "x"), "'3.0'");
    assert_eq!(
        g("x = '{o[0]}/{o[2]}'.format(o=('p', 'q', 'r'))", "x"),
        "'p/r'"
    );
    assert_eq!(
        g("x = '{v:{a}.{b}f}'.format(v=3.14159, a=9, b=4)", "x"),
        "'   3.1416'"
    );
}
