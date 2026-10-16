//! `__prepare__` runs before the class body and seeds the scope the body
//! executes in (PEP 3115); a namespace with no `__name__` of its own gets the
//! `builtins` module's. Frozen expectations from CPython 3.14.

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
fn body_reads_names_seeded_by_prepare() {
    let src = "
class M(type):
    @classmethod
    def __prepare__(mcs, name, bases, **kw):
        return {'z': 1, 'tag': kw.get('q')}
    def __new__(m, n, b, d, **kw):
        return super().__new__(m, n, b, d)
class G(metaclass=M, q=2):
    y = z + 1
    t = tag
r = (G.y, G.t)
";
    assert_eq!(g(src, "r"), "(2, 2)");
}

#[test]
fn prepare_defined_as_plain_function_on_the_metaclass() {
    let src = "
class M(type):
    def __prepare__(name, bases, **kw):
        return {'z': 10}
class F(metaclass=M):
    y = z * 2
r = F.y
";
    assert_eq!(g(src, "r"), "20");
}

#[test]
fn rebinding_a_seeded_name_replays_through_setitem() {
    let src = "
log = []
class NS(dict):
    def __setitem__(s, k, v):
        log.append(k)
        super().__setitem__(k, v)
class M(type):
    @classmethod
    def __prepare__(mcs, name, bases):
        return NS(seed=1)
class C(metaclass=M):
    seed = 5
    other = seed + 1
r = (C.seed, C.other, 'seed' in log, 'other' in log)
";
    assert_eq!(g(src, "r"), "(5, 6, True, True)");
}

#[test]
fn exec_without_dunder_name_resolves_it_from_builtins() {
    let src = "
ns = {}
exec('class E: pass\\nmod = E.__module__\\nnm = __name__', ns)
r = (ns['mod'], ns['nm'])
";
    assert_eq!(g(src, "r"), "('builtins', 'builtins')");
}
