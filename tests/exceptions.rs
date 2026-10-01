//! Exception objects: the attributes, arguments and chaining CPython gives an
//! exception, read back after a snippet runs. Expected values are what CPython
//! 3.14 produces for the same program.

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

/// `AttributeError.obj` and `.name` are what CPython's
/// `set_attribute_error_context` gives an `AttributeError` escaping an
/// attribute read: the receiver and the name, unless the exception already
/// carries either. That holds for a native miss, a fused method call, the
/// `getattr` builtin, a user `__getattr__` raising its own message, and a
/// property whose body misses on ANOTHER object (the inner lookup's context
/// wins); an `AttributeError` raised outside a lookup keeps `None`. The
/// constructor takes `name=`/`obj=` as keyword-only arguments with
/// `getargs.c`'s refusals. Expected values are CPython 3.14's.
#[test]
fn attribute_error_carries_its_obj_and_name() {
    let src = r#"
out = []
class C:
    def __getattr__(s, n): raise AttributeError("custom")
class D:
    other = None
    @property
    def p(self): return self.other.zz
    @property
    def q(self): raise AttributeError('inner', name='x', obj=7)
c, d, lst = C(), D(), []
for f in [lambda: (1).nope, lambda: (1).nope(), lambda: lst.zz, lambda: getattr(2, 'q'), lambda: c.foo, lambda: c.foo(), lambda: d.p, lambda: d.q, lambda: D.zz]:
    try: f()
    except AttributeError as e: out.append((e.name, e.obj is c or e.obj is lst or e.obj is D or e.obj))
def raw(): raise AttributeError('raw')
try: raw()
except AttributeError as e: out.append((e.name, e.obj))
out.append((AttributeError('x', name='n', obj=5).name, AttributeError('x', name='n', obj=5).obj, AttributeError('x', obj=5).args, NameError('x').name))
for s in ["AttributeError('x', nam=1)", "AttributeError(a=1, b=2, c=3)", "NameError(obj=1)", "UnboundLocalError(name=1, obj=2)"]:
    try: eval(s)
    except TypeError as e: out.append(str(e))
"#;
    assert_eq!(
        g(src, "out"),
        "[('nope', 1), ('nope', 1), ('zz', True), ('q', 2), ('foo', True), ('foo', True), \
         ('zz', None), ('x', 7), ('zz', True), (None, None), ('n', 5, ('x',), None), \
         \"AttributeError() got an unexpected keyword argument 'nam'. Did you mean 'name'?\", \
         'AttributeError() takes at most 2 keyword arguments (3 given)', \
         \"NameError() got an unexpected keyword argument 'obj'\", \
         'NameError() takes at most 1 keyword argument (2 given)']"
    );
}

/// `UnicodeDecodeError`/`UnicodeEncodeError`/`UnicodeTranslateError` carry
/// CPython's argument tuple, `(encoding, object, start, end, reason)`, with the
/// five attributes reading it back and `__str__` rendered from it: for an error
/// a codec raised (the bytes and str methods, the utf-16/utf-32 decoders, a text
/// file's encoder) and for one the program constructs, with the constructor's
/// argument checks. Expected values are CPython 3.14's.
#[test]
fn unicode_errors_carry_their_five_tuple() {
    let src = r#"
out = []
def args_of(f):
    try: f()
    except UnicodeError as e: return (type(e).__name__, e.args, (e.encoding, e.object, e.start, e.end, e.reason), str(e))
out.append(args_of(lambda: b'a\xffb'.decode('utf-8')))
out.append(args_of(lambda: bytearray(b'\xe2\x82').decode()))
out.append(args_of(lambda: 'a\xe9€b'.encode('ascii')))
out.append(args_of(lambda: '€'.encode('latin-1')))
out.append(args_of(lambda: b'\x00\xd8a'.decode('utf-16-le')))
out.append(args_of(lambda: b'a\x00\x00\xdc'.decode('utf-16-le')))
out.append(args_of(lambda: b'a\x00\x00\x00\x00\xd8\x00\x00'.decode('utf-32-le')))
out.append(args_of(lambda: open('/dev/null', 'w', encoding='ascii').write('a\xe9€b')))
e = UnicodeDecodeError('e', bytearray(b'ab'), 0, 1, 'r')
out.append((e.object, e.args, str(e)))
out.append(str(UnicodeEncodeError('e', '\U0001f600', 0, 1, 'r')))
out.append((str(UnicodeTranslateError('aĀ', 1, 2, 'r')), UnicodeTranslateError('abc', 0, 2, 'r').encoding))
for s in ["UnicodeDecodeError('x')", "UnicodeDecodeError('e', 'a', 0, 1, 'r')", "UnicodeEncodeError('e', b'a', 0, 1, 'r')", "UnicodeDecodeError('e', b'a', 'x', 1, 'r')"]:
    try: eval(s)
    except TypeError as e: out.append(str(e))
"#;
    assert_eq!(g(src, "out"), r#"[('UnicodeDecodeError', ('utf-8', b'a\xffb', 1, 2, 'invalid start byte'), ('utf-8', b'a\xffb', 1, 2, 'invalid start byte'), "'utf-8' codec can't decode byte 0xff in position 1: invalid start byte"), ('UnicodeDecodeError', ('utf-8', b'\xe2\x82', 0, 2, 'unexpected end of data'), ('utf-8', b'\xe2\x82', 0, 2, 'unexpected end of data'), "'utf-8' codec can't decode bytes in position 0-1: unexpected end of data"), ('UnicodeEncodeError', ('ascii', 'aé€b', 1, 3, 'ordinal not in range(128)'), ('ascii', 'aé€b', 1, 3, 'ordinal not in range(128)'), "'ascii' codec can't encode characters in position 1-2: ordinal not in range(128)"), ('UnicodeEncodeError', ('latin-1', '€', 0, 1, 'ordinal not in range(256)'), ('latin-1', '€', 0, 1, 'ordinal not in range(256)'), "'latin-1' codec can't encode character '\\u20ac' in position 0: ordinal not in range(256)"), ('UnicodeDecodeError', ('utf-16-le', b'\x00\xd8a', 0, 3, 'unexpected end of data'), ('utf-16-le', b'\x00\xd8a', 0, 3, 'unexpected end of data'), "'utf-16-le' codec can't decode bytes in position 0-2: unexpected end of data"), ('UnicodeDecodeError', ('utf-16-le', b'a\x00\x00\xdc', 2, 4, 'illegal encoding'), ('utf-16-le', b'a\x00\x00\xdc', 2, 4, 'illegal encoding'), "'utf-16-le' codec can't decode bytes in position 2-3: illegal encoding"), ('UnicodeDecodeError', ('utf-32-le', b'a\x00\x00\x00\x00\xd8\x00\x00', 4, 8, 'code point in surrogate code point range(0xd800, 0xe000)'), ('utf-32-le', b'a\x00\x00\x00\x00\xd8\x00\x00', 4, 8, 'code point in surrogate code point range(0xd800, 0xe000)'), "'utf-32-le' codec can't decode bytes in position 4-7: code point in surrogate code point range(0xd800, 0xe000)"), ('UnicodeEncodeError', ('ascii', 'aé€b', 1, 3, 'ordinal not in range(128)'), ('ascii', 'aé€b', 1, 3, 'ordinal not in range(128)'), "'ascii' codec can't encode characters in position 1-2: ordinal not in range(128)"), (b'ab', ('e', bytearray(b'ab'), 0, 1, 'r'), "'e' codec can't decode byte 0x61 in position 0: r"), "'e' codec can't encode character '\\U0001f600' in position 0: r", ("can't translate character '\\u0100' in position 1: r", None), 'function takes exactly 5 arguments (1 given)', "a bytes-like object is required, not 'str'", 'argument 2 must be str, not bytes', "'str' object cannot be interpreted as an integer"]"#);
}

/// `__str__` reads the attributes, not `args`, as CPython's
/// `UnicodeDecodeError_str` reads the instance fields: reassigning them changes
/// the rendering and leaves `args` alone.
#[test]
fn unicode_error_str_follows_reassigned_attributes() {
    let src = "e = UnicodeDecodeError('e', b'ab', 0, 1, 'r')\n\
               e.reason = 'changed'; e.start = 1\n\
               x = (str(e), e.args)";
    assert_eq!(
        g(src, "x"),
        "(\"'e' codec can't decode bytes in position 1-0: changed\", ('e', b'ab', 0, 1, 'r'))"
    );
}

/// A bare `raise` re-raises the exception its handler caught even after the
/// handler threw that same object into a generator that let it escape (the
/// shape of `contextlib`'s `__exit__`), and `re.PatternError` is a class whose
/// constructor is `re/_constants.py`'s: `msg`, `pattern`, `pos`, `lineno` and
/// `colno`, with the position folded into the one argument. Expected values
/// are CPython 3.14's.
#[test]
fn reraise_after_a_generator_throw_and_pattern_error_construction() {
    let src = r#"
import re
class MyErr(Exception): pass
def g():
    yield
def throw_into(value):
    gen = g(); next(gen)
    try:
        gen.throw(value)
    except BaseException:
        return False
out = []
try:
    try:
        raise MyErr('m')
    except MyErr as v:
        throw_into(v)
        raise
except BaseException as e:
    out.append(repr(e))
for a in [('m',), ('m', 'ab\ncd', 4), ('m', b'ab\ncd', 4), ('m', 'abc', 1), ('m', None, 3)]:
    e = re.PatternError(*a)
    out.append((repr(e), e.args, e.msg, e.pattern, e.pos, e.lineno, e.colno))
out.append(re.PatternError('m', pos=2, pattern='abc').args)
out.append(re.error is re.PatternError and isinstance(re.PatternError('x'), Exception))
out.append([c.__name__ for c in re.error.__mro__])
"#;
    assert_eq!(
        g(src, "out"),
        r#"["MyErr('m')", ("PatternError('m')", ('m',), 'm', None, None, None, None), ("PatternError('m at position 4 (line 2, column 2)')", ('m at position 4 (line 2, column 2)',), 'm', 'ab\ncd', 4, 2, 2), ("PatternError('m at position 4 (line 2, column 2)')", ('m at position 4 (line 2, column 2)',), 'm', b'ab\ncd', 4, 2, 2), ("PatternError('m at position 1')", ('m at position 1',), 'm', 'abc', 1, 1, 2), ("PatternError('m')", ('m',), 'm', None, 3, None, None), ('m at position 2',), True, ['PatternError', 'Exception', 'BaseException', 'object']]"#
    );
}
