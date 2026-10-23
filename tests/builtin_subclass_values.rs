//! A subclass of `bytes`, `bytearray` or `complex` IS a value of its base: it
//! carries the base's payload, answers the base's protocol and methods, and
//! converts back to the base. Plus the `tp_new` and `__reduce_ex__` a builtin
//! subclass inherits, and `dir()` of such a class. Frozen expectations from
//! CPython 3.14.

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

/// `class B(bytes)` had no native payload: `B(b'a,b')` reprd as a bare object,
/// was unhashable, not iterable, not subscriptable and had no `bytes` methods.
#[test]
fn a_bytes_subclass_is_a_bytes_value() {
    let src = "
class B(bytes): pass
b = B(b'a,b')
out = [b, b + b'!', type(b + b'!').__name__, b[0], b[1:], len(b), b.upper(),
       b == b'a,b', hash(b) == hash(b'a,b'), {b: 1}[b'a,b'], list(b), b.split(b','),
       bytes(b), B.fromhex('6162'), type(B.fromhex('6162')).__name__, isinstance(b, bytes)]
";
    assert_eq!(
        g(src, "out"),
        "[b'a,b', b'a,b!', 'bytes', 97, b',b', 3, b'A,B', True, True, 1, [97, 44, 98], \
         [b'a', b'b'], b'a,b', b'ab', 'B', True]"
    );
}

/// `bytearray_repr` names `Py_TYPE(self)`, so a subclass reprs under its own
/// name; a subclass `__init__` fills the payload through `super().__init__`.
#[test]
fn a_bytearray_subclass_reprs_under_its_name_and_fills_through_super_init() {
    let src = "
class BA(bytearray): pass
class BA2(bytearray):
    def __init__(self, x):
        super().__init__(x)
        self.tag = 1
ba = BA(b'xy')
ba.append(65)
ba[0] = 66
out = [ba, len(ba), ba + b'z', type(ba + b'z').__name__, bytes(ba), ba.decode(),
       BA2(b'zz'), BA2(b'zz').tag]
";
    assert_eq!(
        g(src, "out"),
        "[BA(b'ByA'), 3, bytearray(b'ByAz'), 'bytearray', b'ByA', 'ByA', BA2(b'zz'), 1]"
    );
}

/// `complex(c)` of a subclass instance reads the inherited `complex.__complex__`.
#[test]
fn a_complex_subclass_is_a_complex_value() {
    let src = "
class C(complex): pass
c = C(3, 4)
out = [c, c * c, abs(c), -c, c == 3+4j, hash(c) == hash(3+4j), complex(c),
       type(complex(c)).__name__, c.real, bool(C())]
";
    assert_eq!(
        g(src, "out"),
        "[(3+4j), (-7+24j), 5.0, (-3-4j), True, True, (3+4j), 'complex', 3.0, False]"
    );
}

/// A `bytes` subclass exports the buffer of its payload: `memoryview(b)` and
/// `bytes.join` take it like a `bytes`.
#[test]
fn a_bytes_subclass_is_a_buffer() {
    let src = "
class B(bytes): pass
class BA(bytearray): pass
b = B(b'a,b')
out = [memoryview(b).tobytes(), b'-'.join([b, BA(b'q')]), memoryview(BA(b'mv'))[0]]
";
    assert_eq!(g(src, "out"), "[b'a,b', b'a,b-q', 109]");
}

/// `cls.__new__` of a builtin subclass is the base's `tp_new` (what
/// `copyreg.__newobj__` calls to unpickle), not `object.__new__`; and a type's
/// `__new__` is a builtin method bound to the type.
#[test]
fn a_builtin_subclass_new_is_its_base_tp_new() {
    let src = "
class S(str): pass
class I(int): pass
class B(bytes): pass
class C(complex): pass
class BA(bytearray): pass
out = [S.__new__(S, 'z'), type(S.__new__(S, 'z')).__name__, I.__new__(I, 5),
       type(I.__new__(I, 5)).__name__, B.__new__(B, b'q'), type(B.__new__(B, b'q')).__name__,
       C.__new__(C, 1, 2), BA.__new__(BA), S.__new__ is str.__new__,
       type(str.__new__).__name__, repr(str.__new__).split(' at ')[0]]
";
    assert_eq!(
        g(src, "out"),
        "['z', 'S', 5, 'I', b'q', 'B', (1+2j), BA(b''), True, 'builtin_function_or_method', \
         '<built-in method __new__ of type object']"
    );
}

/// `bytearray` defines its own `__reduce_ex__` (`_common_reduce`): the type,
/// the bytes decoded latin-1 below protocol 3, and the state.
#[test]
fn a_bytearray_subclass_reduces_with_its_own_recipe() {
    let src = "
class BA(bytearray): pass
out = [BA(b'q').__reduce_ex__(2), BA(b'q').__reduce_ex__(3), BA().__reduce_ex__(2),
       BA(b'q').__reduce__()]
";
    assert_eq!(
        g(src, "out"),
        "[(<class '__main__.BA'>, ('q', 'latin-1'), None), (<class '__main__.BA'>, (b'q',), None), \
         (<class '__main__.BA'>, (), None), (<class '__main__.BA'>, ('q', 'latin-1'), None)]"
    );
}

/// `dir()` of a class built on a builtin type lists what the base defines, and
/// every listed name is one `getattr` on the class produces.
#[test]
fn a_builtin_subclass_dir_lists_its_base_and_agrees_with_getattr() {
    let src = "
class L(list): pass
class S(str): pass
class B(bytes): pass
class BA(bytearray): pass
class C(complex): pass
class E(KeyError): pass
missing = [[n for n in dir(k) if not hasattr(k, n) and n != '__weakref__']
           for k in (L, S, B, BA, C, E)]
listed = ['append' in dir(L), '__add__' in dir(S), 'add_note' in dir(E), 'args' in dir(E),
          repr(L.__add__)]
";
    assert_eq!(g(src, "missing"), "[[], [], [], [], [], []]");
    assert_eq!(
        g(src, "listed"),
        "[True, True, True, True, \"<slot wrapper '__add__' of 'list' objects>\"]"
    );
}

/// `list.__new__` (`PyType_GenericNew`) ignores its arguments; the inherited
/// `__init__` fills the payload after a user `__new__`.
#[test]
fn a_mutable_base_new_ignores_its_arguments() {
    let src = r#"
class L(list): pass
class M(list):
    def __new__(cls, it): return super().__new__(cls, it)
class D(dict):
    def __new__(cls, *a, **k): return super().__new__(cls)
class N(list):
    def __new__(cls, *a): return super().__new__(cls)
    def __init__(self, *a): super().__init__([9])
out = [list.__new__(L, [1]), L.__new__(L, [1, 2]), M([1, 2]), D(a=1), D([(1, 2)]), N(1, 2)]
"#;
    assert_eq!(g(src, "out"), r#"[[], [], [1, 2], {'a': 1}, {1: 2}, [9]]"#);
}

/// `set_repr` and `bytearray`'s repr (its `str` too) name a subclass.
#[test]
fn set_and_bytearray_subclasses_name_themselves() {
    let src = r#"
class S(set): pass
class F(frozenset): pass
class BA(bytearray):
    def __new__(cls, x): return super().__new__(cls, x)
out = [str(S([1])), repr(S()), str(F([2])), repr(F()), str(BA(b'q')), repr(S({1, 2}))]
"#;
    assert_eq!(
        g(src, "out"),
        r#"['S({1})', 'S()', 'F({2})', 'F()', "BA(b'q')", 'S({1, 2})']"#
    );
}

/// A memoryview over a `bytes`/`bytearray` subclass instance reports that
/// instance as `obj`, through sub-views too.
#[test]
fn a_memoryview_reports_the_subclass_instance_as_obj() {
    let src = r#"
class B(bytes): pass
class BA(bytearray): pass
b = B(b'xy'); m = memoryview(b)
ba = BA(b'ab'); mv = memoryview(ba); mv[0] = 90
x = b'q'
out = [m.obj is b, type(m.obj).__name__, memoryview(m).obj is b, m[1:].obj is b, ba, mv.obj is ba, memoryview(x).obj is x]
"#;
    assert_eq!(
        g(src, "out"),
        r#"[True, 'B', True, True, BA(b'Zb'), True, True]"#
    );
}

/// `ExceptionGroup[int]`, a subclass of a builtin with `__class_getitem__`, and
/// the classmethod's repr.
#[test]
fn exception_groups_and_builtin_subclasses_parameterize() {
    let src = r#"
class E(ExceptionGroup): pass
class L(list): pass
out = [repr(ExceptionGroup[int]), BaseExceptionGroup[ValueError].__args__, repr(E[int]), repr(L[int]),
       repr(list.__class_getitem__).split(' at ')[0], repr([].__class_getitem__).split(' at ')[0],
       '__class_getitem__' in dir(BaseExceptionGroup)]
"#;
    assert_eq!(
        g(src, "out"),
        r#"['ExceptionGroup[int]', (<class 'ValueError'>,), '__main__.E[int]', '__main__.L[int]', '<built-in method __class_getitem__ of type object', '<built-in method __class_getitem__ of type object', True]"#
    );
}

/// A strided memoryview slice still reports the original exporter as `obj`.
#[test]
fn a_strided_memoryview_slice_keeps_its_exporter() {
    let src = "
b = b'abcd'; ba = bytearray(b'wxyz')
out = [memoryview(b)[::2].obj is b, memoryview(ba)[::-1].obj is ba, memoryview(b)[::2][::1].obj is b]
";
    assert_eq!(g(src, "out"), "[True, True, True]");
}
