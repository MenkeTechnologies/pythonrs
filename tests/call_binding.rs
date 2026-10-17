//! Argument binding for functions whose parameters live in frame slots
//! (`FuncDef::params_in_slots`). Such a call binds its arguments straight into
//! slots instead of a named environment, so every signature shape and every
//! argument error must still come out exactly as CPython produces them. The
//! expected values are CPython 3.15's output for the same program.

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

/// Every function here is slot-eligible: no nested scope, no `try`, no name
/// introspection. Each call shape reaches the slots through a different part
/// of the binder: exact positionals, keywords, defaults, `*args`, keyword-only,
/// `**kwargs`, positional-only, unpacked calls, a shared mutable default,
/// recursion, and a method whose zero-argument `super()` reads its receiver.
const SIGNATURES: &str = "\
def pos(a, b): return (a, b)
def dflt(a, b=2, c=3): return (a, b, c)
def star(a, *rest): return (a, rest)
def kwo(a, *, k, m=5): return (a, k, m)
def kw(a, **extra): return (a, sorted(extra.items()))
def po(a, /, b): return (a, b)
def mix(a, b=1, *args, c, d=4, **kw): return (a, b, args, c, d, sorted(kw.items()))
def acc(x, into=[]):
    into.append(x)
    return into
def fact(n): return 1 if n < 2 else n * fact(n - 1)
class Base:
    def m(self, x): return ('base', x)
class Sub(Base):
    def m(self, x): return ('sub', super().m(x + 1))
";

#[test]
fn every_call_shape_binds_into_the_parameter_slots() {
    let src = format!(
        "{SIGNATURES}\
r = [pos(1, 2), pos(b=2, a=1), dflt(1), dflt(1, c=9), dflt(1, 5), star(1), star(1, 2, 3),
     kwo(1, k=2), kwo(1, m=0, k=2), kw(1, x=2, y=3), po(1, b=2), po(1, 2),
     mix(0, c=3), mix(0, 1, 2, 3, c=5, z=6), mix(*[7, 8, 9], **{{'c': 1, 'q': 2}}),
     acc(1), acc(2), fact(10), Sub().m(1), Base.m(Sub(), 5)]
"
    );
    assert_eq!(
        g(&src, "r"),
        "[(1, 2), (1, 2), (1, 2, 3), (1, 2, 9), (1, 5, 3), (1, ()), (1, (2, 3)), \
         (1, 2, 5), (1, 2, 0), (1, [('x', 2), ('y', 3)]), (1, 2), (1, 2), \
         (0, 1, (), 3, 4, []), (0, 1, (2, 3), 5, 4, [('z', 6)]), \
         (7, 8, (9,), 1, 4, [('q', 2)]), [1, 2], [1, 2], 3628800, \
         ('sub', ('base', 2)), ('base', 5)]"
    );
}

/// A call the binder rejects never reaches the slots; its `TypeError` text is
/// CPython's, in CPython's precedence order.
#[test]
fn argument_errors_are_cpythons_for_slotted_functions() {
    let src = format!(
        "{SIGNATURES}\
errs = []
for call in (lambda: pos(1), lambda: pos(1, 2, 3), lambda: pos(1, 2, a=3), lambda: pos(1, c=2),
             lambda: po(a=1, b=2), lambda: kwo(1), lambda: dflt(), lambda: star()):
    try:
        call()
    except TypeError as e:
        errs.append(str(e))
"
    );
    assert_eq!(
        g(&src, "errs"),
        "[\"pos() missing 1 required positional argument: 'b'\", \
         'pos() takes 2 positional arguments but 3 were given', \
         \"pos() got multiple values for argument 'a'\", \
         \"pos() got an unexpected keyword argument 'c'\", \
         \"po() got some positional-only arguments passed as keyword arguments: 'a'\", \
         \"kwo() missing 1 required keyword-only argument: 'k'\", \
         \"dflt() missing 1 required positional argument: 'a'\", \
         \"star() missing 1 required positional argument: 'a'\"]"
    );
}
