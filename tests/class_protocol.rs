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
