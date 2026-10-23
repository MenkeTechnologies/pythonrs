//! The attribute dicts and class-level descriptors of objects that have no
//! instance dict of their own — functions, builtin exceptions, a class's
//! `mappingproxy` — and `Cls.__weakref__`/`obj.__new__`. Frozen expectations
//! from CPython 3.14.

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

/// `f.__dict__` is the function's attribute store: writes, deletes and a
/// replacement dict are attribute changes, and `functools.wraps`' function
/// slots (`__name__`, `__doc__`, …) stay out of it.
#[test]
fn a_function_dict_is_its_live_attribute_store() {
    let src = r#"
import functools
def f(): pass
f.a = 1
d = f.__dict__; d['b'] = 2
out = [f.b, f.__dict__ is d, dict(d)]
del f.a
out += [dict(f.__dict__), hasattr(f, 'a')]
f.__dict__ = {'z': 3}
out += [f.z]
del f.__dict__['z']
out += [hasattr(f, 'z')]
@functools.wraps(f)
def g(): pass
out += [g.__wrapped__ is f, sorted(g.__dict__)]
def h(): pass
hd = h.__dict__
h.__name__ = 'renamed'
h.__doc__ = 'doc'
out += [dict(hd), h.__name__, h.__doc__]
"#;
    assert_eq!(
        g(src, "out"),
        r#"[2, True, {'a': 1, 'b': 2}, {'b': 2}, False, 3, False, True, ['__wrapped__'], {}, 'renamed', 'doc']"#
    );
}

/// A builtin exception's `__dict__` is live too, with its members kept out.
#[test]
fn a_builtin_exception_dict_is_live() {
    let src = r#"
v = ValueError('x'); v.q = 5
vd = v.__dict__; vd['r'] = 6
out = [v.r, v.__dict__ is vd, dict(vd)]
v.add_note('n')
del v.q
o = OSError(2, 's'); o.__dict__['k'] = 1
out += [dict(v.__dict__), v.__notes__, o.k, o.errno, dict(o.__dict__)]
"#;
    assert_eq!(
        g(src, "out"),
        r#"[6, True, {'q': 5, 'r': 6}, {'r': 6, '__notes__': ['n']}, ['n'], 1, 2, {'k': 1}]"#
    );
}

/// `Cls.__weakref__` is the getset of the class that installed it (one object,
/// as in `Cls.__dict__`), and `obj.__new__` is its type's `__new__`.
#[test]
fn class_weakref_and_instance_new_resolve() {
    let src = r#"
class P: pass
class Q(P): pass
class L(list): pass
class S:
    __slots__ = ('a',)
class E(Exception): pass
out = [repr(c.__weakref__) for c in (P, Q, L, E)]
out += [P.__weakref__ is P.__dict__['__weakref__'], Q.__weakref__ is P.__weakref__, hasattr(S, '__weakref__'),
        E().__new__ is E.__new__, ValueError().__new__ is ValueError.__new__, P().__new__ is object.__new__]
"#;
    assert_eq!(
        g(src, "out"),
        r#"["<attribute '__weakref__' of 'P' objects>", "<attribute '__weakref__' of 'P' objects>", "<attribute '__weakref__' of 'L' objects>", "<attribute '__weakref__' of 'E' objects>", True, True, False, True, True, True]"#
    );
}

/// A class `__dict__` (a `mappingproxy`) has its read methods as attributes.
#[test]
fn a_class_mappingproxy_has_its_read_methods() {
    let src = r#"
class P:
    x = 1
m = P.__dict__
g = m.get
out = [g('x'), m.get('zz', P.__name__), sorted(m.keys())[0], m.copy()['x'], repr(m.items).split(' of ')[0]]
"#;
    assert_eq!(
        g(src, "out"),
        r#"[1, 'P', '__dict__', 1, '<built-in method items']"#
    );
}

/// `ExceptionGroup` is a heap type: it lists `__module__` and `__weakref__`,
/// its `__weakref__` getset is its own, and a subclass inherits it.
#[test]
fn exception_group_carries_its_heap_type_slots() {
    let src = r#"
class E(ExceptionGroup): pass
out = [sorted(set(dir(ExceptionGroup)) - set(dir(BaseExceptionGroup))), repr(ExceptionGroup.__weakref__), repr(E.__weakref__), [n for n in dir(ExceptionGroup) if not hasattr(ExceptionGroup, n)]]
"#;
    assert_eq!(
        g(src, "out"),
        r#"[['__module__', '__weakref__'], "<attribute '__weakref__' of 'ExceptionGroup' objects>", "<attribute '__weakref__' of 'ExceptionGroup' objects>", []]"#
    );
}
