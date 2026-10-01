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
