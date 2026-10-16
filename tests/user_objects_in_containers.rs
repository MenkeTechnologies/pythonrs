//! User objects with their own `__hash__`/`__eq__`/`__repr__` inside the
//! `collections` containers and through `copy`. Expectations frozen from
//! CPython 3.14.8.

use pythonrs::{eval_str, host};

/// Run `src`, then return `repr(r)` as the program itself computes it, so a
/// user `__repr__` is honoured.
fn g(src: &str, name: &str) -> String {
    eval_str(&format!("{src}\n__shown = repr({name})\n"))
        .expect("program should run without error");
    host::with_host(|h| {
        let v = h
            .read_global("__shown")
            .unwrap_or_else(|| panic!("global {name} unbound"));
        h.str_of(&v)
    })
}

#[test]
fn collections_find_value_equal_user_keys() {
    let src = r#"
import collections, copy
class P:
    def __init__(s, v): s.v = v
    def __repr__(s): return f'P({s.v})'
    def __eq__(s, o): return isinstance(o, P) and s.v == o.v
    def __hash__(s): return hash(s.v)
a = collections.Counter([P(1), P(1), P(2)])
c = collections.Counter(); c[P(1)] += 1; c[P(1)] += 1
d = collections.defaultdict(list); d[P(1)].append(1); d[P(1)].append(2)
od = collections.OrderedDict([(P(1), 1), (P(2), 2)]); od.move_to_end(P(1))
u = collections.Counter(); u.update([P(3), P(3)]); u.update({P(3): 2})
b = collections.Counter([P(1), P(3)])
dl = collections.Counter([P(1), P(2)]); del dl[P(2)]; del dl[P(9)]
r = (a, c, d, od, u, a - b, a & b, a + b, dl)
"#;
    assert_eq!(
        g(src, "r"),
        "(Counter({P(1): 2, P(2): 1}), Counter({P(1): 2}), defaultdict(<class 'list'>, {P(1): [1, 2]}), \
OrderedDict({P(2): 2, P(1): 1}), Counter({P(3): 4}), Counter({P(1): 1, P(2): 1}), Counter({P(1): 1}), \
Counter({P(1): 3, P(2): 1, P(3): 1}), Counter({P(1): 1}))"
    );
}

#[test]
fn a_bound_method_shows_its_receivers_repr() {
    let src = "
class W:
    def m(s): pass
    def __repr__(s): return 'W!'
r = (W().m, [W().m], str(W().m))
";
    assert_eq!(
        g(src, "r"),
        "(<bound method W.m of W!>, [<bound method W.m of W!>], '<bound method W.m of W!>')"
    );
}

#[test]
fn copy_honours_the_copy_protocol() {
    let src = r#"
import copy
class A:
    def __init__(s, v): s.v = v
    def __repr__(s): return f'A({s.v})'
    def __copy__(s): return A(s.v + 100)
class B:
    def __init__(s, v): s.v = v
    def __repr__(s): return f'B({s.v})'
    def __deepcopy__(s, memo): return B(s.v + 1000)
class C:
    def __init__(s, v): s.v = v
    def __repr__(s): return f'C({s.v})'
    def __reduce__(s): return (C, (s.v + 5,))
class D:
    def __init__(s, v): s.v = v; s.items = [1]
    def __repr__(s): return f'D({s.v},{s.items})'
    def __getstate__(s): return {'v': s.v + 1, 'items': s.items}
    def __setstate__(s, st): s.v = st['v']; s.items = st['items']
class H:
    def __init__(s): s.a = [1]; s.me = s
    def __deepcopy__(s, memo):
        n = H.__new__(H); memo[id(s)] = n
        n.a = copy.deepcopy(s.a, memo); n.me = n
        return n
class K:
    __slots__ = ('x', 'y')
    def __init__(s): s.x = 1; s.y = [2]
class R:
    def __init__(s, v): s.v = v
    def __reduce__(s): return (R, (s.v,), {'extra': 1})
h = H(); h2 = copy.deepcopy(h); l2 = copy.deepcopy([h, h])
k = K(); k2 = copy.copy(k); k3 = copy.deepcopy(k)
d = D(1); dc = copy.copy(d); dd = copy.deepcopy(d)
rr = copy.deepcopy(R([1]))
r = (copy.copy(A(1)), copy.deepcopy(A(1)), copy.copy(B(1)), copy.deepcopy(B(1)),
     copy.copy(C(1)), copy.deepcopy(C(1)), copy.copy(D(1)), dc.items is d.items, dd.items is d.items,
     h2.me is h2, h2.a is h.a, l2[0] is l2[1], k2.y is k.y, k3.y is k.y, rr.v, rr.extra)
"#;
    assert_eq!(
        g(src, "r"),
        "(A(101), A(1), B(1), B(1001), C(6), C(6), D(2,[1]), True, False, True, False, True, True, False, [1], 1)"
    );
}

#[test]
fn copying_a_builtin_subclass_copies_its_storage() {
    let src = "
import copy
class L(list): pass
class D(dict): pass
l = L([1, 2]); l2 = copy.copy(l); l2.append(3); l3 = copy.deepcopy(l); l3.append(4)
d = D(a=[1]); d2 = copy.copy(d); d2['b'] = 1; d3 = copy.deepcopy(d); d3['a'].append(2)
r = (l, l2, l3, d, d2, d3, type(l2).__name__, type(d3).__name__)
";
    assert_eq!(
        g(src, "r"),
        "([1, 2], [1, 2, 3], [1, 2, 4], {'a': [1]}, {'a': [1], 'b': 1}, {'a': [1, 2]}, 'L', 'D')"
    );
}
