//! Function and class protocol points the stdlib leans on: `__defaults__`
//! assignment, `cls.__module__` assignment, PEP 560 base resolution, abstract
//! instantiation, frame walking and `ContextVar`.

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
fn assigning_defaults_changes_what_a_call_binds() {
    let src = "\
def f(a, b, c): return (a, b, c)
f.__defaults__ = (2, 3)
r1 = f(1)
f.__defaults__ = (9,)
r2 = f(1, 2)
f.__defaults__ = None
try:
    f(1)
except TypeError as e:
    r3 = str(e)
lam = eval('lambda a, b: (a, b)')
lam.__defaults__ = ('d',)
x = (r1, r2, r3, lam(1), f.__defaults__)
";
    assert_eq!(
        g(src, "x"),
        "((1, 2, 3), (1, 2, 9), \"f() missing 2 required positional arguments: 'b' and 'c'\", (1, 'd'), None)"
    );
}

#[test]
fn class_module_assignment_renames_the_repr() {
    let src = "\
class A: pass
A.__module__ = 'pkg.mod'
B = type('B', (), {})
B.__module__ = 'other'
x = (repr(A), A.__module__, repr(A()).startswith('<pkg.mod.A object'), repr(B))
";
    assert_eq!(
        g(src, "x"),
        "(\"<class 'pkg.mod.A'>\", 'pkg.mod', True, \"<class 'other.B'>\")"
    );
}

#[test]
fn abstract_classes_refuse_instantiation() {
    let src = "\
import abc
class Base(abc.ABC):
    @abc.abstractmethod
    def b(self): ...
    @abc.abstractmethod
    def a(self): ...
class Half(Base):
    def a(self): return 1
class Full(Half):
    def b(self): return 2
msgs = []
for cls in (Base, Half):
    try:
        cls()
    except TypeError as e:
        msgs.append(str(e))
x = (msgs, Full().a() + Full().b())
";
    assert_eq!(
        g(src, "x"),
        "([\"Can't instantiate abstract class Base without an implementation for abstract methods 'a', 'b'\", \"Can't instantiate abstract class Half without an implementation for abstract method 'b'\"], 3)"
    );
}

// The native `ContextVar`; with the bridge on, CPython's own tracks the context
// per OS thread, which libtest's thread-per-test layout makes nondeterministic.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn context_var_reset_restores_the_previous_value() {
    let src = "\
import contextvars
v = contextvars.ContextVar('v', default='d')
t1 = v.set(1)
t2 = v.set(2)
v.reset(t2)
mid = v.get()
v.reset(t1)
x = (mid, v.get(), hasattr(v, 'reset'), t2.old_value, t2.var is v)
";
    assert_eq!(g(src, "x"), "(1, 'd', True, 1, True)");
}

#[test]
fn frames_know_their_module_and_the_stack_depth() {
    let src = "\
import sys
def inner():
    f = sys._getframe(0)
    return f.f_code.co_name, f.f_back.f_code.co_name, f.f_globals['__name__']
def outer(): return inner()
try:
    sys._getframe(10_000)
except ValueError as e:
    deep = str(e)
x = (outer(), deep, sys._getframemodulename(0), sys._getframemodulename(10_000))
";
    assert_eq!(
        g(src, "x"),
        "(('inner', 'outer', '__main__'), 'call stack is not deep enough', '__main__', None)"
    );
}

#[test]
fn generic_alias_bases_resolve_to_their_origin() {
    let src = "\
class Box(list[int]): pass
class Pair(dict[str, int]): pass
b = Box([1, 2])
p = Pair(a=1)
x = (Box.__mro__[1].__name__, Pair.__mro__[1].__name__, b + [3], p['a'], isinstance(b, list))
";
    assert_eq!(g(src, "x"), "('list', 'dict', [1, 2, 3], 1, True)");
}

#[test]
fn mro_entries_hook_replaces_a_non_class_base() {
    let src = "\
class Base:
    tag = 'real'
class Shim:
    def __mro_entries__(self, bases):
        return (Base,)
class C(Shim()): pass
x = (C.tag, C.__mro__[1].__name__)
";
    assert_eq!(g(src, "x"), "('real', 'Base')");
}

/// `__class__` inside a class names the class whose body defines the method
/// reading it (CPython's implicit cell): in the method itself, in a closure
/// nested in it, in a class- or staticmethod and a generator method, and in a
/// subclass's override alongside zero-argument `super()`. A function attached
/// to a class afterwards, and the class body itself, have no such cell.
#[test]
fn dunder_class_names_the_defining_class() {
    let src = "\
class A:
    def m(self):
        return __class__
    def nested(self):
        def inner():
            return __class__
        return inner()
    @classmethod
    def c(cls):
        return __class__
    @staticmethod
    def s():
        return __class__
    def gen(self):
        yield __class__
class B(A):
    def m(self):
        return (__class__, super().m())
def free(self):
    return __class__
A.f = free
errs = []
try:
    A().f()
except NameError as e:
    errs.append(str(e))
try:
    class C:
        x = __class__
except NameError as e:
    errs.append(str(e))
x = (A().m() is A, A().nested() is A, A.c() is A, A.s() is A, next(A().gen()) is A,
     B().m()[0] is B, B().m()[1] is A, B().nested() is A, errs)
";
    assert_eq!(
        g(src, "x"),
        "(True, True, True, True, True, True, True, True, \
         [\"name '__class__' is not defined\", \"name '__class__' is not defined\"])"
    );
}

/// A class call returns the instance `__init__` initialized, and raises
/// CPython's `TypeError` when `__init__` returns anything but `None` — both
/// when the call runs `__init__` on the caller's VM (module code outside any
/// `try`) and when it runs it on a VM of its own (from inside a `try` body).
#[test]
fn a_class_call_returns_its_instance_and_checks_what_init_returned() {
    let src = "\
class P:
    def __init__(self, x, y):
        self.x = x; self.y = y
class Q(P):
    def __init__(self, x):
        super().__init__(x, 2 * x)
class Bad:
    def __init__(self):
        return 1
def guarded():
    try:
        Bad()
    except TypeError as e:
        return str(e)
direct = None
try:
    Bad()
except TypeError as e:
    direct = str(e)
q = Q(3)
x = (P(1, 2).y, q.x, q.y, type(q).__name__, guarded(), direct)
";
    assert_eq!(
        g(src, "x"),
        "(2, 3, 6, 'Q', \"__init__() should return None, not 'int'\", \
         \"__init__() should return None, not 'int'\")"
    );
    let uncaught = eval_str("class Bad:\n    def __init__(self):\n        return 1\nBad()\n");
    assert_eq!(
        uncaught.err().as_deref(),
        Some("TypeError: __init__() should return None, not 'int'")
    );
}

/// Attribute reads and writes on instances keep CPython's precedence however
/// they are answered: a data descriptor and a property beat the instance dict,
/// an instance value shadows a class attribute, a property added to the class
/// after the instance exists takes over (and removing it hands back), a slot
/// beats a same-named dict entry, and user `__getattribute__`, `__setattr__`
/// and `__getattr__` see every access they should.
#[test]
fn instance_attributes_keep_the_lookup_precedence() {
    let src = "\
class D:
    def __get__(self, obj, typ): return 'desc'
    def __set__(self, obj, val): obj.__dict__['seen'] = val
class A:
    k = 'class'
    d = D()
    def __init__(self):
        self.x = 1
a = A()
a.__dict__['d'] = 'shadowed?'
a.k = 'inst'
a.d = 5
r = [a.x, a.k, A.k, a.d, a.seen]
A.x = property(lambda s: 'prop')
r.append(a.x)
del A.x
r.append(a.x)
class G:
    def __getattribute__(self, n): return 'ga:' + n
g = G(); object.__setattr__(g, 'y', 1)
r.append(g.y)
class S:
    def __setattr__(self, n, v): object.__setattr__(self, n, v * 2)
s = S(); s.v = 4
r.append(s.v)
class F:
    def __getattr__(self, n): return 'fallback:' + n
f = F(); f.z = 3
r.append((f.z, f.w))
class Sl:
    __slots__ = ('a',)
class Sub(Sl):
    pass
t = Sub(); t.a = 1; t.__dict__['a'] = 5
r.append((t.a, t.__dict__['a']))
";
    assert_eq!(
        g(src, "r"),
        "[1, 'inst', 'class', 'desc', 5, 'prop', 1, 'ga:y', 8, (3, 'fallback:w'), (1, 5)]"
    );
}

#[test]
fn a_builtin_base_brings_its_own_ancestry_into_the_mro() {
    // `class K(KeyError)` linearized to `(K, KeyError, object)`: the builtin's
    // own chain (`LookupError`, `Exception`, `BaseException`) was dropped, and
    // the C3 merge put `ValueError` ahead of `Exception` in a diamond.
    let src = "\
class K(KeyError): pass
class E(Exception): pass
class M(E, ValueError): pass
x = ([c.__name__ for c in K.__mro__], [c.__name__ for c in type.mro(M)])
";
    assert_eq!(
        g(src, "x"),
        "(['K', 'KeyError', 'LookupError', 'Exception', 'BaseException', 'object'], \
         ['M', 'E', 'ValueError', 'Exception', 'BaseException', 'object'])"
    );
}

#[test]
fn super_reaches_the_slots_of_object_and_builtin_bases() {
    // Past every user class, `super()` resolves `object`'s slots (and an
    // exception base's), both as a call and as an attribute; the proxy also
    // answers its own members.
    let src = "\
class Q:
    def __repr__(self): return 'Q!'
    def r(self): return super().__repr__()[:11]
    def w(self): return type(super().__repr__).__name__, super().__eq__(self), super().__ne__(1)
    def s(self): s = super(); return type(s).__name__, s.__class__.__name__, s.__thisclass__.__name__, s.__self__ is self
class E(Exception):
    def __str__(self): return 'x' + super().__str__()
    def __repr__(self): return 'r' + super().__repr__()
class A:
    def __repr__(self): return 'A'
class B(A):
    def __repr__(self): return 'B' + super().__repr__()
class C(B):
    def __repr__(self): return 'C' + super().__repr__() + super(B, self).__repr__()
q = Q()
x = (q.r(), q.w(), q.s(), str(E('a')), repr(E('b')), repr(C()))
";
    assert_eq!(
        g(src, "x"),
        "('<__main__.Q', ('method-wrapper', True, NotImplemented), ('super', 'super', 'Q', True), \
         'xa', \"rE('b')\", 'CBAA')"
    );
}

#[test]
fn a_builtin_base_answers_attributes_through_the_subclass() {
    // `DD.fromkeys` is a classmethod bound to the SUBCLASS (it builds one,
    // storing through the subclass's `__setitem__`); `DD.get` is the base's
    // own descriptor; `str.upper(s)` runs the base method on the payload.
    let src = "\
class DD(dict):
    def __setitem__(self, k, v): super().__setitem__(k, v * 2)
class I(int): pass
class F(float): pass
class S(str):
    def upper(self): return 'overridden'
d = DD.fromkeys('ab', 1)
x = (type(d).__name__, d, type(I.from_bytes(b'\\x01', 'big')).__name__, type(F.fromhex('0x1p1')).__name__,
     repr(DD.get), DD.get(d, 'a'), str.upper(S('ab')), list.append.__name__)
";
    assert_eq!(
        g(src, "x"),
        "('DD', {'a': 2, 'b': 2}, 'I', 'F', \"<method 'get' of 'dict' objects>\", 2, 'AB', 'append')"
    );
}

#[test]
fn numeric_slots_accept_and_run_on_builtin_subclasses() {
    // `PyLong_Check`/`PyFloat_Check`/`PyUnicode_Check` accept subclasses, so
    // the base slots compute rather than decline, and `round`/`math.trunc` of
    // a subclass with no override run the base type's slot.
    let src = "\
import math
class I(int): pass
class F(float): pass
class S(str): pass
x = ((5).__add__(I(7)), (5).__lt__(I(7)), (2.0).__add__(F(1.0)), 'a'.__eq__(S('a')), 'a'.__lt__(S('b')),
     round(F(2.5)), round(F(2.25), 1), math.trunc(F(-2.5)), round(I(15), -1),
     OSError.__str__(OSError(2, 'no')))
";
    assert_eq!(
        g(src, "x"),
        "(12, True, 3.0, True, True, 2, 2.2, -2, 20, '[Errno 2] no')"
    );
}

#[test]
fn exceptions_reduce_to_their_type_args_and_dict() {
    // `BaseException.__reduce__`: `(type, args)`, plus the instance dict when
    // it holds anything; an `OSError` re-adds the filename it split off.
    let src = "\
class E(Exception): pass
e = E('a', 1)
r1 = e.__reduce__()
e.z = 5
v = ValueError('x')
x = (r1, e.__reduce_ex__(2), v.__reduce__(), Exception.__reduce__(e),
     OSError(2, 'x', 'f').__reduce__(), OSError(2, 'x', 'f', None, 'g').__reduce__())
";
    assert_eq!(
        g(src, "x"),
        "((<class '__main__.E'>, ('a', 1)), (<class '__main__.E'>, ('a', 1), {'z': 5}), \
         (<class 'ValueError'>, ('x',)), (<class '__main__.E'>, ('a', 1), {'z': 5}), \
         (<class 'FileNotFoundError'>, (2, 'x', 'f')), \
         (<class 'FileNotFoundError'>, (2, 'x', 'f', None, 'g')))"
    );
}
