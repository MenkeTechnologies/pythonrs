//! Headless language tests: run a Python snippet that binds a global, then read
//! that global's `repr` back from the host. No `python3` required, so these run
//! in CI. Each snippet exercises a distinct language feature end to end
//! (lex → parse → lower → fusevm execute), and the expected value is the value
//! CPython produces for the same program.

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

// Subscripting a type object (`list[int]`, `dict[str, int]`, a user class) is
// generic parameterization -> a `types.GenericAlias`, not indexing. Gated to the
// self-contained build, which routes through the vendored `types`; the ffi build
// routes through CPython's `types` and is not exercised here.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn generic_subscription_builds_a_genericalias() {
    // `list[int]` is a `types.GenericAlias` carrying origin `list` and args `(int,)`.
    assert_eq!(
        g("x = type(list[int]).__name__ == 'GenericAlias'", "x"),
        "True"
    );
    assert_eq!(g("x = list[int].__origin__ is list", "x"), "True");
    assert_eq!(g("x = list[int].__args__ == (int,)", "x"), "True");
    // Multiple args form a tuple, and every type builds the SAME alias type.
    assert_eq!(g("x = dict[str, int].__args__ == (str, int)", "x"), "True");
    assert_eq!(
        g(
            "import types\nx = type(dict[str, int]) is types.GenericAlias",
            "x"
        ),
        "True",
    );
    // A user class parameterizes ONLY through `__class_getitem__`; when it has
    // one, `__origin__` is the class itself.
    assert_eq!(
        g(
            "import types\n\
             class Box:\n\
             \x20   __class_getitem__ = classmethod(types.GenericAlias)\n\
             x = Box[int].__origin__ is Box",
            "x"
        ),
        "True"
    );
    // A plain class does NOT: being a type is not enough, and CPython names the
    // class in the message rather than its metaclass.
    assert_eq!(
        pythonrs::eval_str("class Box: pass\nx = Box[int]").unwrap_err(),
        "TypeError: type 'Box' is not subscriptable"
    );
    assert_eq!(
        pythonrs::eval_str("x = str[int]").unwrap_err(),
        "TypeError: type 'str' is not subscriptable"
    );
    // A metaclass `__getitem__` makes the class subscriptable as an ordinary
    // indexing operation — it outranks any generic-alias reading.
    assert_eq!(
        g(
            "class Meta(type):\n\
             \x20   def __getitem__(cls, item): return ('meta', item)\n\
             class MC(metaclass=Meta): pass\n\
             x = MC[int]",
            "x"
        ),
        "('meta', <class 'int'>)"
    );
    // A builtin FUNCTION is not a type: subscripting it stays a TypeError.
    // Pinned to CPython 3.14.6's message so a rejection for some OTHER reason
    // (a parse failure, a NameError) cannot satisfy this line.
    assert_eq!(
        pythonrs::eval_str("x = len[0]").expect_err("subscripting len must fail"),
        "TypeError: 'builtin_function_or_method' object is not subscriptable"
    );
    // `tuple[int, ...]` prints the ellipsis in its literal spelling.
    assert_eq!(g("x = repr(tuple[int, ...])", "x"), "'tuple[int, ...]'");
}

// `builtins` importable as a module (self-contained build). functools/operator/
// enum/re all `import builtins`; the ffi build uses CPython's own module.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn builtins_module_exposes_functions_types_exceptions() {
    assert_eq!(g("import builtins\nx = builtins.abs(-5)", "x"), "5");
    assert_eq!(
        g("from builtins import len as L\nx = L([1, 2, 3])", "x"),
        "3"
    );
    assert_eq!(
        g("import builtins\nx = builtins.int('42') == 42", "x"),
        "True"
    );
    assert_eq!(
        g(
            "import builtins\nx = builtins.ValueError.__name__ == 'ValueError'",
            "x"
        ),
        "True",
    );
}

// The unmodified CPython types.py runs on pythonrs's native introspection floor
// (no _types shim): its whole `type(_f.__code__)`-onward derivation block
// succeeds. Gated to the self-contained build (the ffi build uses CPython's).
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn faithful_types_module_runs_on_native_primitives() {
    assert_eq!(
        g("import types\nx = types.GenericAlias.__name__", "x"),
        "'GenericAlias'"
    );
    // `types.UnionType` is `typing.Union` as of 3.14, so it reports the typing
    // name — NOT the pre-3.14 `UnionType` — and `types.UnionType` must still be
    // the very type `X | Y` builds.
    assert_eq!(
        g("import types\nx = types.UnionType.__name__", "x"),
        "'Union'"
    );
    assert_eq!(
        g("import types\nx = types.UnionType is type(int | str)", "x"),
        "True"
    );
    for ty in [
        "CodeType",
        "CellType",
        "MappingProxyType",
        "SimpleNamespace",
        "TracebackType",
        "FrameType",
        "WrapperDescriptorType",
        "GetSetDescriptorType",
    ] {
        assert_eq!(
            g(&format!("import types\nx = hasattr(types, {ty:?})"), "x"),
            "True",
            "types.{ty} should be derivable",
        );
    }
    // The rest of the module (PEP 3115 helpers) is intact too.
    assert_eq!(
        g("import types\nx = hasattr(types, 'new_class')", "x"),
        "True"
    );
}

#[test]
fn closure_cells_and_freevars() {
    // A closure exposes its free variables as cells (co_freevars + __closure__).
    let src = "def outer():\n    x = 10\n    y = 20\n    def inner():\n        return x + y\n    \
               return inner\nf = outer()";
    assert_eq!(
        g(&format!("{src}\nz = f.__code__.co_freevars"), "z"),
        "('x', 'y')"
    );
    assert_eq!(g(&format!("{src}\nz = len(f.__closure__)"), "z"), "2");
    assert_eq!(
        g(
            &format!("{src}\nz = sorted(c.cell_contents for c in f.__closure__)"),
            "z"
        ),
        "[10, 20]",
    );
    assert_eq!(
        g(&format!("{src}\nz = type(f.__closure__[0]).__name__"), "z"),
        "'cell'"
    );
    // A `nonlocal` declaration alone makes a name free (even unreferenced).
    let cf =
        "def factory():\n    a = 1\n    def f():\n        nonlocal a\n    return f.__closure__[0]";
    assert_eq!(
        g(&format!("{cf}\nz = type(factory()).__name__"), "z"),
        "'cell'"
    );
    // A non-closure function has __closure__ None.
    assert_eq!(g("def g(): return 1\nz = g.__closure__", "z"), "None");
}

#[test]
fn exception_traceback_and_frame() {
    // A caught exception exposes __traceback__ over the captured frames; each node
    // has a tb_frame. (Faithful types.py derives TracebackType/FrameType here.)
    let src = "try:\n    raise TypeError('boom')\nexcept TypeError as exc:\n    \
               tb = exc.__traceback__\n    x = (type(tb).__name__, type(tb.tb_frame).__name__)";
    assert_eq!(g(src, "x"), "('traceback', 'frame')");
    // A never-propagated exception has no traceback.
    assert_eq!(g("x = TypeError('z').__traceback__", "x"), "None");
}

#[test]
fn introspection_descriptor_types() {
    // The C-level descriptor / mappingproxy types the faithful types.py derives.
    assert_eq!(g("x = type(type.__dict__).__name__", "x"), "'mappingproxy'");
    assert_eq!(
        g("x = type(object.__init__).__name__", "x"),
        "'wrapper_descriptor'"
    );
    assert_eq!(
        g("x = type(object().__str__).__name__", "x"),
        "'method-wrapper'"
    );
    assert_eq!(
        g("x = type(dict.__dict__['fromkeys']).__name__", "x"),
        "'classmethod_descriptor'",
    );
    assert_eq!(
        g("def _f(): pass\nx = type(type(_f).__code__).__name__", "x"),
        "'getset_descriptor'",
    );
    assert_eq!(
        g(
            "def _f(): pass\nx = type(type(_f).__globals__).__name__",
            "x"
        ),
        "'member_descriptor'",
    );
    // A mappingproxy indexes through to its wrapped dict.
    assert_eq!(
        g("x = type(dict.__dict__['fromkeys']).__name__", "x"),
        "'classmethod_descriptor'",
    );
}

#[test]
fn simplenamespace_and_sys_implementation() {
    // sys.implementation is a native SimpleNamespace; its type is what the
    // faithful types.py binds as SimpleNamespace.
    assert_eq!(
        g("import sys\nx = type(sys.implementation).__name__", "x"),
        "'SimpleNamespace'",
    );
    assert_eq!(
        g("import sys\nx = sys.implementation.name", "x"),
        "'pythonrs'"
    );
    // Constructible from the type, repr as namespace(...), attributes mutable.
    assert_eq!(
        g(
            "import sys\nSN = type(sys.implementation)\nn = SN(a=1, b=2)\nx = repr(n)",
            "x"
        ),
        "'namespace(a=1, b=2)'",
    );
    assert_eq!(
        g(
            "import sys\nSN = type(sys.implementation)\nn = SN(a=1)\nn.b = 5\nx = n.a + n.b",
            "x"
        ),
        "6",
    );
}

#[test]
fn pep604_union_type() {
    // `X | Y` on types builds a native types.UnionType (used in annotations and
    // isinstance across the faithful stdlib).
    assert_eq!(g("x = int | str", "x"), "int | str");
    // 3.14 merged the PEP 604 type into `typing.Union`: the name is `Union`, the
    // module is `typing`, and messages spell it `'typing.Union' object …`. The
    // pre-3.14 `builtins.UnionType` spelling is gone.
    assert_eq!(
        g(
            "T = type(int | str)\nx = (T.__name__, T.__qualname__, T.__module__)",
            "x"
        ),
        "('Union', 'Union', 'typing')"
    );
    assert_eq!(
        g("x = repr(type(int | str))", "x"),
        "\"<class 'typing.Union'>\""
    );
    assert_eq!(
        eval_str("x = (int | str)()").unwrap_err(),
        "TypeError: 'typing.Union' object is not callable"
    );
    assert_eq!(g("x = int | None", "x"), "int | None");
    assert_eq!(
        g("x = (int | str | float).__args__", "x"),
        "(<class 'int'>, <class 'str'>, <class 'float'>)",
    );
    assert_eq!(g("x = isinstance(5, int | str)", "x"), "True");
    assert_eq!(g("x = isinstance(1.5, int | str)", "x"), "False");
    assert_eq!(g("x = isinstance(None, int | None)", "x"), "True");
    // Duplicate members dedupe; a lone member collapses to the type itself.
    assert_eq!(g("x = int | int is int", "x"), "True");
}

#[test]
fn function_code_object_co_attributes() {
    // Native code object: argcounts, varnames, flags derived from the FuncDef,
    // matching CPython exactly (needed by inspect/functools/dataclasses/types).
    let sig = "def f(a, b, /, c, *args, d, **kw):\n    x = 1\n    return x\n";
    assert_eq!(g(&format!("{sig}y = f.__code__.co_name"), "y"), "'f'");
    assert_eq!(g(&format!("{sig}y = f.__code__.co_argcount"), "y"), "3");
    assert_eq!(
        g(&format!("{sig}y = f.__code__.co_posonlyargcount"), "y"),
        "2"
    );
    assert_eq!(
        g(&format!("{sig}y = f.__code__.co_kwonlyargcount"), "y"),
        "1"
    );
    assert_eq!(
        g(&format!("{sig}y = f.__code__.co_varnames"), "y"),
        "('a', 'b', 'c', 'd', 'args', 'kw', 'x')",
    );
    assert_eq!(
        g("def f(): pass\ny = type(f.__code__).__name__", "y"),
        "'code'"
    );
    // co_flags: OPTIMIZED|NEWLOCALS = 0x3 for a plain function. CO_NOFREE (0x40)
    // is NOT part of it — `dis.COMPILER_FLAG_NAMES` still names the bit, but
    // 3.14's compiler never sets it, so a non-closure reports 3, not 67.
    assert_eq!(g("def f(): pass\ny = f.__code__.co_flags", "y"), "3");
    // …and a closure reports 19, not 3: what actually distinguishes a nested
    // function is CO_NESTED (0x10), which is set for BOTH the closure and the
    // free-variable-less function beside it.
    assert_eq!(
        g(
            "def o():\n    z = 1\n    def cl(): return z\n    def nf(): pass\n\
             \x20   return cl, nf\ncl, nf = o()\ny = (cl.__code__.co_flags, nf.__code__.co_flags)",
            "y"
        ),
        "(19, 19)"
    );
    // CO_METHOD (0x8000000), new in 3.14, marks a function defined directly in a
    // class body — including a lambda, and including one nested in a function
    // (which is then NESTED|METHOD).
    assert_eq!(
        g(
            "class C:\n    def m(self): pass\n    lam = lambda self: 0\n\
             y = (C.m.__code__.co_flags, C.lam.__code__.co_flags)",
            "y"
        ),
        "(134217731, 134217731)"
    );
    assert_eq!(
        g(
            "def o():\n    class D:\n        def m(self): pass\n    return D\n\
             y = o().m.__code__.co_flags",
            "y"
        ),
        "134217747"
    );
    // CO_HAS_DOCSTRING (0x4000000) is set when the body opens with a string.
    assert_eq!(
        g("def d():\n    'doc'\ny = d.__code__.co_flags", "y"),
        "67108867"
    );
    assert_eq!(
        g(
            "def g():\n    yield 1\ny = g.__code__.co_flags & 0x20 != 0",
            "y"
        ),
        "True"
    );
    assert_eq!(
        g(
            "async def c(): pass\ny = c.__code__.co_flags & 0x80 != 0",
            "y"
        ),
        "True"
    );
}

#[test]
fn function_docstring_is_dunder_doc() {
    // The body's first bare string literal is `__doc__`; absent one, `__doc__` is
    // None (present as an attribute, never an AttributeError).
    assert_eq!(
        g("def f():\n    'the doc'\n    return 1\nx = f.__doc__", "x"),
        "'the doc'"
    );
    assert_eq!(g("def g():\n    return 2\nx = g.__doc__", "x"), "None");
    // A non-string first statement is not a docstring.
    assert_eq!(g("def h():\n    42\nx = h.__doc__", "x"), "None");
}

#[test]
fn delattr_on_class_and_namespace() {
    // delattr removes a class attribute (only instances worked before) and a
    // SimpleNamespace attribute.
    assert_eq!(
        g(
            "class C: pass\nC.x = 5\ndelattr(C, 'x')\nx = hasattr(C, 'x')",
            "x"
        ),
        "False"
    );
    assert_eq!(
        g(
            "import sys\nSN = type(sys.implementation)\nn = SN(a=1)\ndel n.a\nx = hasattr(n, 'a')",
            "x"
        ),
        "False",
    );
}

#[test]
fn metaclass_super_new_is_static() {
    // super().__new__ inside a metaclass __new__ passes the class explicitly (no
    // extra bound receiver -> right arg count), and zero-arg super() there resolves
    // against that first argument. This is what let _collections_abc's ABCMeta run.
    let src = "class Meta(type):\n    def __new__(mcls, name, bases, ns):\n        \
               cls = super().__new__(mcls, name, bases, ns)\n        cls.tag = 'made'\n        \
               return cls\nclass C(metaclass=Meta): pass\nx = C.tag";
    assert_eq!(g(src, "x"), "'made'");
}

#[test]
fn isinstance_of_type_for_type_objects() {
    // Every type object is an instance of `type` -- incl. the coroutine/generator/
    // iterator types the stdlib registers with ABCs; functions and unbound
    // methods are not.
    assert_eq!(
        g("def _c(): pass\nx = isinstance(type(_c), type)", "x"),
        "True"
    );
    assert_eq!(g("x = isinstance(type(iter([])), type)", "x"), "True");
    assert_eq!(g("x = isinstance(int, type)", "x"), "True");
    assert_eq!(g("class C: pass\nx = isinstance(C, type)", "x"), "True");
    assert_eq!(g("x = isinstance(len, type)", "x"), "False");
    assert_eq!(g("x = isinstance(str.upper, type)", "x"), "False");
    assert_eq!(g("x = isinstance(5, type)", "x"), "False");
}

#[test]
fn function_attributes() {
    // Functions carry a writable attribute dict (abc's __isabstractmethod__,
    // functools.wraps, decorators).
    assert_eq!(
        g(
            "def f(): pass\nf.__isabstractmethod__ = True\nx = f.__isabstractmethod__",
            "x"
        ),
        "True",
    );
    // …but a function that was never marked has no such attribute at all: the
    // slot belongs to `staticmethod`/`classmethod`/`property`, not to
    // `function`. `abc` reads it with a `getattr(…, False)` default precisely
    // because of this, so answering False here would hide the real shape.
    assert_eq!(
        eval_str("def g(): pass\nx = g.__isabstractmethod__").unwrap_err(),
        "AttributeError: 'function' object has no attribute '__isabstractmethod__'"
    );
    assert_eq!(
        g(
            "def g(): pass\nx = staticmethod(g).__isabstractmethod__",
            "x"
        ),
        "False"
    );
    assert_eq!(
        g(
            "def g(): pass\nx = classmethod(g).__isabstractmethod__",
            "x"
        ),
        "False"
    );
    assert_eq!(
        g("def g(): pass\nx = property(g).__isabstractmethod__", "x"),
        "False"
    );
    assert_eq!(g("def f(): pass\nf.tag = 42\nx = f.tag", "x"), "42");
    assert_eq!(
        g("def f(): pass\nf.a = 1\nf.b = 2\nx = f.__dict__", "x"),
        "{'a': 1, 'b': 2}"
    );
}

#[test]
fn string_module_and_string_formatter() {
    // Native _string (formatter_parser/formatter_field_name_split) lets the
    // string package + string.Formatter run.
    assert_eq!(g("import string\nx = string.digits", "x"), "'0123456789'");
    assert_eq!(
        g(
            "import string\nx = string.Formatter().format('{0} {name}', 'hi', name='world')",
            "x"
        ),
        "'hi world'",
    );
    assert_eq!(
        g(
            "import _string\nx = list(_string.formatter_parser('a{0}b'))",
            "x"
        ),
        "[('a', '0', '', None), ('b', None, None, None)]",
    );
    assert_eq!(
        g(
            "import _string\nx = _string.formatter_field_name_split('0.name[1]')[0]",
            "x"
        ),
        "0",
    );
}

#[test]
fn nested_fstrings_pep701() {
    // PEP 701: an f-string may nest same-quote f-strings inside its fields.
    assert_eq!(
        g("d = 'dec'\nx = f'{f' {d}' if d else ''} tail'", "x"),
        "' dec tail'"
    );
    assert_eq!(g("x = f'{f'{f'{1 + 1}'}'}'", "x"), "'2'");
    assert_eq!(g("w = 5\nx = f'{f'{w}'.rjust(3)}|'", "x"), "'  5|'");
    // Regular f-strings (conversions, format specs) are unaffected.
    assert_eq!(
        g("n = 'x'\nx = f'{n} = {1 + 2:03d} {n!r}'", "x"),
        "\"x = 003 'x'\""
    );
}

#[test]
fn object_dunder_methods() {
    // Universal object dunders are reachable as bound methods (the stdlib uses
    // e.g. cache.__len__ directly).
    assert_eq!(g("x = {'a': 1}.__len__()", "x"), "1");
    assert_eq!(g("x = [1, 2, 3].__getitem__(1)", "x"), "2");
    assert_eq!(g("x = 'abcd'.__len__()", "x"), "4");
    assert_eq!(g("x = (1, 2, 3).__contains__(2)", "x"), "True");
    // functools.lru_cache uses cache.__len__ internally.
    assert_eq!(
        g(
            "import functools\n@functools.cache\ndef f(n): return n * n\nx = f(6) + f(6)",
            "x"
        ),
        "72",
    );
}

// The os module imports and works on the self-contained build (native posix +
// the circular-import / sys.modules / __new__ fixes it needs). Gated to no-ffi.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn os_module_self_contained() {
    assert_eq!(g("import os\nx = type(os.getcwd()).__name__", "x"), "'str'");
    assert_eq!(g("import os\nx = os.sep", "x"), "'/'");
    assert_eq!(
        g("import os\nx = os.path.join('a', 'b', 'c')", "x"),
        "'a/b/c'"
    );
    assert_eq!(
        g("import os\nx = os.path.basename('/x/y.txt')", "x"),
        "'y.txt'"
    );
    assert_eq!(g("import os\nx = os.getpid() > 0", "x"), "True");
    assert_eq!(g("import os\nx = 'PATH' in os.environ", "x"), "True");
    // `st_size >= 0` is true for any non-negative number, including a stub that
    // always answers 0 — it could not distinguish a real stat from no stat at
    // all. Size a file this test WRITES, so the exact byte count is known.
    assert_eq!(
        g(
            "import os\n\
             p = os.path.join(os.environ.get('TMPDIR', '/tmp'), 'pyrs-stat-%d.bin' % os.getpid())\n\
             f = open(p, 'wb')\n\
             f.write(b'0123456789')\n\
             f.close()\n\
             st = os.stat(p)\n\
             x = (st.st_size, st[6], os.path.getsize(p), os.stat('.').st_size > 0)\n\
             os.remove(p)",
            "x"
        ),
        "(10, 10, 10, True)"
    );
    assert_eq!(g("import contextlib\nx = 1", "x"), "1");
}

// The faithful CPython `enum` stdlib runs self-contained on pythonrs's own VM:
// the metaclass machinery (`__prepare__`, `__set_name__`/`_proto_member`,
// `__init_subclass__`, metaclass `__iter__`, `_simple_enum`) all resolve
// natively. Output is bit-faithful to CPython 3.14.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn enum_module_self_contained() {
    // Plain Enum: repr, member iteration, value lookup.
    let e = "import enum\nclass C(enum.Enum):\n    RED = 1\n    GREEN = 2\n";
    assert_eq!(g(&format!("{e}x = repr(C.RED)"), "x"), "'<C.RED: 1>'");
    assert_eq!(
        g(&format!("{e}x = [m.name for m in C]"), "x"),
        "['RED', 'GREEN']"
    );
    assert_eq!(g(&format!("{e}x = C(2).name"), "x"), "'GREEN'");
    // IntEnum: `str` is the int value (3.11+ ReprEnum behavior), arithmetic works.
    let n = "import enum\nclass N(enum.IntEnum):\n    ONE = 1\n    TWO = 2\n";
    assert_eq!(g(&format!("{n}x = str(N.ONE)"), "x"), "'1'");
    assert_eq!(g(&format!("{n}x = N.TWO + 1"), "x"), "3");
    // IntFlag: bitwise combine, composite repr, membership.
    let p = "import enum\nclass P(enum.IntFlag):\n    R = 4\n    W = 2\n    X = 1\n";
    assert_eq!(g(&format!("{p}x = (P.R | P.W).value"), "x"), "6");
    assert_eq!(g(&format!("{p}x = repr(P.R | P.W)"), "x"), "'<P.R|W: 6>'");
    assert_eq!(g(&format!("{p}x = P.R in (P.R | P.W)"), "x"), "True");
    // StrEnum: members are strings.
    let s = "import enum\nclass S(enum.StrEnum):\n    A = 'aa'\n";
    assert_eq!(g(&format!("{s}x = S.A == 'aa'"), "x"), "True");
}

// A function is a non-data descriptor: `f.__get__(obj, cls)` binds it as a
// method (`hasattr(f, '__get__')` is True, `__set__`/`__delete__` are not) — enum
// relies on this to tell members from methods.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn function_is_descriptor() {
    assert_eq!(
        g("def f(self): return 1\nx = hasattr(f, '__get__')", "x"),
        "True"
    );
    assert_eq!(
        g("def f(self): return 1\nx = hasattr(f, '__set__')", "x"),
        "False"
    );
    let bind = "def f(self): return self + 1\nclass C: pass\nc = C()\nx = f.__get__(5, int)()";
    assert_eq!(g(bind, "x"), "6");
}

// `obj.__class__(args)` — a data attribute invoked as a call — constructs an
// instance of the object's class (compiled as a method call; `__class__`
// resolves to the class, then constructs). Used by enum's `Flag.__or__`.
#[test]
fn class_attr_call_constructs() {
    let src = "class I(int):\n    def dup(self):\n        return self.__class__(int(self) * 2)\nx = int(I(4).dup())";
    assert_eq!(g(src, "x"), "8");
}

// Native `math` combinatorics/accumulation functions, bignum-exact and matching
// CPython 3.14. Common in third-party packages (more-itertools, packaging).
#[test]
fn math_comb_perm_fsum_prod() {
    assert_eq!(g("import math\nx = math.comb(5, 2)", "x"), "10");
    assert_eq!(g("import math\nx = math.comb(3, 5)", "x"), "0"); // k > n
    assert_eq!(
        g("import math\nx = math.comb(50, 25)", "x"),
        "126410606437752"
    );
    assert_eq!(g("import math\nx = math.perm(5, 2)", "x"), "20");
    assert_eq!(g("import math\nx = math.perm(4)", "x"), "24"); // perm(n) == n!
    assert_eq!(g("import math\nx = math.prod([1, 2, 3, 4])", "x"), "24");
    assert_eq!(g("import math\nx = math.prod([])", "x"), "1");
    assert_eq!(
        g("import math\nx = round(math.fsum([0.1] * 10), 10)", "x"),
        "1.0"
    );
}

// `atexit` registers cleanup callbacks that run LIFO at shutdown. `register`
// returns the callback (usable as a decorator); `_run_exitfuncs` runs and drains
// them; `unregister` removes by identity. (At-shutdown firing is exercised by the
// `-c`/file entry point, not the `eval_str` test path.)
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn atexit_callbacks() {
    let src = "\
import atexit
log = []
atexit.register(lambda: log.append('a'))
b = atexit.register(lambda: log.append('b'))
atexit.unregister(b)
atexit.register(lambda: log.append('c'))
n = atexit._ncallbacks()
atexit._run_exitfuncs()
x = (n, log, atexit._ncallbacks())";
    // 2 registered (b removed), run LIFO (c before a), drained to 0.
    assert_eq!(g(src, "x"), "(2, ['c', 'a'], 0)");
}

// A user class exposes `__module__` (its defining module) and, with no
// annotations, `__annotations__ == {}` (not an AttributeError) — CPython
// semantics that typing.py's `_get_protocol_attrs` depends on. `Generic[T]`
// builds a generic alias.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn class_dunders_and_generic_subscript() {
    assert_eq!(g("class C: pass\nx = C.__annotations__", "x"), "{}");
    assert_eq!(g("class C: pass\nx = C.__module__", "x"), "'__main__'");
    assert_eq!(
        g(
            "import collections\nx = collections.Counter.__module__",
            "x"
        ),
        "'collections'"
    );
    let gen =
        "from _typing import Generic, TypeVar\nT = TypeVar('T')\nx = type(Generic[T]).__name__";
    assert_eq!(g(gen, "x"), "'GenericAlias'");
}

// The FULL vendored `collections` runs (via a native `_collections` arm exposing
// the container accelerators as type objects), so ChainMap/Counter/OrderedDict/
// namedtuple/UserList/UserDict all come from the faithful pure-Python source and
// match CPython 3.14.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn collections_full_vendored() {
    assert_eq!(
        g(
            "import collections\nx = collections.Counter('aabbbc').most_common(2)",
            "x"
        ),
        "[('b', 3), ('a', 2)]"
    );
    assert_eq!(
        g(
            "import collections\nx = dict(collections.ChainMap({'a': 1}, {'a': 2, 'b': 3}))",
            "x"
        ),
        "{'a': 1, 'b': 3}",
    );
    let nt =
        "import collections\nP = collections.namedtuple('P', ['x', 'y'])\nx = P(1, 2)._asdict()";
    assert_eq!(g(nt, "x"), "{'x': 1, 'y': 2}");
    assert_eq!(
        g(
            "import collections\nx = list(collections.UserList([1, 2]) + [3])",
            "x"
        ),
        "[1, 2, 3]"
    );
}

// `eval`/`exec` with an explicit globals namespace runs in a scope where a
// function/lambda it defines captures that namespace (namedtuple's eval'd
// `__new__` reads `_tuple_new` from the namespace when later called).
#[test]
fn eval_globals_captured_by_defined_fn() {
    let src = "ns = {'_mul': lambda a: a * 3}\nf = eval('lambda n: _mul(n)', ns)\nx = f(7)";
    assert_eq!(g(src, "x"), "21");
}

// A builtin-subclass instance delegates to its native payload when a base method
// is BOUND to a name (`g = d.get`) and when the instance is passed to
// `zip`/`map`/`dict` (payload iteration / the keys() mapping protocol).
#[test]
fn builtin_subclass_delegation() {
    // Bound base method resolves through the payload.
    let bind = "class D(dict):\n    pass\nd = D(); d['a'] = 1\ng = d.get\nx = (g('a'), g('z', 9))";
    assert_eq!(g(bind, "x"), "(1, 9)");
    // A tuple subclass passed to zip iterates its payload.
    let z = "class T(tuple):\n    pass\nt = tuple.__new__(T, (1, 2))\nx = dict(zip(['a', 'b'], t))";
    assert_eq!(g(z, "x"), "{'a': 1, 'b': 2}");
    // dict() over a user mapping uses keys()+__getitem__.
    let m = "class M:\n    def keys(self):\n        return ['x']\n    def __getitem__(self, k):\n        return k.upper()\nx = dict(M())";
    assert_eq!(g(m, "x"), "{'x': 'X'}");
}

// `import a.b` binds the TOP package `a` (not the leaf), and importing a dotted
// name pulls its parent packages first — so `import os.path` gives the `os`
// module with `os.path` reachable, and `import collections.abc` works via the
// alias its parent registers.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn dotted_import_binds_top_package() {
    assert_eq!(
        g("import os.path\nx = os.path.join('a', 'b')", "x"),
        "'a/b'"
    );
    assert_eq!(g("import os.path\nx = type(os).__name__", "x"), "'module'");
    assert_eq!(
        g(
            "import collections.abc\nx = collections.abc.Mapping.__name__",
            "x"
        ),
        "'Mapping'",
    );
}

// `super().__setattr__(name, value)` (and `__delattr__`) bottom out at object's
// implementations — the plain instance-dict ops. typing's `_GenericAlias`
// relies on this.
#[test]
fn super_setattr_reaches_object() {
    let src = "\
class Guard:
    def __setattr__(self, k, v):
        super().__setattr__(k, v.upper() if isinstance(v, str) else v)
g = Guard()
g.name = 'abc'
g.n = 5
x = (g.name, g.n)";
    assert_eq!(g(src, "x"), "('ABC', 5)");
}

// A builtin type object reports `builtins` as its `__module__` and its name as
// `__qualname__` (typing's deprecated-alias machinery reads both).
#[test]
fn builtin_type_module_qualname() {
    assert_eq!(g("x = list.__module__", "x"), "'builtins'");
    assert_eq!(g("x = dict.__qualname__", "x"), "'dict'");
}

// A `TypeVar`/`ParamSpec`/`TypeVarTuple` from the native `_typing` core exposes
// the dunder attributes typing.py reads, and is hashable (usable in sets/dicts).
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn typing_type_var_core() {
    let src = "import _typing\nT = _typing.TypeVar('T', bound=int)\n\
               x = (T.__name__, T.__bound__.__name__, T.__covariant__, type(T).__name__)";
    assert_eq!(g(src, "x"), "('T', 'int', False, 'TypeVar')");
    assert_eq!(
        g(
            "import _typing\nT = _typing.TypeVar('T')\nx = len({T, T, _typing.TypeVar('T')})",
            "x"
        ),
        "2",
    );
}

// Forward references in annotations no longer abort a definition: a function or
// class-body annotation that names something not yet bound is compiled as a
// thunk whose forward-reference NameError is caught (the entry is dropped),
// while a resolvable annotation still records the real object. Common in
// third-party packages (tomli, dataclass-heavy code).
#[test]
fn forward_reference_annotations_tolerated() {
    // Resolvable function annotations stay real objects.
    assert_eq!(
        g(
            "def f(a: int) -> str:\n    return ''\nx = f.__annotations__",
            "x"
        ),
        "{'a': <class 'int'>, 'return': <class 'str'>}",
    );
    // A forward-ref function annotation leaves annotations empty, not a crash.
    assert_eq!(
        g(
            "def g(x) -> NotYet:\n    return x\ny = g.__annotations__\nx = y",
            "x"
        ),
        "{}"
    );
    // Class body: resolvable kept, forward-ref dropped.
    let cls = "class C:\n    a: int\n    b: Later\n    c: str = 'z'\nx = sorted(C.__annotations__)";
    assert_eq!(g(cls, "x"), "['a', 'c']");
}

// `types.MappingProxyType(d)` (i.e. `type(type.__dict__)`, the `mappingproxy`
// type) wraps a dict in a read-only view.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn mappingproxy_constructor() {
    let src = "from types import MappingProxyType\nm = MappingProxyType({'a': 1, 'b': 2})\nx = (m['a'], m.get('b'), sorted(m.keys()))";
    assert_eq!(g(src, "x"), "(1, 2, ['a', 'b'])");
}

// Native `re` (regex-crate backed) matches CPython for the common surface.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn re_module_core() {
    assert_eq!(
        g(
            "import re\nx = re.match(r'(\\d+)-(\\d+)', '12-34').groups()",
            "x"
        ),
        "('12', '34')"
    );
    assert_eq!(
        g("import re\nx = re.findall(r'\\d+', 'a1b22c333')", "x"),
        "['1', '22', '333']"
    );
    assert_eq!(
        g("import re\nx = re.sub(r'\\d', '#', 'a1b2')", "x"),
        "'a#b#'"
    );
    assert_eq!(
        g("import re\nx = re.sub(r'(\\w)(\\d)', r'\\2\\1', 'a1')", "x"),
        "'1a'"
    );
    assert_eq!(
        g(
            "import re\nm = re.search(r'(?P<y>\\d+)', 'x=42')\nx = m.group('y')",
            "x"
        ),
        "'42'"
    );
    assert_eq!(
        g("import re\nx = re.split(r'\\s+', 'a b  c')", "x"),
        "['a', 'b', 'c']"
    );
}

// PEP 695 type parameters (`class C[T]`, `def f[T]`) parse and run: the params
// bind to `object` so eagerly-evaluated annotations (`-> T`) resolve, and the
// runtime is unaffected. CPython 3.14's typing.py uses this syntax throughout.
#[test]
fn pep695_type_params() {
    assert_eq!(
        g("def ident[T](x: T) -> T:\n    return x\nx = ident(5)", "x"),
        "5"
    );
    let m = "class Stack[T]:\n    def push[U](self, v: U) -> U:\n        return v\nx = Stack().push(99)";
    assert_eq!(g(m, "x"), "99");
    // Multiple params, bound/default syntax parse and are discarded.
    assert_eq!(
        g("class Pair[A, B]:\n    pass\nx = Pair.__name__", "x"),
        "'Pair'"
    );
    assert_eq!(
        g("def f[T: int, U = str](x):\n    return x\nx = f(7)", "x"),
        "7"
    );
}

// A relative import (`from . import _compiler` in re's `__init__`) resolves
// against the module's `__package__`: it reaches the real submodule `re._compiler`
// (which then needs the native `_sre` C-accelerator). Before relative-import
// support the leading dot was dropped and the name collapsed to `''`.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn relative_import_resolves_package() {
    // `email/__init__.py` uses relative imports (`from . import ...`); importing
    // it exercises relative-import resolution against `__package__`. The program
    // running to completion (`g` would panic on an import error) is the check.
    assert_eq!(g("import email\nx = 'ok'", "x"), "'ok'");
}

// `collections.abc` is the pure-Python `_collections_abc` module (CPython aliases
// it via `sys.modules['collections.abc'] = _collections_abc`); pythonrs serves
// `collections` from a native arm, so the alias is wired in the importer.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn collections_abc_alias() {
    let src = "from collections.abc import Mapping, Sequence, Iterable\n\
               x = (issubclass(dict, Mapping), issubclass(list, Sequence), isinstance((), Iterable))";
    assert_eq!(g(src, "x"), "(True, True, True)");
}

// A module whose body fails mid-import is NOT left cached as a broken shell: a
// retry re-runs the body and re-raises (CPython removes it from sys.modules),
// rather than silently resolving to a half-built module that masks the failure.
// Uses `sqlite3`, whose `_sqlite3` accelerator this runtime does not provide.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn failed_import_is_not_cached() {
    let src = "\
res = []
for _ in range(2):
    try:
        import sqlite3
        res.append('cached')
    except ModuleNotFoundError:
        res.append('raised')
x = res";
    assert_eq!(g(src, "x"), "['raised', 'raised']");
}

// A bound method called through a stored reference (`f = obj.m; f()`) — not just
// `obj.m()` — resolves zero-arg super() (owner comes from FuncVal, tagged at
// class registration).
#[test]
fn stored_bound_method_resolves_super() {
    let src =
        "class B:\n    def g(self):\n        return 'b'\nclass C(B):\n    def g(self):\n        \
               return super().g() + 'c'\nf = C().g\nx = f()";
    assert_eq!(g(src, "x"), "'bc'");
}

// Native MT19937 random -> bit-identical to CPython (same seeding). no-ffi only.
#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn random_matches_cpython() {
    assert_eq!(
        g(
            "import random\nrandom.seed(42)\nx = [random.randint(1, 100) for _ in range(5)]",
            "x"
        ),
        "[82, 15, 4, 95, 36]",
    );
    assert_eq!(
        g(
            "import random\nrandom.seed(42)\nx = random.getrandbits(64)",
            "x"
        ),
        "2053695854357871005"
    );
    assert_eq!(
        g(
            "import random\nrandom.seed(42)\nx = random.sample(range(100), 3)",
            "x"
        ),
        "[81, 14, 3]",
    );
}

#[test]
fn thread_locks() {
    // Native _thread locks: RLock is reentrant, plain lock tracks state. (Single
    // user thread, so acquire always succeeds.)
    assert_eq!(
        g(
            "import _thread\nlk = _thread.RLock()\nwith lk:\n    x = 42",
            "x"
        ),
        "42"
    );
    assert_eq!(
        g(
            "import _thread\nl = _thread.allocate_lock()\nl.acquire()\nx = l.locked()",
            "x"
        ),
        "True",
    );
    assert_eq!(g("import _thread\nx = _thread.get_ident()", "x"), "1");
    // functools imports on top of _thread.
    assert_eq!(
        g(
            "import functools\nx = functools.reduce(lambda a, b: a + b, [1, 2, 3, 4])",
            "x"
        ),
        "10"
    );
}

#[test]
fn itertools_module() {
    // Lazy iterators (incl. over infinite sources via islice) and combinatorics.
    assert_eq!(
        g(
            "import itertools as it\nx = list(it.islice(it.count(10, 2), 4))",
            "x"
        ),
        "[10, 12, 14, 16]"
    );
    assert_eq!(
        g(
            "import itertools as it\nx = list(it.islice(it.cycle('AB'), 5))",
            "x"
        ),
        "['A', 'B', 'A', 'B', 'A']"
    );
    assert_eq!(
        g(
            "import itertools as it\nx = list(it.accumulate([1, 2, 3, 4]))",
            "x"
        ),
        "[1, 3, 6, 10]"
    );
    assert_eq!(
        g(
            "import itertools as it\nx = list(it.chain([1, 2], [3]))",
            "x"
        ),
        "[1, 2, 3]"
    );
    assert_eq!(
        g(
            "import itertools as it\nx = list(it.pairwise([1, 2, 3]))",
            "x"
        ),
        "[(1, 2), (2, 3)]"
    );
    assert_eq!(
        g(
            "import itertools as it\nx = list(it.combinations([1, 2, 3], 2))",
            "x"
        ),
        "[(1, 2), (1, 3), (2, 3)]",
    );
    assert_eq!(
        g(
            "import itertools as it\nx = list(it.product([1, 2], [3, 4]))",
            "x"
        ),
        "[(1, 3), (1, 4), (2, 3), (2, 4)]",
    );
    assert_eq!(
        g(
            "import itertools as it\nx = [(k, list(gp)) for k, gp in it.groupby([1, 1, 2, 3, 3])]",
            "x"
        ),
        "[(1, [1, 1]), (2, [2]), (3, [3, 3])]",
    );
}

#[test]
fn errno_module() {
    // Native errno constants (from libc). Low POSIX numbers are stable across
    // Linux/macOS, so assert those.
    assert_eq!(
        g(
            "import errno\nx = (errno.ENOENT, errno.EEXIST, errno.EINVAL)",
            "x"
        ),
        "(2, 17, 22)"
    );
    assert_eq!(
        g("import errno\nx = errno.errorcode[errno.ENOENT]", "x"),
        "'ENOENT'"
    );
}

#[test]
fn bignum_range() {
    // A range whose bounds exceed i64 works fully (list/index/contains/len/bool/
    // repr/iter), matching CPython.
    assert_eq!(
        g("x = list(range(10**20, 10**20 + 4))", "x"),
        "[100000000000000000000, 100000000000000000001, \
          100000000000000000002, 100000000000000000003]",
    );
    assert_eq!(
        g("x = range(10**20, 10**20 + 5)[2]", "x"),
        "100000000000000000002"
    );
    assert_eq!(
        g("x = range(10**20, 10**20 + 5)[-1]", "x"),
        "100000000000000000004"
    );
    assert_eq!(
        g("x = 10**20 + 3 in range(10**20, 10**20 + 5)", "x"),
        "True"
    );
    assert_eq!(g("x = len(range(10**20, 10**20 + 7))", "x"), "7");
    assert_eq!(g("x = bool(range(5, 5))", "x"), "False");
    assert_eq!(
        g("x = range(10**30)", "x"),
        "range(0, 1000000000000000000000000000000)"
    );
    // The type-extraction case from _collections_abc: range(1<<1000) is iterable
    // — and CPython gives a bignum range its OWN iterator type, distinct from
    // the i64 one, so the name is what tells the two cursors apart.
    assert_eq!(
        g("x = type(iter(range(1 << 1000))).__name__", "x"),
        "'longrange_iterator'"
    );
    assert_eq!(
        g("x = type(iter(range(5))).__name__", "x"),
        "'range_iterator'"
    );
}

/// Every builtin container has its own iterator type in CPython, and the name is
/// observable through `type(it).__name__`. pythonrs walks most of them with one
/// snapshot cursor, so without a tag they would all answer `iterator` — the name
/// CPython reserves for the `__getitem__` sequence iterator alone. Each expected
/// value is CPython 3.14.6's for the same expression.
#[test]
fn builtin_iterator_type_names() {
    let names = "d = {1: 2}\nx = [type(it).__name__ for it in (\n\
        \x20 iter([1]), iter((1,)), iter('a'), iter('é'), iter(b'a'),\n\
        \x20 iter(bytearray(b'a')), iter({1}), iter(frozenset({1})),\n\
        \x20 iter(d), iter(d.keys()), iter(d.values()), iter(d.items()),\n\
        \x20 iter(memoryview(b'a')),\n\
        )]";
    assert_eq!(
        g(names, "x"),
        "['list_iterator', 'tuple_iterator', 'str_ascii_iterator', 'str_iterator', \
         'bytes_iterator', 'bytearray_iterator', 'set_iterator', 'set_iterator', \
         'dict_keyiterator', 'dict_keyiterator', 'dict_valueiterator', \
         'dict_itemiterator', 'memory_iterator']"
    );
    // `reversed` splits the same way: `list` and the three dict views have their
    // own reverse types, `range` reuses its forward one, everything else is the
    // generic `reversed` object.
    let rev = "d = {1: 2}\nx = [type(it).__name__ for it in (\n\
        \x20 reversed([1]), reversed((1,)), reversed('ab'), reversed(range(3)),\n\
        \x20 reversed(d), reversed(d.keys()), reversed(d.values()), reversed(d.items()),\n\
        )]";
    assert_eq!(
        g(rev, "x"),
        "['list_reverseiterator', 'reversed', 'reversed', 'range_iterator', \
         'dict_reversekeyiterator', 'dict_reversekeyiterator', \
         'dict_reversevalueiterator', 'dict_reverseitemiterator']"
    );
    // `iterator` is not a catch-all: it is what the old-style `__getitem__`
    // sequence protocol produces, and nothing else reaches it.
    assert_eq!(
        g(
            "class S:\n    def __getitem__(self, i): raise IndexError\n\
             x = type(iter(S())).__name__",
            "x"
        ),
        "'iterator'"
    );
    // Iterating an iterator hands back the SAME object, so the name survives.
    assert_eq!(
        g(
            "it = iter([1])\nx = (iter(it) is it, type(iter(it)).__name__)",
            "x"
        ),
        "(True, 'list_iterator')"
    );
}

/// `iter()`/`next()` on a user-defined iterator. CPython hands back whatever
/// `__iter__` returned — unchanged and unconsumed — so an object whose
/// `__iter__` returns `self` keeps its identity and stays lazy. Draining it into
/// a snapshot instead would hang on any unbounded iterator.
#[test]
fn user_iterator_protocol_is_lazy_and_identity_preserving() {
    let count = "class Count:\n\
        \x20   def __init__(self): self.i = 0\n\
        \x20   def __iter__(self): return self\n\
        \x20   def __next__(self):\n\
        \x20       self.i += 1\n\
        \x20       return self.i\n\
        c = Count()\nit = iter(c)\n";
    assert_eq!(
        g(&format!("{count}x = (it is c, type(it).__name__)"), "x"),
        "(True, 'Count')"
    );
    // Unbounded: three steps must return, which they cannot if `iter` drained it.
    assert_eq!(
        g(&format!("{count}x = [next(it), next(it), next(it)]"), "x"),
        "[1, 2, 3]"
    );
    // `__iter__` returning a generator stays a generator.
    assert_eq!(
        g(
            "class G:\n\
             \x20   def __iter__(self):\n\
             \x20       yield 1\n\
             \x20       yield 2\n\
             it = iter(G())\nx = (type(it).__name__, next(it))",
            "x"
        ),
        "('generator', 1)"
    );
    // A `__next__` that raises `StopIteration` exhausts rather than propagating.
    assert_eq!(
        g(
            "class Two:\n\
             \x20   def __init__(self): self.n = 0\n\
             \x20   def __iter__(self): return self\n\
             \x20   def __next__(self):\n\
             \x20       self.n += 1\n\
             \x20       if self.n > 2: raise StopIteration\n\
             \x20       return self.n\n\
             x = list(Two())",
            "x"
        ),
        "[1, 2]"
    );
    // `__iter__` must return an iterator, and `next()` needs one — CPython's two
    // distinct messages.
    assert_eq!(
        eval_str("class B:\n    def __iter__(self): return 5\nx = iter(B())").unwrap_err(),
        "TypeError: iter() returned non-iterator of type 'int'"
    );
    assert_eq!(
        eval_str("class N: pass\nx = next(N())").unwrap_err(),
        "TypeError: 'N' object is not an iterator"
    );
    // `__next__` alone does not make an object iterable.
    assert_eq!(
        eval_str("class J:\n    def __next__(self): return 1\nx = iter(J())").unwrap_err(),
        "TypeError: 'J' object is not iterable"
    );
}

#[test]
fn arithmetic_and_precedence() {
    assert_eq!(g("x = 2 + 3 * 4 - 1", "x"), "13");
    assert_eq!(g("x = 7 // 2", "x"), "3");
    assert_eq!(g("x = 7 / 2", "x"), "3.5");
    assert_eq!(g("x = 2 ** 10", "x"), "1024");
    assert_eq!(g("x = 17 % 5", "x"), "2");
    assert_eq!(g("x = -3 + 2 * 4", "x"), "5");
}

// Gaps found by differential probing against CPython 3.14 — each of these
// produced a wrong answer or an AttributeError before the fix.
#[test]
fn parity_gaps_found_by_differential_probing() {
    // `abs(-2**63)`: `i64::MIN.abs()` overflows, and the wrapped result is the
    // NEGATIVE input — a silent wrong answer. Python's ints are unbounded.
    assert_eq!(g("x = abs(-2**63)", "x"), "9223372036854775808");
    assert_eq!(g("x = (-2**63).__abs__()", "x"), "9223372036854775808");
    assert_eq!(g("x = abs(-2**62)", "x"), "4611686018427387904");

    // Every class has `__doc__` — the body's docstring, or None. Missing it broke
    // `contextlib.contextmanager`, which reads it off its own helper class.
    assert_eq!(g("class K: pass\nx = K.__doc__", "x"), "None");
    assert_eq!(g("class K:\n    'd'\nx = K.__doc__", "x"), "'d'");
    assert_eq!(
        g(
            "import contextlib\n@contextlib.contextmanager\ndef cm():\n    yield 42\nwith cm() as v:\n    x = v",
            "x"
        ),
        "42"
    );

    // `f.attr(...)` on an attribute stored on a FUNCTION object is
    // getattr-then-call; it used to reach the type-method table instead and
    // raise, while `(f.attr)(...)` worked.
    assert_eq!(
        g(
            "import functools\ndef d(fn):\n    @functools.wraps(fn)\n    def w(*a): return fn(*a)\n    return w\n@d\ndef g0(n): return n * 10\nx = g0.__wrapped__(4)",
            "x"
        ),
        "40"
    );

    // `math.isclose` (PEP 485) and `math.remainder` were absent.
    assert_eq!(
        g("import math\nx = math.isclose(0.1 + 0.2, 0.3)", "x"),
        "True"
    );
    assert_eq!(g("import math\nx = math.isclose(1.0, 1.1)", "x"), "False");
    assert_eq!(
        g("import math\nx = math.isclose(1.0, 1.1, rel_tol=0.2)", "x"),
        "True"
    );
    assert_eq!(
        g(
            "import math\nx = math.isclose(float('nan'), float('nan'))",
            "x"
        ),
        "False"
    );
    assert_eq!(g("import math\nx = math.remainder(7, 3)", "x"), "1.0");
}

// A nested replacement field that ENDS a format spec — `f"{x:{w}}"` — closes two
// braces in a row. The lexer read that `}}` as an escaped brace at any depth, so
// the outer field never closed and the scan ran off the end of the string.
#[test]
fn fstring_nested_field_at_end_of_spec() {
    assert_eq!(g("w = 4\nx = f'{1:{w}}'", "x"), "'   1'");
    assert_eq!(g("w = 4\nx = f'{-1:>{w}}'", "x"), "'  -1'");
    assert_eq!(g("w = 4\nx = f'{1:<{w}}'", "x"), "'1   '");
    assert_eq!(g("w = 4\nx = f'{1:^{w}}'", "x"), "' 1  '");
    // Still-nested but not final, and the escape forms that must not change.
    assert_eq!(g("w = 4\nx = f'{1:0{w}d}'", "x"), "'0001'");
    assert_eq!(g("w = 2\nx = f'{1.5:.{w}f}'", "x"), "'1.50'");
    assert_eq!(g("x = f'{{literal}}'", "x"), "'{literal}'");
    assert_eq!(g("v = 1\nx = f'{{{v}}}'", "x"), "'{1}'");
    assert_eq!(g("v = 1\nx = f'{v}{{}}'", "x"), "'1{}'");
}

// `%` by an integer literal inside a counted loop lowers to native `Mod` plus a
// branchless floor correction (compiler::emit_native_mod) instead of the host
// `BINOP` call. Native `Mod` truncates like C, so every case where Python's
// floored `%` disagrees with truncation has to be pinned — a negative dividend,
// a negative divisor, and both at once. The loops are long enough to be JIT
// -compiled, so these run as native code, not through the interpreter.
#[test]
fn native_modulo_in_loops_matches_floored_semantics() {
    // Positive operands: truncation and flooring agree.
    assert_eq!(
        g("s = 0\nfor i in range(200): s += i % 7\nx = s", "x"),
        "594"
    );
    // Negative dividend, positive divisor: result must be in [0, k).
    assert_eq!(
        g("s = 0\nfor i in range(-200, 0): s += i % 7\nx = s", "x"),
        "606"
    );
    // Negative divisor: result must be in (k, 0].
    assert_eq!(
        g("s = 0\nfor i in range(-200, 200): s += i % -7\nx = s", "x"),
        "-1201"
    );
    // `%` feeding a condition stays exact (the branchless-accumulation path).
    assert_eq!(
        g(
            "c = 0\nfor i in range(-300, 300):\n    if i % 3 == 0:\n        c += 1\nx = c",
            "x"
        ),
        "200"
    );
    // A dividend that leaves i64 mid-loop: the native path must hand off to the
    // bignum hook rather than wrap. 3000000000**3 is well past 2**63.
    assert_eq!(
        g(
            "s = 0\nfor i in range(3000000000, 3000000200): s += (i * i * i) % 1000000007\nx = s",
            "x"
        ),
        "21253743547"
    );
}

// `(a * b) % k` and `(a * b + c) % k` fuse into fusevm's `MulModFloor` /
// `MulAddModFloor`, which take the product in i128. Python reduces the EXACT
// product, so a product that leaves i64 must give the same answer as the bignum
// path it replaces — that is what these pin, along with the floored sign for
// every combination of operand and divisor sign.
#[test]
fn fused_modular_arithmetic_matches_exact_python() {
    // Products well past 2**63 every iteration; the unfused form would wrap.
    assert_eq!(
        g(
            "s = 0\nfor i in range(2000000, 2000200): s += (i * i * i) % 1000000007\nx = s",
            "x"
        ),
        "98924377940"
    );
    // The linear-congruential shape, `(a*b + c) % k`.
    assert_eq!(
        g(
            "s = 0\nfor i in range(1000): s += (i * 6364136223846793005 + 1442695040888963407) % 1000000007\nx = s",
            "x"
        ),
        "501053277580"
    );
    // Written the other way round: `(c + a*b) % k`.
    assert_eq!(
        g(
            "s = 0\nfor i in range(500): s += (99 + i * 31) % 1000\nx = s",
            "x"
        ),
        "247750"
    );
    // Negative dividend and negative divisor: the result is floored, so it takes
    // the divisor's sign — the fused ops must not leave a C-truncated remainder.
    assert_eq!(
        g(
            "s = 0\nfor i in range(-500, 0): s += (i * 7) % 97\nx = s",
            "x"
        ),
        "24089"
    );
    assert_eq!(
        g(
            "s = 0\nfor i in range(-500, 500): s += (i * i * 7 + i) % -97\nx = s",
            "x"
        ),
        "-48056"
    );
    // A float dividend takes the unfused fallback inside the op and must still
    // floor (CPython: `-4.0 % 2 == 0.0`, not `-0.0`).
    assert_eq!(g("y = -4.0\nx = (y * 1) % 2", "x"), "0.0");
    assert_eq!(g("y = 3.5\nx = (y * -3) % 7", "x"), "3.5");
}

// A native loop assumes the values it seeds its slots with are integers, and
// guards that assumption once in its preamble (`ops::IS_INT`). When a guard
// fails, a generic copy of the same loop runs instead — loop versioning. These
// pin the fallback: every case below seeds a native-shaped loop with something
// that is NOT an int, so the generic copy has to produce Python's answer for it.
#[test]
fn native_loop_type_guard_falls_back_to_the_generic_copy() {
    // `str * int` is repetition, not arithmetic.
    assert_eq!(
        g("s = 'a'\nfor i in range(3): s = s * 2\nx = s", "x"),
        "'aaaaaaaa'"
    );
    // `str % int` is formatting — the case the guard exists for, since the
    // native path's remainder handling would compare the result against 0.
    assert_eq!(
        g("t = 'x=%d'\nfor i in range(1): t = t % 7\nx = t", "x"),
        "'x=7'"
    );
    assert_eq!(
        g("f = 1.5\nfor i in range(4): f = f * 2\nx = f", "x"),
        "24.0"
    );
    assert_eq!(
        g("l = [1]\nfor i in range(3): l = l * 2\nx = len(l)", "x"),
        "8"
    );
    // A `bool` seed is excluded deliberately: it is an int to Python, but the
    // loop would write an `int` back, changing `repr` for an untouched name.
    assert_eq!(g("b = True\nfor i in range(3): b = b + i\nx = b", "x"), "4");
    // The loop variable's last-value binding survives the fallback.
    assert_eq!(
        g("u = 'q'\nfor i in range(5): u = u + 'z'\nx = (i, u)", "x"),
        "(4, 'qzzzzz')"
    );
    // An integer-seeded loop after a guard-failing one still takes the fast path.
    assert_eq!(
        g(
            "s = 'a'\nfor i in range(2): s = s * 2\nn = 0\nfor i in range(100): n += i % 7\nx = n",
            "x"
        ),
        "295"
    );
    // `while` loops are versioned the same way.
    assert_eq!(
        g(
            "s = 'ab'\nc = 0\nwhile c < 3:\n    s = s * 2\n    c += 1\nx = (s, c)",
            "x"
        ),
        "('abababababababab', 3)"
    );
    assert_eq!(
        g(
            "f = 1.0\nd = 0\nwhile d < 4:\n    f = f * 3\n    d += 1\nx = f",
            "x"
        ),
        "81.0"
    );
}

// Container operations must be O(1), not O(n). Four separate paths used to clone
// the whole receiver on every operation — a subscript read, a method call, a
// dict/set key lookup, and an iterator step — which made appending to, indexing,
// iterating, or keying a container quadratic. These sizes are chosen so the old
// behavior does not finish: 60k appends took minutes, 60k dict writes likewise,
// and the suite as a whole runs in well under a second when they are O(1).
#[test]
fn container_operations_are_not_quadratic() {
    // append + index-read
    assert_eq!(
        g("a = []\nfor i in range(60000): a.append(i)\nt = 0\nfor i in range(60000): t += a[i]\nx = t", "x"),
        "1799970000"
    );
    // dict insert + lookup, and set add + membership
    assert_eq!(
        g("d = {}\nfor i in range(60000): d[i] = i\nt = 0\nfor i in range(60000): t += d[i]\nx = t", "x"),
        "1799970000"
    );
    assert_eq!(
        g(
            "s = set()\nfor i in range(60000): s.add(i)\nx = (len(s), 59999 in s)",
            "x"
        ),
        "(60000, True)"
    );
    // iteration over a large list (every step used to copy the list)
    assert_eq!(
        g(
            "a = list(range(60000))\nt = 0\nfor v in a: t += v\nx = t",
            "x"
        ),
        "1799970000"
    );
    // comprehension over a large list, and str building through join
    assert_eq!(
        g("a = [i * 2 for i in range(60000)]\nx = len(a) + a[-1]", "x"),
        "179998"
    );
    assert_eq!(
        g(
            "p = []\nfor i in range(20000): p.append('ab')\nx = len(''.join(p))",
            "x"
        ),
        "40000"
    );
}

#[test]
fn bignum_promotion() {
    assert_eq!(g("x = 2 ** 64", "x"), "18446744073709551616");
    assert_eq!(
        g("f = 1\nfor i in range(1, 26): f = f * i\nx = f", "x"),
        "15511210043330985984000000"
    );
}

#[test]
fn strings_and_fstrings() {
    assert_eq!(g("x = 'a' + 'b' * 3", "x"), "'abbb'");
    assert_eq!(g("x = 'Hello'.upper()", "x"), "'HELLO'");
    assert_eq!(g("x = ' hi '.strip()", "x"), "'hi'");
    assert_eq!(g("n = 42\nx = f'n={n} sq={n*n}'", "x"), "'n=42 sq=1764'");
    assert_eq!(g("x = f'{3.14159:.2f}'", "x"), "'3.14'");
    assert_eq!(g("x = ','.join(['a', 'b', 'c'])", "x"), "'a,b,c'");
    assert_eq!(g("x = 'a,b,c'.split(',')", "x"), "['a', 'b', 'c']");
}

/// str positional-arg cluster: split/rsplit maxsplit, find/rfind/index/rindex
/// and count honoring start/end, startswith/endswith honoring start/end and
/// tuple prefixes. Char-index space, faithful to CPython 3.14.
#[test]
fn str_split_maxsplit() {
    // sep + maxsplit
    assert_eq!(g("x = 'a,b,c'.split(',', 1)", "x"), "['a', 'b,c']");
    assert_eq!(g("x = 'a,b,c'.split(',', 0)", "x"), "['a,b,c']");
    assert_eq!(g("x = 'a,b,c'.split(',', 5)", "x"), "['a', 'b', 'c']");
    assert_eq!(g("x = 'a,b,,c'.split(',')", "x"), "['a', 'b', '', 'c']");
    // whitespace split (sep is None) honors maxsplit; tail keeps inner/trailing ws
    assert_eq!(g("x = '  a  b  c  '.split()", "x"), "['a', 'b', 'c']");
    assert_eq!(g("x = 'a b c d'.split(None, 2)", "x"), "['a', 'b', 'c d']");
}

#[test]
fn str_rsplit_maxsplit() {
    // splits from the right and honors maxsplit
    assert_eq!(g("x = 'a b c'.rsplit(' ', 1)", "x"), "['a b', 'c']");
    assert_eq!(g("x = 'a,b,c,d'.rsplit(',', 2)", "x"), "['a,b', 'c', 'd']");
    assert_eq!(g("x = 'a,b,c'.rsplit(',')", "x"), "['a', 'b', 'c']");
    // whitespace rsplit with maxsplit
    assert_eq!(g("x = 'a b c d'.rsplit(None, 1)", "x"), "['a b c', 'd']");
    // prog-name idiom from the argv drop-in
    assert_eq!(g("x = '/a/b/prog.py'.rsplit('/', 1)[-1]", "x"), "'prog.py'");
}

#[test]
fn str_find_rfind_start_end() {
    assert_eq!(g("x = 'abcabc'.find('a', 1)", "x"), "3");
    assert_eq!(g("x = 'abcabc'.rfind('a')", "x"), "3");
    assert_eq!(g("x = 'abcabc'.find('a', 1, 2)", "x"), "-1");
    assert_eq!(g("x = 'abcabc'.find('c', -2)", "x"), "5");
    assert_eq!(g("x = 'abcabc'.rfind('a', 0, 2)", "x"), "0");
    // unicode: char index (2), not byte index (3, since é is 2 bytes)
    assert_eq!(g("x = 'héllo'.find('l')", "x"), "2");
}

#[test]
fn str_index_rindex_start_end() {
    assert_eq!(g("x = 'abcabc'.index('b', 2)", "x"), "4");
    assert_eq!(g("x = 'abcabc'.rindex('b')", "x"), "4");
    // ValueError when not present in the given range
    assert_eq!(
        g(
            "try:\n    'abcabc'.index('b', 5)\n    x = 'no error'\nexcept ValueError as e:\n    x = type(e).__name__",
            "x"
        ),
        "'ValueError'"
    );
}

#[test]
fn str_count_start_end() {
    assert_eq!(g("x = 'abcabc'.count('a', 1)", "x"), "1");
    assert_eq!(g("x = 'abcabc'.count('a')", "x"), "2");
    assert_eq!(g("x = 'aaa'.count('a', 1, 2)", "x"), "1");
    // empty needle counts gaps within the range
    assert_eq!(g("x = 'abc'.count('')", "x"), "4");
    assert_eq!(g("x = 'abc'.count('', 1)", "x"), "3");
}

#[test]
fn str_startswith_endswith_start_end() {
    assert_eq!(g("x = 'hello'.startswith('l', 2)", "x"), "True");
    assert_eq!(g("x = 'hello'.endswith('ll', 0, 4)", "x"), "True");
    assert_eq!(g("x = 'hello'.startswith('l', 2, 3)", "x"), "True");
    assert_eq!(g("x = 'hello'.startswith('he')", "x"), "True");
    assert_eq!(g("x = 'hello'.endswith('lo')", "x"), "True");
    // tuple of prefixes still works
    assert_eq!(g("x = 'hello'.startswith(('x', 'he'))", "x"), "True");
    assert_eq!(g("x = 'hello'.endswith(('x', 'lo'))", "x"), "True");
    assert_eq!(g("x = 'hello'.startswith(('x', 'y'))", "x"), "False");
}

#[test]
fn percent_format_dispatches_instance_str_repr() {
    // `%s`/`%r`/`%a` must call the user instance's __str__/__repr__ (resolved
    // outside the host borrow), matching CPython byte-for-byte.
    let cls = "class C:\n    def __str__(s): return 'S'\n    def __repr__(s): return 'R'\n";
    assert_eq!(g(&format!("{cls}x = '%s' % C()"), "x"), "'S'");
    assert_eq!(g(&format!("{cls}x = '%r' % C()"), "x"), "'R'");
    assert_eq!(g(&format!("{cls}x = '%a' % C()"), "x"), "'R'");
    // mixed tuple: instance + plain value
    assert_eq!(g(&format!("{cls}x = '%s=%d' % (C(), 5)"), "x"), "'S=5'");
    assert_eq!(
        g(&format!("{cls}x = '%s %r %a' % (C(), C(), C())"), "x"),
        "'S R R'"
    );
    // container holding instances (recurses through repr dispatch)
    assert_eq!(
        g(&format!("{cls}x = '%s' % ([C(), C()],)"), "x"),
        "'[R, R]'"
    );
    assert_eq!(g(&format!("{cls}x = '%r' % ((C(),),)"), "x"), "'(R,)'");
    // mapping form
    assert_eq!(g(&format!("{cls}x = '%(k)r' % {{'k': C()}}"), "x"), "'R'");
    // width/precision still apply after dispatch
    assert_eq!(g(&format!("{cls}x = '[%5s]' % C()"), "x"), "'[    S]'");
    assert_eq!(g(&format!("{cls}x = '%.1s' % C()"), "x"), "'S'");
    // `%=` (desugars to `t = t % v`) goes through the same path
    assert_eq!(g(&format!("{cls}x = '%s'\nx %= C()"), "x"), "'S'");
    // `%a` ascii-escapes a non-ASCII dispatched repr
    assert_eq!(
        g(
            "class U:\n    def __repr__(s): return 'é'\nx = '%a' % U()",
            "x"
        ),
        "'\\\\xe9'"
    );
    // plain values unaffected (no regression)
    assert_eq!(g("x = '%s and %r' % ('a', 'b')", "x"), "\"a and 'b'\"");
}

#[test]
fn fstring_nested_format_specs() {
    // A format spec may itself contain replacement fields, evaluated at runtime
    // and spliced into the spec before formatting (CPython semantics).
    assert_eq!(g("x = f'{3.14159:{5}.{2}f}'", "x"), "' 3.14'");
    assert_eq!(
        g("w = 8\nn = 2\nx = f'{3.14159:{w}.{n}f}'", "x"),
        "'    3.14'"
    );
    assert_eq!(g("w = 8\nx = f'{42:>{w}}'", "x"), "'      42'");
    assert_eq!(g("w = 8\nx = f'{42:0{w}d}'", "x"), "'00000042'");
    assert_eq!(g("x = f'{\"x\":{\"*\"}>{6}}'", "x"), "'*****x'");
    assert_eq!(
        g("w = 10\nx = f'{\"mid\":{\"=\"}^{w}}'", "x"),
        "'===mid===='"
    );
    assert_eq!(g("w = 8\nx = f'{255:#{w}x}'", "x"), "'    0xff'");
    // nested field with its own conversion
    assert_eq!(g("w = 5\nx = f'{3.14:>{w}}'", "x"), "' 3.14'");
    // non-nested spec still works (no regression)
    assert_eq!(g("x = f'{3.14159:.2f}'", "x"), "'3.14'");
    assert_eq!(g("x = f'{42:05d}'", "x"), "'00042'");
}

#[test]
fn lists_dicts_sets_tuples() {
    assert_eq!(g("x = [1, 2, 3] + [4]", "x"), "[1, 2, 3, 4]");
    assert_eq!(g("a = [1, 2]\na.append(3)\nx = a", "x"), "[1, 2, 3]");
    assert_eq!(g("x = {'a': 1, 'b': 2}", "x"), "{'a': 1, 'b': 2}");
    assert_eq!(
        g("d = {'a': 1}\nd['b'] = 2\nx = d", "x"),
        "{'a': 1, 'b': 2}"
    );
    assert_eq!(g("x = sorted({3, 1, 2, 1})", "x"), "[1, 2, 3]");
    assert_eq!(g("x = (1, 2, 3)[1]", "x"), "2");
}

#[test]
fn slicing() {
    assert_eq!(g("x = list(range(10))[2:8:2]", "x"), "[2, 4, 6]");
    assert_eq!(g("x = [1, 2, 3, 4, 5][::-1]", "x"), "[5, 4, 3, 2, 1]");
    assert_eq!(g("x = 'python'[1:4]", "x"), "'yth'");
    assert_eq!(g("x = [1, 2, 3, 4][-2:]", "x"), "[3, 4]");
}

#[test]
fn comprehensions() {
    assert_eq!(g("x = [i * i for i in range(5)]", "x"), "[0, 1, 4, 9, 16]");
    assert_eq!(
        g("x = [i for i in range(10) if i % 2 == 0]", "x"),
        "[0, 2, 4, 6, 8]"
    );
    assert_eq!(
        g("x = {i: i * i for i in range(3)}", "x"),
        "{0: 0, 1: 1, 2: 4}"
    );
    assert_eq!(
        g("x = [y for row in [[1, 2], [3, 4]] for y in row]", "x"),
        "[1, 2, 3, 4]"
    );
}

#[test]
fn functions_defaults_varargs() {
    assert_eq!(g("def f(a, b=10):\n    return a + b\nx = f(5)", "x"), "15");
    assert_eq!(
        g(
            "def f(*args):\n    return sum(args)\nx = f(1, 2, 3, 4)",
            "x"
        ),
        "10"
    );
    assert_eq!(
        g("def f(a, **kw):\n    return kw['k']\nx = f(1, k=99)", "x"),
        "99"
    );
}

#[test]
fn closures() {
    assert_eq!(
        g(
            "def make(n):\n    def add(x):\n        return x + n\n    return add\nx = make(10)(5)",
            "x"
        ),
        "15"
    );
}

#[test]
fn classes_and_inheritance() {
    let src = "\
class A:
    def __init__(self, v):
        self.v = v
    def go(self):
        return self.v * 2
class B(A):
    def go(self):
        return self.v * 3
x = B(7).go()";
    assert_eq!(g(src, "x"), "21");
    assert_eq!(g("class A:\n    pass\nx = isinstance(A(), A)", "x"), "True");
}

#[test]
fn exceptions() {
    let src = "\
try:
    z = 1 / 0
    y = 'no'
except ZeroDivisionError:
    y = 'caught'
finally:
    w = 'done'
x = y + '/' + w";
    assert_eq!(g(src, "x"), "'caught/done'");
    assert_eq!(
        g(
            "try:\n    raise ValueError('boom')\nexcept ValueError as e:\n    x = str(e)",
            "x"
        ),
        "'boom'"
    );
}

#[test]
fn builtins_and_hof() {
    assert_eq!(
        g("x = list(map(lambda n: n * 2, [1, 2, 3]))", "x"),
        "[2, 4, 6]"
    );
    assert_eq!(
        g("x = list(filter(lambda n: n > 2, [1, 2, 3, 4]))", "x"),
        "[3, 4]"
    );
    assert_eq!(g("x = sorted([3, 1, 2], reverse=True)", "x"), "[3, 2, 1]");
    assert_eq!(g("x = max([1, 5, 3], key=lambda n: -n)", "x"), "1");
    assert_eq!(g("x = sum(range(101))", "x"), "5050");
    assert_eq!(
        g("x = list(enumerate(['a', 'b']))", "x"),
        "[(0, 'a'), (1, 'b')]"
    );
}

#[test]
fn control_flow() {
    assert_eq!(
        g(
            "x = 0\nfor i in range(5):\n    if i == 3:\n        break\n    x += i",
            "x"
        ),
        "3"
    );
    assert_eq!(
        g(
            "x = []\nfor i in range(5):\n    if i % 2:\n        continue\n    x.append(i)",
            "x"
        ),
        "[0, 2, 4]"
    );
    assert_eq!(g("x = 'yes' if 5 > 3 else 'no'", "x"), "'yes'");
}

#[test]
fn cache_roundtrip_is_transparent() {
    // Running the same source twice must produce the same value (2nd run served
    // from the rkyv cache).
    let src = "x = sum([i * i for i in range(10)])";
    assert_eq!(g(src, "x"), "285");
    assert_eq!(g(src, "x"), "285");
}

#[test]
fn operator_dunders() {
    // Arithmetic / comparison operator overloading via dunders on a user class.
    let src = "
class V:
    def __init__(self, x): self.x = x
    def __add__(self, o): return V(self.x + o.x)
    def __sub__(self, o): return V(self.x - o.x)
    def __mul__(self, k): return V(self.x * k)
    def __mod__(self, o): return V(self.x % o.x)
    def __eq__(self, o): return self.x == o.x
    def __lt__(self, o): return self.x < o.x
a = (V(2) + V(3)).x
b = (V(10) - V(4)).x
c = (V(5) * 4).x
d = (V(17) % V(5)).x
e = V(1) == V(1)
f = V(1) == V(2)
g_ = V(1) < V(2)
xs = [v.x for v in sorted([V(3), V(1), V(2)])]
";
    assert_eq!(g(src, "a"), "5");
    assert_eq!(g(src, "b"), "6");
    assert_eq!(g(src, "c"), "20");
    assert_eq!(g(src, "d"), "2");
    assert_eq!(g(src, "e"), "True");
    assert_eq!(g(src, "f"), "False");
    assert_eq!(g(src, "g_"), "True");
    assert_eq!(g(src, "xs"), "[1, 2, 3]");
}

#[test]
fn dunder_repr_in_containers() {
    // `str`/`repr` of a container must dispatch each element's `__repr__`.
    let src = "
class P:
    def __init__(self, n): self.n = n
    def __repr__(self): return f'P({self.n})'
lst = str([P(1), P(2)])
tup = str((P(3),))
dct = str({'k': P(4)})
";
    assert_eq!(g(src, "lst"), "'[P(1), P(2)]'");
    assert_eq!(g(src, "tup"), "'(P(3),)'");
    assert_eq!(g(src, "dct"), "\"{'k': P(4)}\"");
}

// ── generators / yield ────────────────────────────────────────────────────────

#[test]
fn generators_basic() {
    let src = "
def count(n):
    i = 0
    while i < n:
        yield i
        i += 1
whole = list(count(5))
first_two = [0, 0]
g2 = count(2)
first_two[0] = next(g2)
first_two[1] = next(g2)
total = sum(count(10))
loop = []
for v in count(3):
    loop.append(v)
";
    assert_eq!(g(src, "whole"), "[0, 1, 2, 3, 4]");
    assert_eq!(g(src, "first_two"), "[0, 1]");
    assert_eq!(g(src, "total"), "45");
    assert_eq!(g(src, "loop"), "[0, 1, 2]");
}

#[test]
fn generator_loop_break_continue_across_try() {
    // A generator whose loop `continue`/`break` crosses a `try`/`finally`
    // boundary. Both the signal-driven loop lowering (needed so a `finally` runs
    // before the loop exit) and `yield` inside that lowered body must work at
    // once. Regression: the compiler took the native jump-patch path for any
    // loop containing `yield`, but a `continue` inside an `except` compiles into
    // the try handler's own chunk, so patching its jump into the main chunk
    // panicked (`patch_jump on non-jump op`) — this is exactly the shape in the
    // vendored stdlib `os._walk`.
    let src = "
def gen(items):
    i = 0
    while i < len(items):
        x = items[i]
        i += 1
        try:
            if x < 0:
                raise ValueError
            yield 100 // x
        except (ValueError, ZeroDivisionError):
            continue
out = list(gen([2, 0, 5, -1, 4]))

def stop_at(n):
    i = 0
    while True:
        try:
            if i >= n:
                break
            yield i * i
        finally:
            i += 1
squares = list(stop_at(4))
";
    assert_eq!(g(src, "out"), "[50, 20, 25]");
    assert_eq!(g(src, "squares"), "[0, 1, 4, 9]");
}

#[test]
fn generators_yield_expression_and_from() {
    // A `yield` expression receives the value passed to the caller's resume; a
    // plain iteration sends None (falsy), so the echo accumulates the yields.
    let src = "
def squares(xs):
    for x in xs:
        yield x * x
def chained():
    yield from range(3)
    yield from [7, 8]
sq = list(squares(range(4)))
ch = list(chained())
# lazy generator expression: type is generator, evaluated on demand
gx = (i * i for i in range(5))
tname = type(gx).__name__
vals = list(gx)
filtered = list(n for n in range(6) if n % 2 == 0)
";
    assert_eq!(g(src, "sq"), "[0, 1, 4, 9]");
    assert_eq!(g(src, "ch"), "[0, 1, 2, 7, 8]");
    assert_eq!(g(src, "tname"), "'generator'");
    assert_eq!(g(src, "vals"), "[0, 1, 4, 9, 16]");
    assert_eq!(g(src, "filtered"), "[0, 2, 4]");
}

#[test]
fn generator_is_lazy() {
    // A generator expression must NOT evaluate its body eagerly: only the two
    // elements actually consumed by `next` are produced (an eager list would
    // divide by zero on the 0 element).
    let src = "
seen = []
def tap(x):
    seen.append(x)
    return x
gen = (tap(i) for i in range(100))
one = next(gen)
two = next(gen)
consumed = list(seen)
";
    assert_eq!(g(src, "one"), "0");
    assert_eq!(g(src, "two"), "1");
    assert_eq!(g(src, "consumed"), "[0, 1]");
}

// ── call-site * / ** unpacking ────────────────────────────────────────────────

#[test]
fn call_arg_unpacking() {
    let src = "
def f(a, b, c):
    return (a, b, c)
lst = [10, 20, 30]
r1 = f(*lst)
r2 = f(*[1], *[2, 3])
r3 = f(1, *[2], 3)
def h(a, b, c, x=0, y=0):
    return (a, b, c, x, y)
r4 = h(*[1, 2], 3, **{'x': 9}, y=8)
def var(*args, **kwargs):
    return (args, sorted(kwargs.items()))
r5 = var(*[1, 2], 3, **{'k': 4}, z=5)
";
    assert_eq!(g(src, "r1"), "(10, 20, 30)");
    assert_eq!(g(src, "r2"), "(1, 2, 3)");
    assert_eq!(g(src, "r3"), "(1, 2, 3)");
    assert_eq!(g(src, "r4"), "(1, 2, 3, 9, 8)");
    assert_eq!(g(src, "r5"), "((1, 2, 3), [('k', 4), ('z', 5)])");
}

// ── literal spreads ──────────────────────────────────────────────────────────

#[test]
fn literal_spreads() {
    assert_eq!(g("x = [*[1, 2], 3, *[4, 5]]", "x"), "[1, 2, 3, 4, 5]");
    assert_eq!(g("x = (*[1, 2], 3)", "x"), "(1, 2, 3)");
    assert_eq!(g("x = sorted({*[1, 2], *[2, 3, 4]})", "x"), "[1, 2, 3, 4]");
    // ** dict spread with later keys overriding earlier ones, insertion order.
    assert_eq!(
        g("x = {**{'a': 1}, 'b': 2, **{'c': 3, 'a': 10}}", "x"),
        "{'a': 10, 'b': 2, 'c': 3}"
    );
    // None is a legal dict key and must not be confused with a ** spread slot.
    assert_eq!(g("x = {**{'a': 1}, None: 2}", "x"), "{'a': 1, None: 2}");
}

// ── match / case ──────────────────────────────────────────────────────────────

#[test]
fn match_literal_capture_wildcard_or_guard() {
    let src = "
def d(v):
    match v:
        case 0:
            return 'zero'
        case 1 | 2 | 3:
            return 'small'
        case int() if v > 100:
            return 'big'
        case str() as s:
            return 'str:' + s
        case _:
            return 'other'
a = d(0)
b = d(2)
c = d(200)
e = d('hi')
f = d(3.5)
";
    assert_eq!(g(src, "a"), "'zero'");
    assert_eq!(g(src, "b"), "'small'");
    assert_eq!(g(src, "c"), "'big'");
    assert_eq!(g(src, "e"), "'str:hi'");
    assert_eq!(g(src, "f"), "'other'");
}

#[test]
fn match_sequence_and_mapping() {
    let src = "
def d(v):
    match v:
        case [a, b]:
            return ('pair', a, b)
        case [a, *rest]:
            return ('head', a, rest)
        case {'name': n, 'age': age}:
            return ('person', n, age)
        case _:
            return ('other',)
p = d([10, 20])
h = d([1, 2, 3, 4])
m = d({'name': 'Al', 'age': 30})
rest_bind = None
match {'k': 1, 'a': 2, 'b': 3}:
    case {'k': v, **others}:
        rest_bind = (v, sorted(others.items()))
";
    assert_eq!(g(src, "p"), "('pair', 10, 20)");
    assert_eq!(g(src, "h"), "('head', 1, [2, 3, 4])");
    assert_eq!(g(src, "m"), "('person', 'Al', 30)");
    assert_eq!(g(src, "rest_bind"), "(1, [('a', 2), ('b', 3)])");
}

#[test]
fn match_class_patterns() {
    let src = "
class Point:
    __match_args__ = ('x', 'y')
    def __init__(self, x, y):
        self.x = x
        self.y = y
def loc(p):
    match p:
        case Point(0, 0):
            return 'origin'
        case Point(x=0, y=y):
            return ('y-axis', y)
        case Point(x, y):
            return ('point', x, y)
        case _:
            return '?'
a = loc(Point(0, 0))
b = loc(Point(0, 5))
c = loc(Point(3, 4))
";
    assert_eq!(g(src, "a"), "'origin'");
    assert_eq!(g(src, "b"), "('y-axis', 5)");
    assert_eq!(g(src, "c"), "('point', 3, 4)");
}

// ── nonlocal ──────────────────────────────────────────────────────────────────

#[test]
fn nonlocal_rebinds_enclosing_function_scope() {
    // `nonlocal` writes to the nearest enclosing FUNCTION scope, distinct from
    // `global` (which would touch module scope).
    let src = "
def counter():
    n = 0
    def inc():
        nonlocal n
        n += 1
        return n
    return inc
c = counter()
calls = [c(), c(), c()]
outer_x = 'g'
def outer():
    x = 'outer'
    def inner():
        nonlocal x
        x = 'changed'
    inner()
    return x
changed = outer()
still_global = outer_x
";
    assert_eq!(g(src, "calls"), "[1, 2, 3]");
    assert_eq!(g(src, "changed"), "'changed'");
    // The module-level name of the same spelling must be untouched.
    assert_eq!(g(src, "still_global"), "'g'");
}

#[test]
fn nonlocal_skips_to_deep_enclosing_scope() {
    let src = "
def deep():
    a = 1
    def mid():
        def inner():
            nonlocal a
            a = 99
        inner()
    mid()
    return a
x = deep()
";
    assert_eq!(g(src, "x"), "99");
}

// ── comprehension own-scope ───────────────────────────────────────────────────

#[test]
fn comprehension_loop_var_does_not_leak() {
    // Python 3 gives comprehensions their own scope: the loop variable must not
    // leak, but enclosing variables are still readable.
    assert_eq!(
        g("i = 'before'\nsq = [i * i for i in range(4)]\nx = i", "x"),
        "'before'"
    );
    assert_eq!(
        g("k = 'keep'\nd = {v: v for v in range(2)}\nx = k", "x"),
        "'keep'"
    );
    // Enclosing var is read inside the comprehension.
    assert_eq!(
        g("y = 100\nx = [n + y for n in range(3)]", "x"),
        "[100, 101, 102]"
    );
    // Nested comprehension loop vars also stay contained.
    assert_eq!(
        g(
            "j = 'j'\nx = [a * b for a in range(2) for b in range(3)]\nleaked = j",
            "leaked"
        ),
        "'j'"
    );
}

// ── Python floor division / modulo semantics ─────────────────────────────────

#[test]
fn floor_division_signs() {
    // `//` floors toward negative infinity for every sign combination.
    assert_eq!(g("x = -7 // 2", "x"), "-4");
    assert_eq!(g("x = 7 // -2", "x"), "-4");
    assert_eq!(g("x = -7 // -2", "x"), "3");
    assert_eq!(g("x = 7 // 2", "x"), "3");
    // A large operand exercises the BigInt floor path.
    assert_eq!(g("x = (-7 * 10**30) // (3 * 10**20)", "x"), "-23333333334");
}

#[test]
fn modulo_takes_divisor_sign() {
    // `%` result carries the sign of the divisor.
    assert_eq!(g("x = -7 % 100", "x"), "93");
    assert_eq!(g("x = -7 % -100", "x"), "-7");
    assert_eq!(g("x = 7 % -100", "x"), "-93");
    assert_eq!(g("x = 0 % -5", "x"), "0");
    // Float modulo also floors.
    assert_eq!(g("x = -7.0 % 3.0", "x"), "2.0");
    // BigInt modulo path.
    assert_eq!(g("x = (-7 * 10**25) % 100", "x"), "0");
    assert_eq!(g("x = (-(10**25) - 7) % 100", "x"), "93");
}

#[test]
fn pow_three_arg_modular() {
    assert_eq!(g("x = pow(2, 10, 1000)", "x"), "24");
    assert_eq!(g("x = pow(3, 4, 5)", "x"), "1");
    // Large exponent must not overflow (modular square-and-multiply).
    assert_eq!(g("x = pow(2, 1000, 10**9 + 7)", "x"), "688423210");
    // Negative base normalizes to the modulus sign.
    assert_eq!(g("x = pow(-3, 3, 7)", "x"), "1");
    // Negative modulus yields a non-positive result.
    assert_eq!(g("x = pow(2, 3, -5)", "x"), "-2");
}

// ── printf-style `str % args` ────────────────────────────────────────────────

#[test]
fn percent_format_numeric() {
    assert_eq!(g("x = '%.2f' % 3.14159", "x"), "'3.14'");
    assert_eq!(g("x = '%5d' % 42", "x"), "'   42'");
    assert_eq!(g("x = '%-5d|' % 42", "x"), "'42   |'");
    assert_eq!(g("x = '%05d' % 42", "x"), "'00042'");
    assert_eq!(g("x = '%+d' % 7", "x"), "'+7'");
    assert_eq!(g("x = '% d' % 7", "x"), "' 7'");
    assert_eq!(g("x = '%x' % 255", "x"), "'ff'");
    assert_eq!(g("x = '%#x' % 255", "x"), "'0xff'");
    assert_eq!(g("x = '%o' % 8", "x"), "'10'");
    assert_eq!(g("x = '%e' % 12345.678", "x"), "'1.234568e+04'");
    assert_eq!(g("x = '%.2e' % 12345.678", "x"), "'1.23e+04'");
    assert_eq!(g("x = '%g' % 0.0001", "x"), "'0.0001'");
    assert_eq!(g("x = '%g' % 0.00001", "x"), "'1e-05'");
    assert_eq!(g("x = '%g' % 1000000", "x"), "'1e+06'");
}

#[test]
fn percent_format_strings_and_star() {
    assert_eq!(g("x = '%s=%s' % ('k', 3)", "x"), "'k=3'");
    assert_eq!(g("x = '%r' % 'hi'", "x"), "\"'hi'\"");
    assert_eq!(g("x = '%.3s' % 'abcdef'", "x"), "'abc'");
    // `*` pulls width / precision from the argument tuple.
    assert_eq!(g("x = '%*d' % (5, 42)", "x"), "'   42'");
    assert_eq!(g("x = '%.*f' % (2, 3.14159)", "x"), "'3.14'");
    // Mapping form.
    assert_eq!(
        g("x = '%(name)s is %(age)d' % {'name': 'x', 'age': 5}", "x"),
        "'x is 5'"
    );
    assert_eq!(g("x = '%c%c' % (72, 105)", "x"), "'Hi'");
    assert_eq!(g("x = '100%%' % ()", "x"), "'100%'");
}

#[test]
fn bignum_bitwise_shift_and_conversions() {
    // Shifts route through the BigInt path (no i64 wraparound / no panic).
    assert_eq!(g("x = 1 << 64", "x"), "18446744073709551616");
    assert_eq!(g("x = 1 << 100", "x"), "1267650600228229401496703205376");
    assert_eq!(g("x = -5 >> 1", "x"), "-3");
    // Bitwise ops on values beyond i64.
    assert_eq!(g("x = (10 ** 30) & 7", "x"), "0");
    assert_eq!(g("x = ~(10 ** 20)", "x"), "-100000000000000000001");
    // Exact integer comparison beyond f64 precision.
    assert_eq!(g("x = 10 ** 20 < 10 ** 20 + 1", "x"), "True");
    // int(float) and radix conversions are bignum-safe.
    assert_eq!(g("x = int(1e20)", "x"), "100000000000000000000");
    assert_eq!(g("x = hex(10 ** 20)", "x"), "'0x56bc75e2d63100000'");
    assert_eq!(g("x = abs(-(10 ** 20))", "x"), "100000000000000000000");
    // Base parsing with a prefix, and underscores.
    assert_eq!(g("x = int('0x1F', 16)", "x"), "31");
    assert_eq!(g("x = int('1_000')", "x"), "1000");
    // `bool` bit-ops stay `bool`.
    assert_eq!(g("x = True & False", "x"), "False");
    assert_eq!(g("x = True | False", "x"), "True");
}

#[test]
fn negative_shift_is_catchable_valueerror() {
    // `1 >> -1` must raise a catchable ValueError, never abort the process.
    assert_eq!(
        g(
            "try:\n    1 >> -1\n    x = 'no error'\nexcept ValueError as e:\n    x = str(e)",
            "x"
        ),
        "'negative shift count'"
    );
}

#[test]
fn custom_getitem_slice_and_slice_repr() {
    // A user `__getitem__` receiving a slice must not stack-overflow, and the
    // returned slice object reprs like CPython.
    assert_eq!(
        g(
            "class C:\n    def __getitem__(self, k):\n        return k\nx = C()[1:5:2]",
            "x"
        ),
        "slice(1, 5, 2)"
    );
    assert_eq!(
        g(
            "class C:\n    def __getitem__(self, k):\n        return k\nx = C()[::-1]",
            "x"
        ),
        "slice(None, None, -1)"
    );
}

#[test]
fn static_and_class_methods() {
    let src = "
class C:
    tag = 'cls'
    @staticmethod
    def f(x):
        return x * 2
    @classmethod
    def g(cls, x):
        return cls.tag + str(x)
    @classmethod
    def make(cls):
        return cls()
class D(C):
    tag = 'D'
via_cls = C.f(5)
via_inst = C().f(3)
cm_cls = C.g(5)
cm_inst = C().g(7)
cm_inherit = D.g(9)
unbound = (lambda h: h(10))(C.f)
alt_ctor = type(C.make()).__name__
";
    assert_eq!(g(src, "via_cls"), "10");
    assert_eq!(g(src, "via_inst"), "6");
    assert_eq!(g(src, "cm_cls"), "'cls5'");
    assert_eq!(g(src, "cm_inst"), "'cls7'");
    // classmethod binds the *derived* class, so D.g sees D.tag.
    assert_eq!(g(src, "cm_inherit"), "'D9'");
    assert_eq!(g(src, "unbound"), "20");
    assert_eq!(g(src, "alt_ctor"), "'C'");
}

#[test]
fn type_returns_a_real_class() {
    // type(x) compares/repr's as a class, not an internal builtin-function object.
    assert_eq!(g("x = type(5) == int", "x"), "True");
    assert_eq!(g("x = type('a') == str", "x"), "True");
    assert_eq!(g("x = type([]) == list", "x"), "True");
    assert_eq!(g("x = type(5) is int", "x"), "True");
    assert_eq!(g("x = type(5) is str", "x"), "False");
    assert_eq!(g("x = isinstance(int, type)", "x"), "True");
    assert_eq!(g("x = str(int)", "x"), "\"<class 'int'>\"");
    // A user class: type(instance) equals and is-identical to the class object.
    let src =
        "class B:\n    pass\nb = B()\neq = type(b) == B\nis_ = type(b) is B\nnm = type(b).__name__";
    assert_eq!(g(src, "eq"), "True");
    assert_eq!(g(src, "is_"), "True");
    assert_eq!(g(src, "nm"), "'B'");
}

#[test]
fn super_cooperative_inheritance() {
    // super().__init__ + method extension through a single chain.
    let src = "
class A:
    def __init__(self, x):
        self.x = x
    def greet(self):
        return 'A' + str(self.x)
class B(A):
    def __init__(self, x, y):
        super().__init__(x)
        self.y = y
    def greet(self):
        return super().greet() + 'B' + str(self.y)
b = B(1, 2)
coords = (b.x, b.y)
msg = b.greet()
";
    assert_eq!(g(src, "coords"), "(1, 2)");
    assert_eq!(g(src, "msg"), "'A1B2'");
}

#[test]
fn super_diamond_c3_mro() {
    // Cooperative super() across a diamond must visit each base once, in C3 order.
    let src = "
class A:
    def m(self):
        return ['A']
class B(A):
    def m(self):
        return ['B'] + super().m()
class C(A):
    def m(self):
        return ['C'] + super().m()
class D(B, C):
    def m(self):
        return ['D'] + super().m()
x = D().m()
";
    assert_eq!(g(src, "x"), "['D', 'B', 'C', 'A']");
}

#[test]
fn numeric_keys_unify_in_dict_and_set() {
    // 1, 1.0, True hash and compare equal, so they collapse to one key.
    assert_eq!(g("x = 1.0 in {1}", "x"), "True");
    assert_eq!(g("x = True in {1}", "x"), "True");
    assert_eq!(g("x = len({1, 1.0, True})", "x"), "1");
    // The set keeps the FIRST-inserted element object (1, an int).
    assert_eq!(g("x = sorted({1, 1.0, True})", "x"), "[1]");
    assert_eq!(g("x = {1, 1.0, True}", "x"), "{1}");
    // Dict keeps the first key object, updates the value.
    assert_eq!(g("x = {1: 'a', 1.0: 'b', True: 'c'}", "x"), "{1: 'c'}");
    assert_eq!(
        g("d = {}\nd[1] = 'a'\nd[1.0] = 'b'\nx = d", "x"),
        "{1: 'b'}"
    );
    // Bignum-valued float unifies with the bignum int key.
    assert_eq!(g("x = len({10 ** 20, float(10 ** 20)})", "x"), "1");
    // Merge / update follow the same rule.
    assert_eq!(g("x = {**{1: 'a'}, **{1.0: 'b'}}", "x"), "{1: 'b'}");
    assert_eq!(
        g("d = {1.0: 'a'}\nd.update({1: 'b'})\nx = d", "x"),
        "{1.0: 'b'}"
    );
    // float() accepts bignums and underscore-grouped literals.
    assert_eq!(g("x = float('1_000.5')", "x"), "1000.5");
}

#[test]
fn round_bankers_and_negative_ndigits() {
    // Round-half-to-even (banker's), returning an int with no ndigits.
    assert_eq!(g("x = round(2.5)", "x"), "2");
    assert_eq!(g("x = round(0.5)", "x"), "0");
    assert_eq!(g("x = round(1.5)", "x"), "2");
    assert_eq!(g("x = round(-2.5)", "x"), "-2");
    // Representation-correct: 2.675 is really 2.6749…, so it rounds down.
    assert_eq!(g("x = round(2.675, 2)", "x"), "2.67");
    assert_eq!(g("x = round(1.5 / 10.0, 1)", "x"), "0.1");
    // ndigits present -> float, even for a whole result.
    assert_eq!(g("x = round(2.5, 0)", "x"), "2.0");
    // Negative ndigits round ints/floats to powers of ten (half-to-even).
    assert_eq!(g("x = round(12345, -2)", "x"), "12300");
    assert_eq!(g("x = round(1250, -2)", "x"), "1200");
    assert_eq!(g("x = round(1350, -2)", "x"), "1400");
    assert_eq!(g("x = round(123.456, -1)", "x"), "120.0");
}

#[test]
fn format_negative_and_bignum_radix() {
    // Negative ints format as sign + magnitude, not two's complement.
    assert_eq!(g("x = '{:b}'.format(-7)", "x"), "'-111'");
    assert_eq!(g("x = '{:x}'.format(-255)", "x"), "'-ff'");
    assert_eq!(g("x = '{:#x}'.format(-255)", "x"), "'-0xff'");
    assert_eq!(g("x = '{:08b}'.format(-7)", "x"), "'-0000111'");
    // Bignum-safe radix + decimal formatting.
    assert_eq!(g("x = '{:x}'.format(10 ** 20)", "x"), "'56bc75e2d63100000'");
    assert_eq!(
        g("x = '{:d}'.format(10 ** 20)", "x"),
        "'100000000000000000000'"
    );
    // The `format()` builtin path (regression: had a double-borrow panic).
    assert_eq!(g("x = format(255, 'x')", "x"), "'ff'");
    assert_eq!(g("x = format(-7, 'b')", "x"), "'-111'");
}

#[test]
fn slice_negative_step_clamping() {
    // Start beyond len with a negative step clamps to the last index.
    assert_eq!(g("x = [1, 2, 3, 4, 5][5:-2:-2]", "x"), "[5]");
    assert_eq!(g("x = (10, 20, 30, 40)[5::-2]", "x"), "(40, 20)");
    assert_eq!(g("x = (10, 20, 30, 40)[5:-2:-2]", "x"), "(40,)");
    assert_eq!(g("x = [0, 1, 2, 3, 4, 5, 6][10:2:-2]", "x"), "[6, 4]");
    assert_eq!(g("x = [1, 2, 3, 4, 5][-1:-4:-1]", "x"), "[5, 4, 3]");
}

#[test]
fn range_membership_is_constant_time() {
    // O(1) membership must not iterate a huge range.
    assert_eq!(g("x = 999999999999 in range(1000000000000)", "x"), "True");
    assert_eq!(g("x = 4 in range(0, 10, 2)", "x"), "True");
    assert_eq!(g("x = 5 in range(0, 10, 2)", "x"), "False");
    assert_eq!(g("x = 4 in range(10, 0, -2)", "x"), "True");
    // Integral float equals its int value; a fractional float never matches.
    assert_eq!(g("x = 2.0 in range(5)", "x"), "True");
    assert_eq!(g("x = 2.5 in range(5)", "x"), "False");
}

#[test]
fn property_descriptor() {
    // Read-only property.
    assert_eq!(
        g(
            "class C:\n    @property\n    def x(self): return 42\nx = C().x",
            "x"
        ),
        "42"
    );
    // getter + setter round-trip.
    assert_eq!(
        g(
            "class C:\n    @property\n    def v(self): return self._v\n    @v.setter\n    def v(self, n): self._v = n * 2\nc = C()\nc.v = 5\nx = c.v",
            "x"
        ),
        "10"
    );
    // property() functional form with fget/fset.
    assert_eq!(
        g(
            "class C:\n    def _g(self): return self._n + 1\n    def _s(self, n): self._n = n\n    n = property(_g, _s)\nc = C()\nc.n = 10\nx = c.n",
            "x"
        ),
        "11"
    );
}

#[test]
fn user_data_descriptor() {
    // A data descriptor (__get__/__set__) overrides the instance dict.
    assert_eq!(
        g(
            "class D:\n    def __get__(self, o, t=None): return o._raw * 3\n    def __set__(self, o, val): o._raw = val\nclass C:\n    d = D()\nc = C()\nc.d = 4\nx = c.d",
            "x"
        ),
        "12"
    );
}

#[test]
fn set_name_hook() {
    assert_eq!(
        g(
            "seen = []\nclass D:\n    def __set_name__(self, owner, name): seen.append((owner.__name__, name))\nclass C:\n    a = D()\n    b = D()\nx = seen",
            "x"
        ),
        "[('C', 'a'), ('C', 'b')]"
    );
}

#[test]
fn call_dunder() {
    assert_eq!(
        g(
            "class C:\n    def __call__(self, x): return x + 1\nc = C()\nx = c(41)",
            "x"
        ),
        "42"
    );
    assert_eq!(
        g(
            "class C:\n    def __call__(self): return 0\nx = callable(C())",
            "x"
        ),
        "True"
    );
    assert_eq!(g("class C:\n    pass\nx = callable(C())", "x"), "False");
}

#[test]
fn getattr_fallback() {
    assert_eq!(
        g(
            "class C:\n    def __getattr__(self, n): return 'dyn:' + n\nx = C().missing",
            "x"
        ),
        "'dyn:missing'"
    );
}

#[test]
fn format_dunder() {
    // f-string honors __format__ with the spec.
    assert_eq!(
        g(
            "class C:\n    def __format__(self, s): return 'F[' + s + ']'\nx = f'{C():>3}'",
            "x"
        ),
        "'F[>3]'"
    );
    // str.format honors __format__ and !r conversion.
    assert_eq!(
        g("class C:\n    def __format__(self, s): return 'z'\n    def __repr__(self): return 'R'\nx = '{}-{!r}'.format(C(), C())", "x"),
        "'z-R'"
    );
    // format() builtin.
    assert_eq!(
        g(
            "class C:\n    def __format__(self, s): return 'q' + s\nx = format(C(), 'w')",
            "x"
        ),
        "'qw'"
    );
}

#[test]
fn ne_derived_and_not_implemented() {
    // __ne__ is derived from __eq__ when not defined.
    assert_eq!(
        g("class C:\n    def __init__(s, v): s.v = v\n    def __eq__(s, o): return s.v == o.v\nx = (C(1) == C(1), C(1) != C(2), C(1) != C(1))", "x"),
        "(True, True, False)"
    );
    // Returning NotImplemented falls back to identity (== against a foreign type).
    assert_eq!(
        g("class A:\n    def __eq__(s, o):\n        if isinstance(o, A): return True\n        return NotImplemented\nx = (A() == A(), A() == 5, 5 == A())", "x"),
        "(True, False, False)"
    );
}

#[test]
fn unary_dunders() {
    // Unwrap to scalars so the test doesn't depend on __repr__ dispatch in the
    // read-back harness (repr_of is &self and can't call a method).
    assert_eq!(
        g("class V:\n    def __init__(s, x): s.x = x\n    def __neg__(s): return V(-s.x)\n    def __abs__(s): return V(abs(s.x))\n    def __invert__(s): return V(~s.x)\n    def __pos__(s): return V(+s.x)\nx = ((-V(5)).x, abs(V(-3)).x, (~V(4)).x, (+V(7)).x)", "x"),
        "(-5, 3, -5, 7)"
    );
}

#[test]
fn iteration_protocol() {
    // __getitem__ sequence-protocol iteration.
    assert_eq!(
        g("class S:\n    def __init__(s): s.d = [10, 20, 30]\n    def __getitem__(s, i):\n        if i >= len(s.d): raise IndexError\n        return s.d[i]\nx = [list(S()), 20 in S(), 99 in S()]", "x"),
        "[[10, 20, 30], True, False]"
    );
    // __contains__ overrides iteration.
    assert_eq!(
        g(
            "class C:\n    def __contains__(s, x): return x == 42\nx = (42 in C(), 1 in C())",
            "x"
        ),
        "(True, False)"
    );
    // __reversed__.
    assert_eq!(
        g(
            "class C:\n    def __reversed__(s): return iter([3, 2, 1])\nx = list(reversed(C()))",
            "x"
        ),
        "[3, 2, 1]"
    );
}

#[test]
fn new_dunder() {
    // __new__ creates the instance and __init__ receives the same args.
    assert_eq!(
        g("class C:\n    def __new__(cls, x): return object.__new__(cls)\n    def __init__(self, x): self.x = x * 2\nx = C(7).x", "x"),
        "14"
    );
    // __new__ returning a foreign object skips __init__.
    assert_eq!(
        g("class C:\n    def __new__(cls): return 99\n    def __init__(self): self.bad = True\nx = C()", "x"),
        "99"
    );
}

#[test]
fn bool_len_dunder_dispatch() {
    // bool()/any/all honor __bool__ then __len__ on instances.
    assert_eq!(
        g("class C:\n    def __init__(s, n): s.n = n\n    def __len__(s): return s.n\nx = (bool(C(0)), bool(C(3)), any([C(0), C(2)]), all([C(1), C(0)]))", "x"),
        "(False, True, True, False)"
    );
    assert_eq!(
        g(
            "class B:\n    def __bool__(s): return False\nx = bool(B())",
            "x"
        ),
        "False"
    );
}

#[test]
fn bare_reraise_in_handler() {
    // A bare `raise` in an except handler re-raises the active exception, caught
    // by an outer handler.
    assert_eq!(
        g(
            "def f():\n    try:\n        raise ValueError('boom')\n    except ValueError:\n        raise\nx = 'unset'\ntry:\n    f()\nexcept ValueError as e:\n    x = str(e)",
            "x"
        ),
        "'boom'"
    );
}

#[test]
fn instance_and_class_introspection() {
    // Instance __class__ / __dict__ and vars().
    assert_eq!(
        g("class C:\n    def __init__(s): s.a = 1; s.b = 2\nc = C()\nx = (c.__class__.__name__, c.__dict__, vars(c))", "x"),
        "('C', {'a': 1, 'b': 2}, {'a': 1, 'b': 2})"
    );
    // Class __bases__ / __mro__ names.
    assert_eq!(
        g("class A: pass\nclass B(A): pass\nx = ([b.__name__ for b in B.__bases__], [c.__name__ for c in B.__mro__])", "x"),
        "(['A'], ['B', 'A', 'object'])"
    );
    // User class repr carries the __main__ module qualifier (builtins don't).
    assert_eq!(
        g("class Widget: pass\nx = repr(Widget)", "x"),
        "\"<class '__main__.Widget'>\""
    );
}

#[test]
fn generator_send_throw_close() {
    // .send() feeds a value into the yield expression.
    assert_eq!(
        g("def acc():\n    t = 0\n    while True:\n        x = yield t\n        t += x\na = acc()\nnext(a)\ny1 = a.send(5)\ny2 = a.send(10)\nx = (y1, y2)", "x"),
        "(5, 15)"
    );
    // .throw() raises at the suspended yield; a handler can resume.
    assert_eq!(
        g("def g():\n    try:\n        yield 1\n    except ValueError:\n        yield 99\ngen = g()\nnext(gen)\nx = gen.throw(ValueError())", "x"),
        "99"
    );
    // .close() runs finally and stops the generator.
    assert_eq!(
        g("log = []\ndef g():\n    try:\n        yield 1\n    finally:\n        log.append('closed')\ngen = g()\nnext(gen)\ngen.close()\nx = log", "x"),
        "['closed']"
    );
}

#[test]
fn generator_return_value() {
    // StopIteration carries the generator's return value.
    assert_eq!(
        g("def g():\n    yield 1\n    return 42\ngen = g()\nnext(gen)\nval = None\ntry:\n    next(gen)\nexcept StopIteration as e:\n    val = e.value\nx = val", "x"),
        "42"
    );
    // `yield from` evaluates to the delegated generator's return value.
    assert_eq!(
        g("def sub():\n    yield 1\n    yield 2\n    return 99\ndef main():\n    r = yield from sub()\n    yield r\nx = list(main())", "x"),
        "[1, 2, 99]"
    );
}

#[test]
fn keyword_only_defaults() {
    // A keyword-only param with a default may be omitted.
    assert_eq!(
        g(
            "def f(a, *, c, d=4): return a + c + d\nx = (f(1, c=3), f(1, c=3, d=10))",
            "x"
        ),
        "(8, 14)"
    );
    // All-optional keyword-only.
    assert_eq!(g("def f(a, *, c=10): return a + c\nx = f(1)", "x"), "11");
    // Lambda keyword-only default.
    assert_eq!(
        g("h = lambda a, *, b=2: a * b\nx = (h(5), h(5, b=3))", "x"),
        "(10, 15)"
    );
    // Mixed positional + keyword-only defaults.
    assert_eq!(
        g(
            "def f(a=1, b=2, *, c=3, d=4): return (a, b, c, d)\nx = (f(), f(10, c=30))",
            "x"
        ),
        "((1, 2, 3, 4), (10, 2, 30, 4))"
    );
}

#[test]
fn zero_to_negative_power_raises() {
    // `0 ** <negative>` is a ZeroDivisionError, not `inf`.
    assert_eq!(
        g(
            "x = 'unset'\ntry:\n    0 ** -1\nexcept ZeroDivisionError:\n    x = 'zde'",
            "x"
        ),
        "'zde'"
    );
    // Non-zero base still works.
    assert_eq!(g("x = 2 ** -1", "x"), "0.5");
}

#[test]
fn slots_enforcement() {
    // A fully-slotted instance rejects undeclared attributes.
    assert_eq!(
        g("class P:\n    __slots__ = ('x', 'y')\n    def __init__(s): s.x = 1; s.y = 2\np = P()\nres = 'unset'\ntry:\n    p.z = 3\nexcept AttributeError:\n    res = 'blocked'\nx = (p.x, p.y, res)", "x"),
        "(1, 2, 'blocked')"
    );
    // A non-slotted base restores the instance __dict__ (slots don't restrict).
    assert_eq!(
        g("class B: pass\nclass D(B):\n    __slots__ = ('a',)\nd = D()\nd.a = 1\nd.b = 2\nx = (d.a, d.b)", "x"),
        "(1, 2)"
    );
}

#[test]
fn complex_arithmetic() {
    assert_eq!(g("x = (1+2j) + (3+4j)", "x"), "(4+6j)");
    assert_eq!(g("x = (1+2j) * (3+4j)", "x"), "(-5+10j)");
    assert_eq!(g("x = (1+2j) - (3+4j)", "x"), "(-2-2j)");
    assert_eq!(g("x = complex(1, 2)", "x"), "(1+2j)");
    assert_eq!(g("x = complex('1+2j')", "x"), "(1+2j)");
    assert_eq!(g("x = complex('-2j')", "x"), "-2j");
    assert_eq!(g("x = abs(3+4j)", "x"), "5.0");
    assert_eq!(g("x = (2+3j).conjugate()", "x"), "(2-3j)");
    assert_eq!(g("x = ((2+3j).real, (2+3j).imag)", "x"), "(2.0, 3.0)");
    assert_eq!(g("x = (2+3j) ** 2", "x"), "(-5+12j)");
    assert_eq!(g("x = 2j ** 2", "x"), "(-4+0j)");
    // A negative real base to a fractional power yields a complex root.
    assert_eq!(
        g("x = (-8) ** (1/3)", "x"),
        "(1.0000000000000002+1.7320508075688772j)"
    );
    assert_eq!(g("x = (1+2j) == (1+2j)", "x"), "True");
    assert_eq!(g("x = bool(0j)", "x"), "False");
    // A zero-imaginary complex keys the same slot as the equal real number.
    assert_eq!(g("x = complex(1, 0) in {1}", "x"), "True");
}

#[test]
fn exception_chaining() {
    // `raise X from Y` sets __cause__ (and __suppress_context__).
    assert_eq!(
        g(
            "try:\n    try:\n        raise ValueError('inner')\n    except ValueError as e:\n        raise TypeError('outer') from e\nexcept TypeError as t:\n    x = type(t.__cause__).__name__",
            "x"
        ),
        "'ValueError'"
    );
    assert_eq!(
        g(
            "try:\n    try:\n        raise ValueError('inner')\n    except ValueError as e:\n        raise TypeError('outer') from e\nexcept TypeError as t:\n    x = t.__suppress_context__",
            "x"
        ),
        "True"
    );
    // Implicit __context__ during handling; no explicit cause.
    assert_eq!(
        g(
            "try:\n    try:\n        raise ValueError('v')\n    except ValueError:\n        raise TypeError('t')\nexcept TypeError as t:\n    x = (type(t.__context__).__name__, t.__cause__)",
            "x"
        ),
        "('ValueError', None)"
    );
    // User exception class carries a chain via the side table.
    assert_eq!(
        g(
            "class E(Exception): pass\ntry:\n    raise E('x') from ValueError('c')\nexcept E as e:\n    x = type(e.__cause__).__name__",
            "x"
        ),
        "'ValueError'"
    );
}

#[test]
fn lazy_iterators() {
    // zip/map/filter/enumerate are lazy iterator objects, not eager lists.
    assert_eq!(g("x = type(zip([1],[2])).__name__", "x"), "'zip'");
    assert_eq!(g("x = type(map(str,[1])).__name__", "x"), "'map'");
    assert_eq!(g("x = type(filter(None,[1])).__name__", "x"), "'filter'");
    assert_eq!(g("x = type(enumerate([1])).__name__", "x"), "'enumerate'");
    // next() drives them; they exhaust once.
    assert_eq!(
        g(
            "z = zip([1,2],[3,4])\nx = (next(z), list(z), next(z, 'end'))",
            "x"
        ),
        "((1, 3), [(2, 4)], 'end')"
    );
    assert_eq!(g("x = list(map(lambda a: a*2, [1,2,3]))", "x"), "[2, 4, 6]");
    assert_eq!(
        g("x = list(filter(lambda a: a % 2, range(10)))", "x"),
        "[1, 3, 5, 7, 9]"
    );
    assert_eq!(
        g("x = list(enumerate('ab', start=5))", "x"),
        "[(5, 'a'), (6, 'b')]"
    );
    // reversed is a one-shot iterator, not a list.
    assert_eq!(
        g("r = reversed([1,2,3])\nx = (next(r), list(r))", "x"),
        "(3, [2, 1])"
    );
    // Infinite source never materializes (would hang if eager).
    assert_eq!(
        g(
            "def c():\n    i=0\n    while True:\n        yield i\n        i+=1\nx = list(zip(c(), ['a','b','c']))",
            "x"
        ),
        "[(0, 'a'), (1, 'b'), (2, 'c')]"
    );
}

#[test]
fn frozenset_type() {
    assert_eq!(g("x = frozenset([1,2,2])", "x"), "frozenset({1, 2})");
    assert_eq!(g("x = frozenset()", "x"), "frozenset()");
    assert_eq!(g("x = type(frozenset()).__name__", "x"), "'frozenset'");
    // Hashable: usable as a dict key and a set member.
    assert_eq!(
        g("d = {frozenset([1,2]): 'a'}\nx = d[frozenset([2,1])]", "x"),
        "'a'"
    );
    assert_eq!(
        g(
            "x = len({frozenset([1,2]), frozenset([2,1]), frozenset([3])})",
            "x"
        ),
        "2"
    );
    // Set algebra: result type follows the left operand.
    assert_eq!(
        g("x = type(frozenset([1,2]) | {3}).__name__", "x"),
        "'frozenset'"
    );
    assert_eq!(g("x = type({1,2} | frozenset([3])).__name__", "x"), "'set'");
    assert_eq!(
        g("x = frozenset([1,2,3]) & frozenset([2,3,4])", "x"),
        "frozenset({2, 3})"
    );
    // isinstance: frozenset is not a set and vice versa.
    assert_eq!(
        g("x = (isinstance(frozenset(), frozenset), isinstance(frozenset(), set), isinstance({1}, frozenset))", "x"),
        "(True, False, False)"
    );
    // set == frozenset by membership.
    assert_eq!(g("x = frozenset([1,2]) == {1,2}", "x"), "True");
}

#[test]
fn set_ops_and_comparisons() {
    // Subset partial-order operators.
    assert_eq!(
        g("x = ({1,2} <= {1,2,3}, {1,2} < {1,2})", "x"),
        "(True, False)"
    );
    assert_eq!(
        g("x = ({1,2} < {3,4}, {1,2} > {3,4})", "x"),
        "(False, False)"
    );
    assert_eq!(g("x = {1,2,3} > {1,2}", "x"), "True");
    // isdisjoint and the *_update mutators (accept any iterable).
    assert_eq!(g("x = {1,2}.isdisjoint([3,4])", "x"), "True");
    assert_eq!(g("x = {1,2}.isdisjoint([2,3])", "x"), "False");
    assert_eq!(
        g("s = {1,2,3}\ns.intersection_update([2,3,4])\nx = s", "x"),
        "{2, 3}"
    );
    assert_eq!(
        g("s = {1,2,3}\ns.difference_update([2])\nx = s", "x"),
        "{1, 3}"
    );
    assert_eq!(
        g(
            "s = {1,2,3}\ns.symmetric_difference_update([3,4])\nx = s",
            "x"
        ),
        "{1, 2, 4}"
    );
    assert_eq!(g("x = {1,2,3}.issubset([1,2,3,4])", "x"), "True");
}

#[test]
fn dict_views_and_merge() {
    // Views are live view objects, not list snapshots.
    assert_eq!(
        g("d = {1:2,3:4}\nx = type(d.keys()).__name__", "x"),
        "'dict_keys'"
    );
    assert_eq!(g("d = {1:2,3:4}\nx = d.keys()", "x"), "dict_keys([1, 3])");
    assert_eq!(
        g("d = {1:2,3:4}\nx = d.items()", "x"),
        "dict_items([(1, 2), (3, 4)])"
    );
    // Live update: a view reflects later mutation.
    assert_eq!(
        g("d = {1:2}\nk = d.keys()\nd[3] = 4\nx = sorted(k)", "x"),
        "[1, 3]"
    );
    // View set-ops return a set.
    assert_eq!(g("d = {1:2}\nx = d.keys() | {3}", "x"), "{1, 3}");
    assert_eq!(g("d = {1:2,3:4}\nx = d.items() & {(1,2)}", "x"), "{(1, 2)}");
    // fromkeys, dict merge, update variants.
    assert_eq!(
        g("x = dict.fromkeys([1,2,3])", "x"),
        "{1: None, 2: None, 3: None}"
    );
    assert_eq!(g("x = dict.fromkeys([1,2], 0)", "x"), "{1: 0, 2: 0}");
    assert_eq!(g("x = {1:2} | {3:4}", "x"), "{1: 2, 3: 4}");
    assert_eq!(g("d = {1:2}\nd |= {3:4}\nx = d", "x"), "{1: 2, 3: 4}");
    assert_eq!(
        g("d = {}\nd.update(a=1, b=2)\nx = d", "x"),
        "{'a': 1, 'b': 2}"
    );
    assert_eq!(
        g("d = {}\nd.update([(1,2),(3,4)])\nx = d", "x"),
        "{1: 2, 3: 4}"
    );
}

#[test]
fn range_methods_and_equality() {
    assert_eq!(g("x = range(10)[2:8:2]", "x"), "range(2, 8, 2)");
    assert_eq!(g("x = list(range(10)[2:8:2])", "x"), "[2, 4, 6]");
    assert_eq!(g("x = range(10)[::-1]", "x"), "range(9, -1, -1)");
    assert_eq!(g("x = range(10).index(4)", "x"), "4");
    assert_eq!(g("x = range(0,20,2).index(6)", "x"), "3");
    assert_eq!(
        g("x = (range(10).count(4), range(10).count(99))", "x"),
        "(1, 0)"
    );
    assert_eq!(g("x = range(10) == range(0, 10)", "x"), "True");
    assert_eq!(g("x = range(0) == range(5, 5)", "x"), "True");
    assert_eq!(g("x = range(0,10,2) == range(0,11,2)", "x"), "False");
    assert_eq!(g("x = range(0,10,2) == range(0,9,2)", "x"), "True");
}

#[test]
fn slice_assignment_and_del() {
    assert_eq!(g("x = [1,2,3,4,5]\nx[1:3] = [9]\n", "x"), "[1, 9, 4, 5]");
    assert_eq!(
        g("x = [1,2,3,4,5]\nx[1:1] = [8,9]\n", "x"),
        "[1, 8, 9, 2, 3, 4, 5]"
    );
    assert_eq!(
        g("x = [1,2,3,4,5,6]\nx[::2] = [7,8,9]\n", "x"),
        "[7, 2, 8, 4, 9, 6]"
    );
    assert_eq!(g("x = [1,2,3]\nx[:] = [9,9,9,9]\n", "x"), "[9, 9, 9, 9]");
    assert_eq!(g("x = [1,2,3,4,5]\nx[1:4] = []\n", "x"), "[1, 5]");
    assert_eq!(g("x = [1,2,3,4,5]\ndel x[1:3]\n", "x"), "[1, 4, 5]");
    assert_eq!(g("x = [1,2,3,4,5,6]\ndel x[::2]\n", "x"), "[2, 4, 6]");
    // A generator RHS is materialized without a borrow panic.
    assert_eq!(
        g("x = [1,2,3]\nx[1:2] = (i for i in [7,8])\n", "x"),
        "[1, 7, 8, 3]"
    );
}

#[test]
fn str_methods_tier5() {
    assert_eq!(g("x = 'a.b.c'.partition('.')", "x"), "('a', '.', 'b.c')");
    assert_eq!(g("x = 'a.b.c'.rpartition('.')", "x"), "('a.b', '.', 'c')");
    assert_eq!(g("x = 'x'.partition('.')", "x"), "('x', '', '')");
    assert_eq!(g("x = 'abcb'.rindex('b')", "x"), "3");
    assert_eq!(
        g("x = ('123'.isnumeric(), 'abc'.isnumeric())", "x"),
        "(True, False)"
    );
    assert_eq!(
        g("x = ('1'.isdecimal(), '\u{00bd}'.isdecimal())", "x"),
        "(True, False)"
    );
    assert_eq!(
        g("x = ('Hello World'.istitle(), 'hello'.istitle())", "x"),
        "(True, False)"
    );
    assert_eq!(
        g("x = ('abc'.isidentifier(), '1a'.isidentifier())", "x"),
        "(True, False)"
    );
    assert_eq!(g("x = 'a\\tbc'.expandtabs(4)", "x"), "'a   bc'");
    assert_eq!(g("x = 'abc'.translate({97:98})", "x"), "'bbc'");
    assert_eq!(
        g("x = 'hello'.translate(str.maketrans('lo','LO'))", "x"),
        "'heLLO'"
    );
    assert_eq!(g("x = str.maketrans('ab','xy')", "x"), "{97: 120, 98: 121}");
    assert_eq!(g("x = '{a:.2f}'.format_map({'a':3.14159})", "x"), "'3.14'");
}

#[test]
fn repr_escaping_and_ascii_and_octal() {
    // repr escapes C0 controls; ascii escapes non-ASCII. `g` reprs the string
    // global, so these are the double-repr forms python3 also produces.
    assert_eq!(g(r#"x = repr("a\x00b\x1f")"#, "x"), r#""'a\\x00b\\x1f'""#);
    assert_eq!(g("x = ascii('caf\u{00e9}')", "x"), r#""'caf\\xe9'""#);
    // Octal string escape.
    assert_eq!(g(r#"x = "\101\102\103""#, "x"), "'ABC'");
    // Printable Unicode is kept verbatim in repr.
    assert_eq!(g("x = repr('\u{00e9}')", "x"), "\"'\u{00e9}'\"");
}

#[test]
fn three_arg_type_and_posonly() {
    // Dynamic class creation via 3-arg type().
    assert_eq!(
        g("C = type('C', (), {'x': 5})\nx = (C.x, C.__name__)", "x"),
        "(5, 'C')"
    );
    assert_eq!(
        g(
            "C = type('C', (), {'m': lambda self: 42})\nx = C().m()",
            "x"
        ),
        "42"
    );
    assert_eq!(
        g(
            "class B:\n    def f(self): return 7\nD = type('D', (B,), {})\nx = D().f()",
            "x"
        ),
        "7"
    );
    // Positional-only enforcement.
    assert_eq!(
        g("def f(a, b, /, c): return a+b+c\nx = f(1, 2, c=3)", "x"),
        "6"
    );
    assert_eq!(
        g("def f(a, /, **kw): return (a, kw)\nx = f(1, a=2)", "x"),
        "(1, {'a': 2})"
    );
    assert_eq!(
        g(
            "def f(a, b, /): return a+b\ntry:\n    f(a=1, b=2)\nexcept TypeError:\n    x = 'rejected'",
            "x"
        ),
        "'rejected'"
    );
}

#[test]
fn named_unicode_escapes() {
    // \N{NAME} resolves to the codepoint, in normal strings and f-strings.
    // Expected values match CPython 3.14 byte for byte.
    assert_eq!(g("x = '\\N{LATIN SMALL LETTER E WITH ACUTE}'", "x"), "'é'");
    assert_eq!(
        g("x = '\\N{GREEK SMALL LETTER ALPHA}\\N{BULLET}'", "x"),
        "'α•'"
    );
    assert_eq!(g("x = len('\\N{ROCKET}')", "x"), "1");
    assert_eq!(g("x = ord('\\N{SNOWMAN}')", "x"), "9731");
    // Case-insensitive name matching (CPython accepts lowercase).
    assert_eq!(g("x = '\\N{bullet}'", "x"), "'•'");
    // f-string: the escape's braces are not a replacement field.
    assert_eq!(g("x = f'a\\N{BULLET}b {1+1}'", "x"), "'a•b 2'");
    assert_eq!(g("x = f'\\N{ROCKET}{7}'", "x"), "'🚀7'");
    // An escaped backslash means \N is literal, not an escape.
    assert_eq!(g("x = '\\\\N{BULLET}'", "x"), "'\\\\N{BULLET}'");
}

#[test]
fn named_unicode_escape_errors() {
    // Unknown name (CPython's exact unicodeescape error, byte-identical payload).
    let e = eval_str("x = '\\N{NOT A REAL NAME}'").unwrap_err();
    assert!(
        e.contains(
            "(unicode error) 'unicodeescape' codec can't decode bytes in position 0-18: unknown Unicode character name"
        ),
        "got: {e}"
    );
    // Position offset accounts for a leading char.
    let e = eval_str("x = 'x\\N{BOGUS NAME HERE}'").unwrap_err();
    assert!(
        e.contains("position 1-19: unknown Unicode character name"),
        "got: {e}"
    );
    // Empty braces -> malformed.
    let e = eval_str("x = '\\N{}'").unwrap_err();
    assert!(
        e.contains("position 0-2: malformed \\N character escape"),
        "got: {e}"
    );
    // Missing brace -> malformed.
    let e = eval_str("x = '\\Nfoo'").unwrap_err();
    assert!(
        e.contains("position 0-1: malformed \\N character escape"),
        "got: {e}"
    );
    // Unterminated brace -> malformed, spans to end of literal.
    let e = eval_str("x = '\\N{FOO'").unwrap_err();
    assert!(
        e.contains("position 0-5: malformed \\N character escape"),
        "got: {e}"
    );
    // CPython matches case-insensitively but NOT loosely: stray whitespace or
    // underscore-for-space must fail.
    //
    // A bare `is_err()` here passed on ANY failure, including one whose message
    // CPython never produced — and pythonrs emitted this diagnostic with no
    // exception name at all (`(unicode error) …` where CPython prints
    // `SyntaxError: (unicode error) …`), which `is_err()` could not see. Every
    // expectation below is byte-checked against `python3 -c` on 3.14.6.
    for (src, want) in [
        (
            "x = '\\N{ SPACE}'",
            "SyntaxError: (unicode error) 'unicodeescape' codec can't decode bytes \
             in position 0-9: unknown Unicode character name",
        ),
        (
            "x = '\\N{GREEK_SMALL_LETTER_ALPHA}'",
            "SyntaxError: (unicode error) 'unicodeescape' codec can't decode bytes \
             in position 0-27: unknown Unicode character name",
        ),
        // f-string unknown name also errors.
        (
            "x = f'\\N{NOPE}'",
            "SyntaxError: (unicode error) 'unicodeescape' codec can't decode bytes \
             in position 0-7: unknown Unicode character name",
        ),
    ] {
        assert_eq!(
            eval_str(src).expect_err("must be rejected"),
            want,
            "for {src}"
        );
    }
}

/// `\x`/`\u`/`\U` take a FIXED number of hex digits. Too few is a `SyntaxError`,
/// not a shorter escape — pythonrs read whatever digits were present and silently
/// produced a different string (`'\x2'` became `'\x02'`, `'\xzz'` became `'zz'`).
/// Bytes literals reject the same defect with `PyBytes_DecodeEscape`'s different
/// wording, and do not treat `\u`/`\U`/`\N` as escapes at all.
///
/// Every expectation is byte-checked against `python3 -c 'print(repr(<lit>))'`
/// on CPython 3.14.6.
#[test]
fn fixed_width_hex_escapes_reject_a_short_digit_run() {
    const UNI: &str = "SyntaxError: (unicode error) 'unicodeescape' codec can't decode bytes";
    for (src, want) in [
        (
            "x = '\\x2'",
            format!("{UNI} in position 0-2: truncated \\xXX escape"),
        ),
        (
            "x = '\\x'",
            format!("{UNI} in position 0-1: truncated \\xXX escape"),
        ),
        // A non-hex character ends the digit run where it stands.
        (
            "x = '\\xzz'",
            format!("{UNI} in position 0-1: truncated \\xXX escape"),
        ),
        (
            "x = '\\u12'",
            format!("{UNI} in position 0-3: truncated \\uXXXX escape"),
        ),
        (
            "x = '\\uzzzz'",
            format!("{UNI} in position 0-1: truncated \\uXXXX escape"),
        ),
        (
            "x = '\\U0001'",
            format!("{UNI} in position 0-5: truncated \\UXXXXXXXX escape"),
        ),
        // Past U+10FFFF: the digits are all there, the code point is not legal.
        (
            "x = '\\U00110000'",
            format!("{UNI} in position 0-9: illegal Unicode character"),
        ),
        (
            "x = '\\UFFFFFFFF'",
            format!("{UNI} in position 0-9: illegal Unicode character"),
        ),
        (
            "x = b'\\x2'",
            "SyntaxError: (value error) invalid \\x escape at position 0".to_string(),
        ),
        (
            "x = b'\\xzz'",
            "SyntaxError: (value error) invalid \\x escape at position 0".to_string(),
        ),
    ] {
        assert_eq!(
            eval_str(src).expect_err("must be rejected"),
            want,
            "for {src}"
        );
    }
    // …and the well-formed forms still decode, pinned by VALUE so a fix that
    // rejected everything would fail here.
    assert_eq!(g("x = '\\xff'", "x"), "'\u{ff}'");
    assert_eq!(g("x = '\\u00e9'", "x"), "'\u{e9}'");
    assert_eq!(g("x = repr('\\U0001F600')", "x"), "\"'\u{1f600}'\"");
    assert_eq!(g("x = b'\\xff'", "x"), "b'\\xff'");
    assert_eq!(g("x = b'a\\x2b'", "x"), "b'a+'");
    assert_eq!(g("x = r'\\x2'", "x"), "'\\\\x2'");
    // In a bytes literal `\u`/`\U`/`\N` are two literal characters, not escapes:
    // pythonrs decoded them and turned six bytes into one.
    assert_eq!(g("x = b'\\u1234'", "x"), "b'\\\\u1234'");
    assert_eq!(g("x = b'\\N{BULLET}'", "x"), "b'\\\\N{BULLET}'");
    assert_eq!(g("x = b'\\U0001F600'", "x"), "b'\\\\U0001F600'");
}

#[test]
fn decode_escapes_named_unicode_unit() {
    use pythonrs::lexer::decode_escapes;
    assert_eq!(decode_escapes("\\N{BULLET}", false).unwrap(), "•");
    assert_eq!(
        decode_escapes("\\N{LATIN SMALL LETTER E WITH ACUTE}", false).unwrap(),
        "é"
    );
    // Raw strings keep the escape literal.
    assert_eq!(decode_escapes("\\N{BULLET}", true).unwrap(), "\\N{BULLET}");
    // Pinned to the message, not merely to "errored": a bare `is_err()` cannot
    // tell a correct rejection from a rejection for the wrong reason.
    assert_eq!(
        decode_escapes("\\N{ SPACE}", false).expect_err("loose name must be rejected"),
        "SyntaxError: (unicode error) 'unicodeescape' codec can't decode bytes \
         in position 0-9: unknown Unicode character name"
    );
}

#[test]
fn set_repr_cpython_hash_order() {
    // A set/frozenset of machine ints iterates and reprs in CPython's
    // open-addressing table order, not insertion order. `set(iterable)` builds
    // incrementally, exactly as pythonrs does, so these match byte-for-byte.
    assert_eq!(g("x = set([3, 1, 2])", "x"), "{1, 2, 3}");
    assert_eq!(g("x = set([10, 5, 1, 2, 3])", "x"), "{1, 2, 3, 5, 10}");
    assert_eq!(g("x = set([-1, -5, 3])", "x"), "{3, -5, -1}");
    assert_eq!(g("x = set([100, 1, 50])", "x"), "{1, 50, 100}");
    assert_eq!(g("x = frozenset([3, 1, 2])", "x"), "frozenset({1, 2, 3})");
    // Colliding ints beyond the initial table (drives a resize + linear probing).
    assert_eq!(g("x = set([9, 1, 17, 25, 33])", "x"), "{33, 1, 9, 17, 25}");
    // Iteration follows the same order.
    assert_eq!(
        g("x = list(set([10, 5, 1, 2, 3]))", "x"),
        "[1, 2, 3, 5, 10]"
    );
    // `1`, `1.0`, `True` unify to one element (int key), repr uses the first.
    assert_eq!(g("x = set([2.0, 1])", "x"), "{1, 2.0}");
}

#[test]
fn metaclasses() {
    // `class A(metaclass=M)` runs `M.__new__`/`M.__init__`; `type(A) is M`.
    let base = "class M(type):\n    def __new__(mcls, name, bases, ns):\n        ns['injected'] = 99\n        return super().__new__(mcls, name, bases, ns)\n    def __init__(cls, name, bases, ns):\n        cls.tag = name.lower()\n        super().__init__(name, bases, ns)\nclass A(metaclass=M):\n    pass\n";
    assert_eq!(
        g(&format!("{base}x = (A.injected, A.tag, type(A) is M)"), "x"),
        "(99, 'a', True)"
    );
    // A subclass inherits the metaclass (no explicit `metaclass=`).
    assert_eq!(
        g(
            &format!("{base}class B(A): pass\nx = (type(B) is M, B.injected)"),
            "x"
        ),
        "(True, 99)"
    );
    // A metaclass method is callable on the class, bound to the class.
    assert_eq!(
        g("class M(type):\n    def kind(cls): return cls.__name__ + '!'\nclass A(metaclass=M): pass\nx = A.kind()", "x"),
        "'A!'"
    );
    // Metaclass `__call__` controls instantiation (singleton pattern).
    let singleton = "class S(type):\n    _i = {}\n    def __call__(cls, *a):\n        if cls not in cls._i:\n            cls._i[cls] = super().__call__(*a)\n        return cls._i[cls]\nclass DB(metaclass=S):\n    def __init__(self): self.v = 7\n";
    assert_eq!(
        g(
            &format!("{singleton}a = DB()\nb = DB()\nx = (a is b, a.v)"),
            "x"
        ),
        "(True, 7)"
    );
    // 3-arg `type(name, bases, ns)` builds an ordinary class (`type` metaclass).
    assert_eq!(
        g(
            "D = type('D', (), {'v': 5})\nx = (D.v, type(D) is type)",
            "x"
        ),
        "(5, True)"
    );
    // A class object is usable as a dict key (identity by name).
    assert_eq!(g("x = {int: 'i', str: 's'}[int]", "x"), "'i'");
}

#[test]
fn instance_hash_dict_set_keys() {
    // A class with `__hash__` + `__eq__` gives value-equal instances one dict/set
    // slot; lookups with an equal-but-distinct instance find the entry.
    const C: &str = "class C:\n    def __init__(s, v): s.v = v\n    def __hash__(s): return s.v\n    def __eq__(s, o): return isinstance(o, C) and s.v == o.v\n";
    assert_eq!(
        g(
            &format!("{C}d = {{C(1): 'a', C(2): 'b'}}\nx = d[C(1)]"),
            "x"
        ),
        "'a'"
    );
    // Value-equal keys collapse; a re-store updates in place.
    assert_eq!(
        g(
            &format!("{C}d = {{C(1): 'a'}}\nd[C(1)] = 'z'\nx = (len(d), d[C(1)])"),
            "x"
        ),
        "(1, 'z')"
    );
    // Set membership + dedup of equal instances.
    assert_eq!(
        g(
            &format!("{C}s = {{C(1), C(2), C(1)}}\nx = (len(s), C(1) in s, C(9) in s)"),
            "x"
        ),
        "(2, True, False)"
    );
    // `hash()` returns the `__hash__` result verbatim.
    assert_eq!(g(&format!("{C}x = hash(C(42))"), "x"), "42");
    // A bare class (no `__hash__`/`__eq__`) is hashable by identity.
    assert_eq!(
        g(
            "class B: pass\nb = B()\nd = {b: 1}\nx = (d[b], B() in d)",
            "x"
        ),
        "(1, False)"
    );
    // `__eq__` without `__hash__` (and `__hash__ = None`) makes it unhashable.
    for body in ["def __eq__(s, o): return True", "__hash__ = None"] {
        let src = format!("class U:\n    {body}\ntry:\n    _ = {{U()}}\n    x = 'hashable'\nexcept TypeError:\n    x = 'unhashable'");
        assert_eq!(g(&src, "x"), "'unhashable'");
    }
}

#[test]
fn set_algebra_between_independent_user_hashed_sets() {
    // Two sets built separately hold DIFFERENT objects for the same value, and a
    // user-`__hash__` key carries the heap id it collapsed onto — so the algebra
    // has to re-key one operand against the other instead of comparing the stored
    // keys. Every expectation below is the value CPython 3.14 prints.
    const C: &str = "class P:\n    def __init__(s, v): s.v = v\n    def __hash__(s): return hash(s.v)\n    def __eq__(s, o): return isinstance(o, P) and s.v == o.v\na = {P(1), P(2)}\nb = {P(2), P(3)}\nv = lambda s: sorted(e.v for e in s)\n";
    assert_eq!(g(&format!("{C}x = v(a & b)"), "x"), "[2]");
    assert_eq!(g(&format!("{C}x = v(a | b)"), "x"), "[1, 2, 3]");
    assert_eq!(g(&format!("{C}x = v(a - b)"), "x"), "[1]");
    assert_eq!(g(&format!("{C}x = v(a ^ b)"), "x"), "[1, 3]");
    // The method spellings take the same path as the operators.
    assert_eq!(g(&format!("{C}x = v(a.intersection(b))"), "x"), "[2]");
    assert_eq!(g(&format!("{C}x = v(a.difference(b))"), "x"), "[1]");
    assert_eq!(
        g(&format!("{C}x = v(a.symmetric_difference(b))"), "x"),
        "[1, 3]"
    );
    // The subset orders, `==`, and `isdisjoint` compare keys the same way.
    assert_eq!(g(&format!("{C}x = {{P(1)}} == {{P(1)}}"), "x"), "True");
    assert_eq!(
        g(&format!("{C}x = ({{P(1)}} <= a, a >= {{P(1)}})"), "x"),
        "(True, True)"
    );
    assert_eq!(
        g(&format!("{C}x = ({{P(1)}} < a, a < a)"), "x"),
        "(True, False)"
    );
    assert_eq!(
        g(&format!("{C}x = (a.issubset(a), a.isdisjoint(b))"), "x"),
        "(True, False)"
    );
    // A dict keyed the same way compares by value too.
    assert_eq!(
        g(&format!("{C}x = {{P(1): 'a'}} == {{P(1): 'a'}}"), "x"),
        "True"
    );
    // In-place forms mutate the receiver without duplicating the shared element.
    assert_eq!(
        g(&format!("{C}s = {{P(1), P(2)}}\ns &= b\nx = v(s)"), "x"),
        "[2]"
    );
    assert_eq!(
        g(&format!("{C}s = {{P(1), P(2)}}\ns -= b\nx = v(s)"), "x"),
        "[1]"
    );
    assert_eq!(
        g(&format!("{C}s = {{P(1)}}\ns |= b\nx = v(s)"), "x"),
        "[1, 2, 3]"
    );
    assert_eq!(
        g(&format!("{C}s = {{P(1), P(2)}}\ns ^= b\nx = v(s)"), "x"),
        "[1, 3]"
    );
    // `update` / `symmetric_difference_update` raised `unhashable type: 'P'`
    // before, because they hashed the element inside the host borrow.
    assert_eq!(
        g(
            &format!("{C}s = {{P(1)}}\ns.update({{P(1), P(4)}})\nx = v(s)"),
            "x"
        ),
        "[1, 4]"
    );
    assert_eq!(
        g(
            &format!(
                "{C}s = {{P(1), P(2)}}\ns.symmetric_difference_update({{P(2), P(3)}})\nx = v(s)"
            ),
            "x"
        ),
        "[1, 3]"
    );
    assert_eq!(
        g(
            &format!("{C}s = {{P(1), P(2)}}\ns.intersection_update({{P(2)}})\nx = v(s)"),
            "x"
        ),
        "[2]"
    );
    assert_eq!(
        g(
            &format!("{C}s = {{P(1), P(2)}}\ns.difference_update({{P(2)}})\nx = v(s)"),
            "x"
        ),
        "[1]"
    );
    // A default (identity-hashed) instance must NOT collapse: two `B()`s are
    // distinct members, so the alignment pass cannot over-merge.
    assert_eq!(
        g(
            "class B: pass\nb1, b2 = B(), B()\nx = (len({b1, b2} & {b1}), len({b1} & {b2}))",
            "x"
        ),
        "(1, 0)"
    );
}

#[test]
fn dict_update_rekeys_value_equal_keys_onto_the_existing_slot() {
    // `dict.update` copied the SOURCE's keys verbatim, and a value key carries
    // the heap id it collapsed onto in its own dict — so a value-equal key added
    // a SECOND slot, giving a dict with two `P(2)` entries, which CPython cannot
    // produce. The pair-iterable form hashed under the host borrow instead and
    // raised `unhashable type: 'P'`. Values are what CPython 3.14 prints.
    const C: &str = "class P:\n    def __init__(s, v): s.v = v\n    def __hash__(s): return hash(s.v)\n    def __eq__(s, o): return isinstance(o, P) and s.v == o.v\nd = lambda m: sorted((k.v, v) for k, v in m.items())\n";
    // Mapping form: the later value wins in the FIRST key's slot.
    assert_eq!(
        g(
            &format!(
                "{C}m = {{P(1): 'a', P(2): 'b'}}\nm.update({{P(2): 'B', P(3): 'C'}})\nx = d(m)"
            ),
            "x"
        ),
        "[(1, 'a'), (2, 'B'), (3, 'C')]"
    );
    // Pair-iterable form, which used to be a TypeError outright.
    assert_eq!(
        g(
            &format!("{C}m = {{P(1): 'a'}}\nm.update([(P(1), 'z'), (P(2), 'q')])\nx = d(m)"),
            "x"
        ),
        "[(1, 'z'), (2, 'q')]"
    );
    // `|=` routes through the same update, and `|` through the operator path.
    assert_eq!(
        g(
            &format!("{C}m = {{P(1): 'a'}}\nm |= {{P(1): 'z'}}\nx = (d(m), len(m))"),
            "x"
        ),
        "([(1, 'z')], 1)"
    );
    // Two value-equal keys WITHIN one update collapse, as in a dict literal.
    assert_eq!(
        g(
            &format!("{C}m = {{}}\nm.update([(P(1), 'a'), (P(1), 'b')])\nx = (d(m), len(m))"),
            "x"
        ),
        "([(1, 'b')], 1)"
    );
    // An identity-hashed instance must still NOT collapse.
    assert_eq!(
        g(
            "class B: pass\nb1, b2 = B(), B()\nm = {b1: 1}\nm.update({b2: 2})\nx = len(m)",
            "x"
        ),
        "2"
    );
}

#[test]
fn user_hash_without_user_eq_falls_back_to_identity() {
    // CPython lets a class define `__hash__` and inherit `object.__eq__`. The
    // key collapse called `__eq__` directly, so any two same-hash keys of such a
    // class raised `AttributeError: 'P' object has no attribute '__eq__'` — the
    // whole dict/set was unusable. Identity equality means they stay distinct.
    const C: &str =
        "class P:\n    def __init__(s, v): s.v = v\n    def __hash__(s): return s.v // 2\n";
    assert_eq!(g(&format!("{C}x = len({{P(5): 1, P(5): 2}})"), "x"), "2");
    assert_eq!(g(&format!("{C}x = len({{P(1), P(1)}})"), "x"), "2");
    assert_eq!(
        g(
            &format!("{C}x = (len({{P(1)}} | {{P(1)}}), len({{P(1)}} & {{P(1)}}))"),
            "x"
        ),
        "(2, 0)"
    );
    // The SAME object still collapses onto its own slot.
    assert_eq!(g(&format!("{C}p = P(1)\nx = len({{p, p}})"), "x"), "1");
    // A builtin-type subclass that adds `__hash__` compares through its payload,
    // so two equal-valued instances DO collapse.
    assert_eq!(
        g(
            "class S(str):\n    def __hash__(s): return 0\nx = len({S('a'), S('a')})",
            "x"
        ),
        "1"
    );
}

#[test]
fn dict_key_and_item_views_are_set_like() {
    // CPython's `dict_keys`/`dict_items` are sets: they take part in the algebra,
    // in `==`, and in the subset order. `==` answered False for every view and
    // the ordering ops raised `'<=' not supported between instances of
    // 'dict_keys' and 'set'`.
    assert_eq!(
        g(
            "d = {1: 0, 2: 0}\nx = (d.keys() == {1, 2}, d.keys() == {1}, {1, 2} == d.keys())",
            "x"
        ),
        "(True, False, True)"
    );
    assert_eq!(
        g("d = {1: 0, 2: 0}\nx = (d.keys() <= {1, 2}, d.keys() < {1, 2, 3}, d.keys() >= {1}, d.keys() > {1})", "x"),
        "(True, True, True, True)"
    );
    assert_eq!(
        g(
            "d = {1: 0, 2: 0}\nx = (d.items() == {(1, 0), (2, 0)}, d.items() <= {(1, 0), (2, 0)})",
            "x"
        ),
        "(True, True)"
    );
    // A `dict_values` view has no set behavior: two views are never equal.
    assert_eq!(g("d = {1: 0}\nx = d.values() == d.values()", "x"), "False");
    // A view is not equal to a list, and a keys view is not an items view.
    assert_eq!(
        g(
            "d = {1: 0}\nx = (d.keys() == [1], d.keys() == d.items())",
            "x"
        ),
        "(False, False)"
    );
    // The same, with value keys: a keys view re-keys like a set, and the old
    // re-hash-under-the-borrow path silently DROPPED every value key, so
    // `d.keys() & s` came back empty.
    const C: &str = "class P:\n    def __init__(s, v): s.v = v\n    def __hash__(s): return hash(s.v)\n    def __eq__(s, o): return isinstance(o, P) and s.v == o.v\nv = lambda s: sorted(e.v for e in s)\nd = {P(1): 0, P(2): 0}\n";
    assert_eq!(g(&format!("{C}x = v(d.keys() & {{P(2)}})"), "x"), "[2]");
    assert_eq!(
        g(&format!("{C}x = v(d.keys() | {{P(3)}})"), "x"),
        "[1, 2, 3]"
    );
    assert_eq!(g(&format!("{C}x = v(d.keys() - {{P(1)}})"), "x"), "[2]");
    assert_eq!(
        g(&format!("{C}x = v(d.keys() ^ {{P(1), P(3)}})"), "x"),
        "[2, 3]"
    );
    assert_eq!(
        g(
            &format!("{C}x = (d.keys() == {{P(1), P(2)}}, d.keys() <= {{P(1)}})"),
            "x"
        ),
        "(True, False)"
    );
}

#[test]
fn set_predicates_answer_for_an_unhashable_free_iterable() {
    // `{1}.issubset([P(1)])` is False in CPython, not a TypeError: the argument's
    // elements are hashed with the receiver's keys as collapse candidates. With
    // no candidate to collapse onto, hashing still has to happen OUTSIDE the host
    // borrow, which the old short-circuit skipped.
    const C: &str = "class P:\n    def __init__(s, v): s.v = v\n    def __hash__(s): return hash(s.v)\n    def __eq__(s, o): return isinstance(o, P) and s.v == o.v\n";
    assert_eq!(
        g(
            &format!("{C}x = ({{1}}.issubset([P(1)]), {{1}}.isdisjoint([P(1)]))"),
            "x"
        ),
        "(False, True)"
    );
    assert_eq!(
        g(
            &format!("{C}x = ({{P(1)}}.issubset([P(1)]), {{P(1)}}.isdisjoint([1]))"),
            "x"
        ),
        "(True, True)"
    );
}

#[test]
fn slots_validation_matches_type_new() {
    // CPython `type_new_slots_impl`: a slot name that is also bound in the class
    // body would be shadowed by the slot descriptor, so class creation rejects it.
    for (src, want) in [
        (
            "class C:\n    __slots__ = ('a',)\n    a = 1",
            "ValueError: 'a' in __slots__ conflicts with class variable",
        ),
        // The single-name string form is the same check.
        (
            "class C:\n    __slots__ = 'a'\n    a = 1",
            "ValueError: 'a' in __slots__ conflicts with class variable",
        ),
        // A method counts as a class variable.
        (
            "class C:\n    __slots__ = ['m']\n    def m(self): pass",
            "ValueError: 'm' in __slots__ conflicts with class variable",
        ),
        // Validity is checked first, for every slot, before any conflict.
        (
            "class C:\n    __slots__ = ('a b',)",
            "TypeError: __slots__ must be identifiers",
        ),
        (
            "class C:\n    __slots__ = (1,)",
            "TypeError: __slots__ items must be strings, not 'int'",
        ),
        (
            "class C:\n    __slots__ = ('__dict__', '__dict__')",
            "TypeError: __dict__ slot disallowed: we already got one",
        ),
    ] {
        assert_eq!(pythonrs::eval_str(src).unwrap_err(), want, "for {src:?}");
    }
    // Accepted: a slot with no namespace binding, an empty `__slots__` next to a
    // class variable, a lone `__dict__`, and `__qualname__` (class creation
    // inserts that one itself, so it is exempt from the conflict check).
    for src in [
        "class C:\n    __slots__ = ('a',)\n    def m(self): pass",
        "class C:\n    __slots__ = ()\n    a = 1",
        "class C:\n    __slots__ = ('__dict__',)",
        "class C:\n    __slots__ = ('__qualname__',)",
        // `__doc__` too, as long as the body has no docstring: CPython's compiler
        // emits the `__doc__` store ONLY for a real docstring, so there is
        // nothing for the slot descriptor to collide with. `typing._SpecialForm`
        // is exactly this shape (`__slots__ = ('_name', '__doc__', '_getitem')`
        // with no docstring), so a synthesized `__doc__ = None` in the namespace
        // made `import typing` itself fail.
        "class C:\n    __slots__ = ('__doc__',)",
        "class C:\n    __slots__ = ('_name', '__doc__', '_getitem')",
    ] {
        assert!(pythonrs::eval_str(src).is_ok(), "for {src:?}");
    }
    // A docstring IS a class variable, so slotting `__doc__` beside one conflicts.
    assert_eq!(
        pythonrs::eval_str("class C:\n    'doc'\n    __slots__ = ('__doc__',)").unwrap_err(),
        "ValueError: '__doc__' in __slots__ conflicts with class variable"
    );
    // The slot still restricts attributes after passing validation.
    assert_eq!(
        g(
            "class C:\n    __slots__ = ('a',)\nc = C()\nc.a = 1\ntry:\n    c.b = 2\n    x = 'set'\nexcept AttributeError:\n    x = (c.a, 'blocked')",
            "x"
        ),
        "(1, 'blocked')"
    );
}

#[test]
fn walrus_in_comprehension_leaks() {
    // A `:=` target inside a comprehension binds in the enclosing scope (PEP 572),
    // not the hidden comprehension function; the result is unaffected.
    assert_eq!(
        g("r = range(3)\nres = [y for x in r if (y := x)]", "res"),
        "[1, 2]"
    );
    assert_eq!(g("r = range(3)\n_ = [y for x in r if (y := x)]", "y"), "2");
    // Walrus in the element, over a list.
    assert_eq!(g("_ = [(z := i) + z for i in [1, 2, 3]]", "z"), "3");
    // Set and dict comprehensions leak their walrus target too.
    assert_eq!(g("_ = {(k := x) for x in range(4)}", "k"), "3");
    assert_eq!(g("_ = {(m := x): x for x in range(2)}", "m"), "1");
    // Inside a function the target is nonlocal to that function, not global; the
    // function exposes it via its return so we can read it back at module scope.
    assert_eq!(
        g(
            "def f():\n    t = -1\n    out = [t for x in range(3) if (t := x * 2)]\n    return out, t\nres = f()",
            "res"
        ),
        "([2, 4], 4)"
    );
}

#[test]
fn user_exception_str_repr_args() {
    // A user Exception subclass inherits BaseException's args/str/repr: str is
    // the message ('' / str(arg) / repr(tuple)), repr is `Class(arg, …)`.
    assert_eq!(
        g("class E(Exception): pass\ns = str(E('boom'))", "s"),
        "'boom'"
    );
    assert_eq!(g("class E(Exception): pass\ns = str(E())", "s"), "''");
    assert_eq!(
        g("class E(Exception): pass\ns = str(E('a', 'b'))", "s"),
        "\"('a', 'b')\""
    );
    assert_eq!(
        g("class E(Exception): pass\nr = repr(E('a', 'b'))", "r"),
        "\"E('a', 'b')\""
    );
    assert_eq!(g("class E(Exception): pass\nr = repr(E())", "r"), "'E()'");
    assert_eq!(
        g("class E(Exception): pass\na = E('x', 1).args", "a"),
        "('x', 1)"
    );
    assert_eq!(g("class E(Exception): pass\na = E().args", "a"), "()");
    // isinstance across the builtin hierarchy + user subclass chain.
    assert_eq!(
        g(
            "class A(Exception): pass\nclass B(A): pass\nb = isinstance(B('m'), A) and isinstance(B('m'), Exception)",
            "b"
        ),
        "True"
    );
    // A user __init__ that calls super().__init__ overrides args; a custom
    // __str__ still leaves the default repr = `Class(args…)`.
    assert_eq!(
        g(
            "class E(Exception):\n    def __init__(self, k):\n        super().__init__('missing ' + k)\n        self.k = k\ne = E('id')\nres = (str(e), e.args, e.k)",
            "res"
        ),
        "('missing id', ('missing id',), 'id')"
    );
    assert_eq!(
        g(
            "class E(Exception):\n    def __str__(self): return 'custom'\nres = (str(E('z')), repr(E('z')))",
            "res"
        ),
        "('custom', \"E('z')\")"
    );
    // Caught user exception: `e` and `e.args` are usable in the handler.
    assert_eq!(
        g(
            "out = None\nclass E(Exception): pass\ntry:\n    raise E('bang')\nexcept E as e:\n    out = (str(e), e.args)",
            "out"
        ),
        "('bang', ('bang',))"
    );
}

#[test]
fn super_in_property_accessor() {
    // A zero-arg super() inside a property getter resolves self + the defining
    // class, so both super().<method>() and super().<property> work.
    assert_eq!(
        g(
            "class A:\n    def base(self): return 10\nclass B(A):\n    @property\n    def v(self): return super().base() + 1\nx = B().v",
            "x"
        ),
        "11"
    );
    assert_eq!(
        g(
            "class A:\n    @property\n    def v(self): return 10\nclass B(A):\n    @property\n    def v(self): return super().v + 5\nx = B().v",
            "x"
        ),
        "15"
    );
}

#[test]
fn fstring_ascii_conversion() {
    // `!a` ascii-escapes non-ASCII in the repr (previously passed repr through).
    // Built via chr() so the expected value has no backslash-escaping ambiguity:
    // ascii(chr(233)) == "'" + "\\" + "xe9" + "'".
    assert_eq!(
        g("b = f'{chr(233)!a}' == chr(39)+chr(92)+'xe9'+chr(39)", "b"),
        "True"
    );
    assert_eq!(
        g(
            "b = f'{chr(1000)!a}' == chr(39)+chr(92)+'u03e8'+chr(39)",
            "b"
        ),
        "True"
    );
    // The `ascii()` builtin agrees with `!a`.
    assert_eq!(
        g("b = ascii(chr(233)) == chr(39)+chr(92)+'xe9'+chr(39)", "b"),
        "True"
    );
    // `!r` leaves non-ASCII intact: repr(chr(233)) == "'é'".
    assert_eq!(
        g("b = f'{chr(233)!r}' == chr(39)+chr(233)+chr(39)", "b"),
        "True"
    );
}

#[test]
fn str_percent_format_native_authoritative() {
    // `str % obj` is native formatting (str.__mod__), authoritative over any
    // right-operand __rmod__: a %s/%r of an exception instance uses its message.
    assert_eq!(
        g("class E(Exception): pass\ns = '%s' % E('boom')", "s"),
        "'boom'"
    );
    assert_eq!(
        g("class E(Exception): pass\ns = '%r' % E('x', 1)", "s"),
        "\"E('x', 1)\""
    );
    // A right operand with `__rmod__` never intercepts `str %` — str formatting
    // wins, so a mismatched arg count raises rather than calling __rmod__.
    let e = eval_str("class V:\n    def __rmod__(self, o): return 'nope'\nx = 'lit' % V()")
        .unwrap_err();
    assert!(
        e.contains("not all arguments converted"),
        "unexpected error: {e}"
    );
    // Plain-value %-format (tuples, %r) is unaffected.
    assert_eq!(g("s = '%s=%r' % ('k', (1, 2))", "s"), "'k=(1, 2)'");
}

#[test]
fn init_subclass_hook() {
    // PEP 487: the parent's __init_subclass__ fires with the new class and the
    // class-header keywords.
    assert_eq!(
        g(
            "class P:\n    def __init_subclass__(cls, /, tag=None, **kw):\n        cls.tag = tag\nclass C(P, tag='x'): pass\nt = C.tag",
            "t"
        ),
        "'x'"
    );
    // An explicit @classmethod form and no-keyword default both work.
    assert_eq!(
        g(
            "seen = []\nclass P:\n    @classmethod\n    def __init_subclass__(cls, **kw):\n        seen.append(cls.__name__)\nclass C(P): pass\nout = seen",
            "out"
        ),
        "['C']"
    );
    // Extra keywords with only object's default hook is a TypeError.
    let e = eval_str("class P: pass\nclass C(P, tag='x'): pass").unwrap_err();
    assert!(
        e.contains("__init_subclass__() takes no keyword arguments"),
        "unexpected error: {e}"
    );
}

#[test]
fn format_spec_sign_aware_zero_pad() {
    // The `0` flag / `=` align inserts fill AFTER the sign and any radix prefix.
    assert_eq!(g("s = f'{5:+05d}'", "s"), "'+0005'");
    assert_eq!(g("s = f'{-3:05d}'", "s"), "'-0003'");
    assert_eq!(g("s = f'{5: 05d}'", "s"), "' 0005'");
    assert_eq!(g("s = f'{255:#08x}'", "s"), "'0x0000ff'");
    assert_eq!(g("s = f'{-255:#08x}'", "s"), "'-0x000ff'");
    assert_eq!(g("s = f'{3.14:+08.2f}'", "s"), "'+0003.14'");
    assert_eq!(g("s = f'{-42:=8d}'", "s"), "'-     42'");
    // A `+`/space sign flag prefixes a non-negative value.
    assert_eq!(g("s = f'{5: d}'", "s"), "' 5'");
    assert_eq!(g("s = f'{7:>6d}'", "s"), "'     7'");
    // `parse_internal_render_format_spec`'s 0-padding special case keys off
    // whether a FILL was named, not whether an alignment was: `<08d` writes an
    // alignment but no fill, so the `0` still becomes the fill and pads on the
    // right. Naming a fill (`*<08d`) makes the `0` part of the width instead.
    // CPython 3.14.6.
    assert_eq!(g("s = format(1, '<08d')", "s"), "'10000000'");
    assert_eq!(g("s = format(1, '*<08d')", "s"), "'1*******'");
    assert_eq!(g("s = format(1.0, '< 012.2e')", "s"), "' 1.00e+00000'");
    assert_eq!(g("s = format(1, '^08d')", "s"), "'00010000'");
    // Grouping stops at the digits: the `%` suffix and the exponent are
    // remainder, so neither takes a separator.
    assert_eq!(g("s = format(1, '_.0%')", "s"), "'100%'");
    assert_eq!(g("s = format(1234, '_.0%')", "s"), "'123_400%'");
    assert_eq!(g("s = format(1.5, ',.0')", "s"), "'2e+00'");
}

/// `c` produces a character, so the numeric decorations are rejected outright —
/// including a bare `-`, which every other type accepts as the default sign.
/// CPython 3.14.6 messages.
#[test]
fn char_format_rejects_sign_and_alternate_form() {
    for spec in ["+c", "-c", " c"] {
        assert_eq!(
            pythonrs::eval_str(&format!("x = format(65, '{spec}')")).unwrap_err(),
            "ValueError: Sign not allowed with integer format specifier 'c'",
            "spec {spec}"
        );
    }
    assert_eq!(
        pythonrs::eval_str("x = format(65, '#c')").unwrap_err(),
        "ValueError: Alternate form (#) not allowed with integer format specifier 'c'"
    );
    // The precision check runs FIRST, so it wins over both of the above.
    assert_eq!(
        pythonrs::eval_str("x = format(65, '+.1c')").unwrap_err(),
        "ValueError: Precision not allowed in integer format specifier"
    );
    // Everything else about `c` still works.
    assert_eq!(g("x = format(65, '=c')", "x"), "'A'");
    assert_eq!(g("x = format(65, '05c')", "x"), "'0000A'");
}

// ── async / await / asyncio (native fusevm event loop) ───────────────────────

#[test]
fn async_def_returns_coroutine() {
    // Calling an `async def` returns a coroutine object; the body does NOT run.
    assert_eq!(
        g(
            "async def f():\n    return 1\nc = f()\nt = type(c).__name__\nimport asyncio\nasyncio.run(c)",
            "t"
        ),
        "'coroutine'"
    );
}

#[test]
fn asyncio_run_awaits_result() {
    assert_eq!(
        g(
            "import asyncio\nasync def main():\n    await asyncio.sleep(0)\n    return 7\nr = asyncio.run(main())",
            "r"
        ),
        "7"
    );
}

#[test]
fn asyncio_gather_ordered_results() {
    assert_eq!(
        g(
            "import asyncio\nasync def sq(n):\n    await asyncio.sleep(0)\n    return n*n\nasync def main():\n    return await asyncio.gather(sq(1), sq(2), sq(3))\nr = asyncio.run(main())",
            "r"
        ),
        "[1, 4, 9]"
    );
}

#[test]
fn asyncio_create_task_and_future() {
    // A Task sets a Future's result; the main coroutine awaits the Future.
    assert_eq!(
        g(
            "import asyncio\nasync def setter(fut):\n    await asyncio.sleep(0)\n    fut.set_result(99)\nasync def main():\n    fut = asyncio.Future()\n    asyncio.create_task(setter(fut))\n    return await fut\nr = asyncio.run(main())",
            "r"
        ),
        "99"
    );
}

#[test]
fn await_exception_propagates() {
    assert_eq!(
        g(
            "import asyncio\nasync def boom():\n    await asyncio.sleep(0)\n    raise ValueError('nope')\nasync def main():\n    try:\n        await boom()\n    except ValueError as e:\n        return str(e)\nr = asyncio.run(main())",
            "r"
        ),
        "'nope'"
    );
}

#[test]
fn asyncio_sleep_timer_ordering() {
    // Timers fire in virtual-clock order regardless of scheduling order.
    assert_eq!(
        g(
            "import asyncio\nout = []\nasync def t(name, d):\n    await asyncio.sleep(d)\n    out.append(name)\nasync def main():\n    await asyncio.gather(t('slow', 0.2), t('fast', 0.1), t('mid', 0.15))\nasyncio.run(main())",
            "out"
        ),
        "['fast', 'mid', 'slow']"
    );
}

#[test]
fn async_for_custom_aiterator() {
    let src = "import asyncio\n\
class R:\n    def __init__(self, n):\n        self.n = n\n        self.i = 0\n    def __aiter__(self):\n        return self\n    async def __anext__(self):\n        if self.i >= self.n:\n            raise StopAsyncIteration\n        self.i += 1\n        await asyncio.sleep(0)\n        return self.i\n\
out = []\n\
async def main():\n    async for x in R(3):\n        out.append(x)\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[1, 2, 3]");
}

#[test]
fn async_with_context_manager() {
    let src = "import asyncio\n\
log = []\n\
class CM:\n    async def __aenter__(self):\n        log.append('enter')\n        return 5\n    async def __aexit__(self, *a):\n        log.append('exit')\n        return False\n\
async def main():\n    async with CM() as r:\n        log.append(r)\n\
asyncio.run(main())";
    assert_eq!(g(src, "log"), "['enter', 5, 'exit']");
}

#[test]
fn async_comprehension_list() {
    let src = "import asyncio\n\
class R:\n    def __init__(self, n):\n        self.n = n\n        self.i = 0\n    def __aiter__(self):\n        return self\n    async def __anext__(self):\n        if self.i >= self.n:\n            raise StopAsyncIteration\n        self.i += 1\n        await asyncio.sleep(0)\n        return self.i\n\
async def main():\n    return [x * x async for x in R(4)]\n\
r = asyncio.run(main())";
    assert_eq!(g(src, "r"), "[1, 4, 9, 16]");
}

#[test]
fn async_comprehension_filter_and_dict() {
    let src = "import asyncio\n\
class R:\n    def __init__(self, n):\n        self.n = n\n        self.i = 0\n    def __aiter__(self):\n        return self\n    async def __anext__(self):\n        if self.i >= self.n:\n            raise StopAsyncIteration\n        self.i += 1\n        return self.i\n\
async def main():\n    return {x: x * x async for x in R(4) if x % 2 == 0}\n\
r = asyncio.run(main())";
    assert_eq!(g(src, "r"), "{2: 4, 4: 16}");
}

#[test]
fn asyncio_event_wait_set() {
    let src = "import asyncio\n\
async def waiter(ev, out):\n    await ev.wait()\n    out.append('woke')\n\
out = []\n\
async def main():\n    ev = asyncio.Event()\n    t = asyncio.create_task(waiter(ev, out))\n    await asyncio.sleep(0)\n    out.append('set')\n    ev.set()\n    await t\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "['set', 'woke']");
}

#[test]
fn asyncio_lock_mutual_exclusion() {
    let src = "import asyncio\n\
out = []\n\
async def worker(lock, n):\n    async with lock:\n        out.append('in ' + str(n))\n        await asyncio.sleep(0)\n        out.append('out ' + str(n))\n\
async def main():\n    lock = asyncio.Lock()\n    await asyncio.gather(worker(lock, 1), worker(lock, 2))\n\
asyncio.run(main())";
    // The lock serializes the critical sections: 1 fully then 2 fully.
    assert_eq!(g(src, "out"), "['in 1', 'out 1', 'in 2', 'out 2']");
}

#[test]
fn asyncio_queue_producer_consumer() {
    let src = "import asyncio\n\
out = []\n\
async def producer(q):\n    for i in range(3):\n        await q.put(i)\n\
async def consumer(q):\n    for _ in range(3):\n        out.append(await q.get())\n\
async def main():\n    q = asyncio.Queue()\n    await asyncio.gather(producer(q), consumer(q))\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[0, 1, 2]");
}

#[test]
fn async_generator_comprehension() {
    let src = "import asyncio\n\
async def ag(n):\n    for i in range(n):\n        await asyncio.sleep(0)\n        yield i * i\n\
async def main():\n    return [x async for x in ag(4)]\n\
r = asyncio.run(main())";
    assert_eq!(g(src, "r"), "[0, 1, 4, 9]");
}

#[test]
fn async_generator_type_and_async_for() {
    let src = "import asyncio\n\
async def ag(n):\n    for i in range(n):\n        await asyncio.sleep(0)\n        yield i * 10\n\
out = []\n\
async def main():\n    async for v in ag(3):\n        out.append(v)\n    return type(ag(1)).__name__\n\
tn = asyncio.run(main())";
    assert_eq!(g(src, "out"), "[0, 10, 20]");
    assert_eq!(g(src, "tn"), "'async_generator'");
}

#[test]
fn task_cancel_caught_inside_coroutine() {
    // Cancelling a suspended Task injects CancelledError at its await point; the
    // coroutine's try/except runs, and returning normally leaves it un-cancelled.
    let src = "import asyncio\n\
out = []\n\
async def worker():\n    try:\n        await asyncio.sleep(10)\n        return 'no'\n    except asyncio.CancelledError:\n        return 'caught'\n\
async def main():\n    t = asyncio.create_task(worker())\n    await asyncio.sleep(0)\n    c = t.cancel()\n    r = await t\n    out.append(c)\n    out.append(r)\n    out.append(t.cancelled())\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[True, 'caught', False]");
}

#[test]
fn task_cancel_propagates_and_marks_cancelled() {
    // A coroutine that does not catch CancelledError becomes a cancelled Task:
    // awaiting it raises, and cancelled() is True.
    let src = "import asyncio\n\
out = []\n\
async def worker():\n    await asyncio.sleep(10)\n    return 'no'\n\
async def main():\n    t = asyncio.create_task(worker())\n    await asyncio.sleep(0)\n    t.cancel()\n    try:\n        await t\n        out.append('no-raise')\n    except asyncio.CancelledError:\n        out.append('raised')\n    out.append(t.cancelled())\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "['raised', True]");
}

#[test]
fn async_generator_asend_roundtrip() {
    // `asend(v)` resumes the body, `v` becoming the value of the `yield`
    // expression; exhaustion raises StopAsyncIteration.
    let src = "import asyncio\n\
async def ag():\n    a = yield 1\n    b = yield a + 1\n    yield b + 1\n\
out = []\n\
async def main():\n    g = ag()\n    out.append(await g.asend(None))\n    out.append(await g.asend(10))\n    out.append(await g.asend(20))\n    try:\n        await g.asend(0)\n    except StopAsyncIteration:\n        out.append('stop')\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[1, 11, 21, 'stop']");
}

#[test]
fn async_generator_athrow_caught() {
    // `athrow(exc)` raises at the current `yield`; a body that catches it and
    // yields again returns that next value.
    let src = "import asyncio\n\
out = []\n\
async def ag():\n    try:\n        while True:\n            yield 1\n    except ValueError:\n        yield 2\n\
async def main():\n    g = ag()\n    out.append(await g.asend(None))\n    out.append(await g.athrow(ValueError))\n    await g.aclose()\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[1, 2]");
}

#[test]
fn async_generator_aclose_finishes() {
    // `aclose()` raises GeneratorExit and drives the body to completion; a later
    // `asend` on the closed generator raises StopAsyncIteration.
    let src = "import asyncio\n\
out = []\n\
async def ag():\n    try:\n        yield 1\n        yield 2\n    finally:\n        out.append('cleanup')\n\
async def main():\n    g = ag()\n    out.append(await g.asend(None))\n    await g.aclose()\n    try:\n        await g.asend(None)\n    except StopAsyncIteration:\n        out.append('stop')\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[1, 'cleanup', 'stop']");
}

#[test]
fn asyncio_wait_for_timeout_and_success() {
    // `wait_for` raises TimeoutError past the deadline, and returns the result
    // when the awaitable finishes in time.
    let src = "import asyncio\n\
out = []\n\
async def slow():\n    await asyncio.sleep(10)\n    return 'slow'\n\
async def fast():\n    await asyncio.sleep(0)\n    return 'fast'\n\
async def main():\n    try:\n        await asyncio.wait_for(slow(), timeout=1)\n        out.append('no')\n    except asyncio.TimeoutError:\n        out.append('timeout')\n    out.append(await asyncio.wait_for(fast(), timeout=5))\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "['timeout', 'fast']");
}

#[test]
fn asyncio_bounded_queue_backpressure() {
    // A bounded Queue blocks `put` while full; the consumer drains it in order.
    let src = "import asyncio\n\
out = []\n\
async def main():\n    q = asyncio.Queue(maxsize=2)\n    async def prod():\n        for i in range(5):\n            await q.put(i)\n        await q.put(-1)\n    async def cons():\n        while True:\n            v = await q.get()\n            if v == -1:\n                break\n            out.append(v)\n            await asyncio.sleep(0)\n    await asyncio.gather(prod(), cons())\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[0, 1, 2, 3, 4]");
}

#[test]
fn asyncio_wait_first_completed() {
    // `wait(return_when=FIRST_COMPLETED)` settles as soon as one task finishes,
    // leaving the slower one pending.
    let src = "import asyncio\n\
out = []\n\
async def f(v, d):\n    await asyncio.sleep(d)\n    return v\n\
async def main():\n    t1 = asyncio.create_task(f(1, 3))\n    t2 = asyncio.create_task(f(2, 1))\n    done, pending = await asyncio.wait([t1, t2], return_when=asyncio.FIRST_COMPLETED)\n    out.append(len(done))\n    out.append(len(pending))\n    await asyncio.wait([t1, t2])\n\
asyncio.run(main())";
    assert_eq!(g(src, "out"), "[1, 1]");
}

/// `str.splitlines`: the full CPython line-boundary set (`\n \r \r\n \v \f \x1c
/// \x1d \x1e \x85    `), `\r\n` as one break, no trailing empty line,
/// and `keepends` retaining the boundary characters.
#[test]
fn str_splitlines_boundaries_and_keepends() {
    assert_eq!(g("x = 'a\\nb\\r\\nc'.splitlines()", "x"), "['a', 'b', 'c']");
    assert_eq!(
        g("x = 'a\\nb\\n'.splitlines(True)", "x"),
        "['a\\n', 'b\\n']"
    );
    assert_eq!(
        g("x = 'a\\rb\\r\\nc\\n'.splitlines(True)", "x"),
        "['a\\r', 'b\\r\\n', 'c\\n']"
    );
    // Vertical tab, form feed, and the C1/Unicode separators are all breaks.
    assert_eq!(
        g(
            "x = 'a\\x0bb\\x0cc\\x1cd\\x1ee\\x85f\\u2028g'.splitlines()",
            "x"
        ),
        "['a', 'b', 'c', 'd', 'e', 'f', 'g']"
    );
    // No trailing empty element for a terminal boundary; interior blank stays.
    assert_eq!(g("x = 'a\\n\\nb'.splitlines()", "x"), "['a', '', 'b']");
    assert_eq!(g("x = ''.splitlines()", "x"), "[]");
}

/// `str.casefold`: full Unicode folding, not just simple lowercasing — the
/// multi-character folds (`ß`->`ss`, titlecase digraphs) that `str.lower` misses.
#[test]
fn str_casefold_full_folding() {
    assert_eq!(g("x = 'Straße'.casefold()", "x"), "'strasse'");
    assert_eq!(g("x = 'ǅ'.casefold()", "x"), "'ǆ'"); // U+01C5 -> U+01C6
    assert_eq!(g("x = 'ﬀ'.casefold()", "x"), "'ff'"); // U+FB00 LATIN SMALL LIGATURE FF
                                                      // Ordinary text folds identically to lowercasing.
    assert_eq!(g("x = 'HELLO World'.casefold()", "x"), "'hello world'");
    // `lower` must NOT gain the full folds (ß stays ß).
    assert_eq!(g("x = 'Straße'.lower()", "x"), "'straße'");
}

/// A float formatted with a precision but NO presentation type (`f"{x:.3}"`,
/// `format(x, '.3')`) uses CPython's "general" format — significant digits with a
/// `g`-style switch to scientific (one exponent sooner than `g`), keeping a
/// trailing `.0` for a whole result — NOT fixed-point (`.3f`).
#[test]
fn float_no_type_precision_format() {
    assert_eq!(g("x = format(3.14159, '.3')", "x"), "'3.14'");
    assert_eq!(g("x = format(2.0, '.3')", "x"), "'2.0'");
    assert_eq!(g("x = format(100.0, '.3')", "x"), "'1e+02'");
    assert_eq!(g("x = format(100.0, '.5')", "x"), "'100.0'");
    assert_eq!(g("x = format(12345.678, '.5')", "x"), "'1.2346e+04'");
    // Rounding carry bumps the exponent across the scientific threshold.
    assert_eq!(g("x = format(9.99, '.2')", "x"), "'1e+01'");
    // Width padding still applies around the general body.
    assert_eq!(g("x = f'{3.14159:{5}.{3}}'", "x"), "' 3.14'");
    // A fixed `.Nf` type is unaffected.
    assert_eq!(g("x = format(3.14159, '.3f')", "x"), "'3.142'");
}

/// The argument-clinic `str` methods accept their arguments by keyword
/// (`"a b c".split(maxsplit=1)`); every other `str` method rejects keywords with
/// CPython's `TypeError`, and an unexpected keyword on an accepting method also
/// raises.
#[test]
fn str_method_keyword_arguments() {
    assert_eq!(g("x = 'a b c d'.split(maxsplit=1)", "x"), "['a', 'b c d']");
    assert_eq!(
        g("x = 'a-b-c'.split(sep='-', maxsplit=1)", "x"),
        "['a', 'b-c']"
    );
    assert_eq!(g("x = 'aaa'.replace('a', 'b', count=2)", "x"), "'bba'");
    assert_eq!(g("x = 'a\\tb'.expandtabs(tabsize=4)", "x"), "'a   b'");
    // A non-accepting method raises "takes no keyword arguments".
    assert_eq!(
        g(
            "try:\n    'x'.center(5, fillchar='*')\nexcept TypeError as e:\n    x = str(e)",
            "x"
        ),
        "'str.center() takes no keyword arguments'"
    );
    // An unexpected keyword on an accepting method raises.
    assert_eq!(
        g(
            "try:\n    'a b'.split(bad=1)\nexcept TypeError as e:\n    x = str(e)",
            "x"
        ),
        "\"split() got an unexpected keyword argument 'bad'\""
    );
}

/// The native `math.gcd`/`floor`/`ceil` are bignum-safe: `gcd` is variadic
/// (CPython 3.9+) and does not truncate an operand beyond `i64` to `0`, and
/// `floor`/`ceil` of a large float produce an exact `int`, not the i64-saturated
/// cast.
#[test]
fn math_bignum_safe() {
    assert_eq!(
        g(
            "import math\nx = math.gcd(123456789012345678901234567890, 987654321)",
            "x"
        ),
        "9"
    );
    assert_eq!(g("import math\nx = math.gcd(2**70, 12)", "x"), "4");
    assert_eq!(g("import math\nx = math.gcd(24, 36, 48)", "x"), "12");
    assert_eq!(g("import math\nx = math.gcd()", "x"), "0");
    assert_eq!(
        g("import math\nx = math.floor(1e20)", "x"),
        "100000000000000000000"
    );
    assert_eq!(
        g("import math\nx = math.ceil(-1e20)", "x"),
        "-100000000000000000000"
    );
    assert_eq!(g("import math\nx = math.floor(3.7)", "x"), "3");
}

/// `str.title`/`capitalize` use the Unicode *titlecase* mapping for the leading
/// letter, not uppercase — the Latin digraph ligatures (`ǳ` → `ǲ`, not `Ǳ`)
/// differ. `str.isdecimal`/`isdigit`/`isnumeric` follow the Unicode
/// Decimal/Digit/Numeric properties (other scripts' decimals, superscripts,
/// circled digits, fractions), not just ASCII.
#[test]
fn unicode_titlecase_and_numeric_predicates() {
    assert_eq!(g("x = '\u{01F3}'.title()", "x"), "'\u{01F2}'");
    assert_eq!(g("x = '\u{01F3}'.upper()", "x"), "'\u{01F1}'");
    assert_eq!(g("x = '\u{01C6}xyz'.capitalize()", "x"), "'\u{01C5}xyz'");
    assert_eq!(g("x = 'hello world'.title()", "x"), "'Hello World'");
    // Decimal: other scripts' Nd digits, not superscripts/fractions.
    assert_eq!(g("x = '\u{0969}'.isdecimal()", "x"), "True"); // Devanagari 3
    assert_eq!(g("x = '\u{FF15}'.isdecimal()", "x"), "True"); // fullwidth 5
    assert_eq!(g("x = '\u{00B2}'.isdecimal()", "x"), "False"); // superscript 2
                                                               // Digit: decimals plus Numeric_Type=Digit (superscripts, circled).
    assert_eq!(g("x = '\u{00B2}'.isdigit()", "x"), "True"); // superscript 2
    assert_eq!(g("x = '\u{2465}'.isdigit()", "x"), "True"); // circled 6
    assert_eq!(g("x = '\u{00BD}'.isdigit()", "x"), "False"); // 1/2 fraction
                                                             // Numeric: also fractions and letter-numbers.
    assert_eq!(g("x = '\u{00BD}'.isnumeric()", "x"), "True"); // 1/2
    assert_eq!(g("x = '\u{2167}'.isnumeric()", "x"), "True"); // Roman VIII
    assert_eq!(g("x = '\u{2167}'.isdigit()", "x"), "False");
}

/// List/tuple membership (`in`), `.index`, `.count`, and `.remove` honor a user
/// `__eq__` (CPython's `PyObject_RichCompareBool` — identity first, then `==`),
/// not native identity. Previously an instance was found only by identity.
#[test]
fn sequence_membership_uses_eq() {
    let cls = "class M:\n    def __init__(s, v): s.v = v\n    def __eq__(s, o): return isinstance(o, M) and s.v == o.v\n    def __hash__(s): return hash(s.v)\n";
    assert_eq!(g(&format!("{cls}x = M(1) in [M(1), M(2)]"), "x"), "True");
    assert_eq!(g(&format!("{cls}x = M(3) in [M(1), M(2)]"), "x"), "False");
    assert_eq!(g(&format!("{cls}x = M(1) in (M(1), M(2))"), "x"), "True");
    assert_eq!(g(&format!("{cls}x = [M(1), M(2)].index(M(2))"), "x"), "1");
    assert_eq!(
        g(&format!("{cls}x = [M(1), M(2), M(1)].count(M(1))"), "x"),
        "2"
    );
    assert_eq!(
        g(
            &format!("{cls}l = [M(1), M(2), M(3)]\nl.remove(M(2))\nx = [m.v for m in l]"),
            "x"
        ),
        "[1, 3]"
    );
}

/// `str.swapcase` is Unicode-aware: accented letters swap case (`ï`->`Ï`,
/// `é`->`É`) and a 1->many mapping expands (`ß`->`SS`); an ASCII-only
/// implementation left the accented letters unchanged.
#[test]
fn str_swapcase_unicode() {
    assert_eq!(g("x = 'naïve'.swapcase()", "x"), "'NAÏVE'");
    assert_eq!(g("x = 'É'.swapcase()", "x"), "'é'");
    assert_eq!(g("x = 'café ÑOÑO'.swapcase()", "x"), "'CAFÉ ñoño'");
    assert_eq!(g("x = 'ß'.swapcase()", "x"), "'SS'");
    // ASCII and non-cased characters behave as before.
    assert_eq!(
        g("x = 'Hello, World! 123'.swapcase()", "x"),
        "'hELLO, wORLD! 123'"
    );
}

/// `int.bit_count` / `int.bit_length` for native and bignum ints (ones and bit
/// width of the magnitude).
#[test]
fn int_bit_count_and_length() {
    assert_eq!(g("x = (255).bit_count()", "x"), "8");
    assert_eq!(g("x = (0).bit_count()", "x"), "0");
    assert_eq!(g("x = (-7).bit_count()", "x"), "3"); // magnitude of -7 is 0b111
    assert_eq!(g("x = (2**64 - 1).bit_count()", "x"), "64");
    assert_eq!(g("x = (2**100).bit_count()", "x"), "1");
    assert_eq!(g("x = (2**100).bit_length()", "x"), "101");
    assert_eq!(g("x = (0).bit_length()", "x"), "0");
}

/// `int.to_bytes` / `int.from_bytes`: byteorder, `signed` two's complement, the
/// default length/byteorder, and a bignum round-trip.
#[test]
fn int_to_from_bytes() {
    assert_eq!(g("x = (10).to_bytes(2, 'big')", "x"), "b'\\x00\\n'");
    assert_eq!(g("x = (258).to_bytes(2, 'little')", "x"), "b'\\x02\\x01'");
    assert_eq!(g("x = (5).to_bytes()", "x"), "b'\\x05'"); // defaults: length 1, big
    assert_eq!(g("x = (0).to_bytes(0, 'big')", "x"), "b''");
    assert_eq!(
        g("x = (-1).to_bytes(2, 'big', signed=True)", "x"),
        "b'\\xff\\xff'"
    );
    assert_eq!(g("x = int.from_bytes(b'\\x01\\x02', 'big')", "x"), "258");
    assert_eq!(
        g("x = int.from_bytes(b'\\xff\\xff', 'big', signed=True)", "x"),
        "-1"
    );
    assert_eq!(g("x = int.from_bytes([1, 0], 'big')", "x"), "256");
    // Bignum round-trips through its own byte width.
    assert_eq!(
        g(
            "n = 2**100\nx = int.from_bytes(n.to_bytes(13, 'big'), 'big') == n",
            "x"
        ),
        "True"
    );
}

/// `int.to_bytes` overflow / bad-argument errors match CPython's messages.
#[test]
fn int_to_bytes_errors() {
    let e = |src: &str| eval_str(src).unwrap_err();
    assert!(e("(-1).to_bytes(2, 'big')").contains("can't convert negative int to unsigned"));
    assert!(e("(256).to_bytes(1, 'big')").contains("int too big to convert"));
    assert!(e("(128).to_bytes(1, 'big', signed=True)").contains("int too big to convert"));
    assert!(e("(5).to_bytes(2, 'middle')").contains("byteorder must be either 'little' or 'big'"));
}

/// `float.as_integer_ratio` (exact rational) and `int.as_integer_ratio`.
#[test]
fn as_integer_ratio_exact() {
    assert_eq!(g("x = (0.5).as_integer_ratio()", "x"), "(1, 2)");
    assert_eq!(g("x = (0.0).as_integer_ratio()", "x"), "(0, 1)");
    assert_eq!(g("x = (-2.5).as_integer_ratio()", "x"), "(-5, 2)");
    assert_eq!(g("x = (10).as_integer_ratio()", "x"), "(10, 1)");
    // 0.1 is not exactly a tenth — its true binary ratio surfaces here.
    assert_eq!(
        g("x = (0.1).as_integer_ratio()", "x"),
        "(3602879701896397, 36028797018963968)"
    );
}

/// `float.hex` / `float.fromhex`: exact hex formatting and a bit-exact round trip.
#[test]
fn float_hex_and_fromhex() {
    assert_eq!(g("x = (3.14).hex()", "x"), "'0x1.91eb851eb851fp+1'");
    assert_eq!(g("x = (1.0).hex()", "x"), "'0x1.0000000000000p+0'");
    assert_eq!(g("x = (0.0).hex()", "x"), "'0x0.0p+0'");
    assert_eq!(g("x = (-0.0).hex()", "x"), "'-0x0.0p+0'");
    // Smallest positive subnormal.
    assert_eq!(g("x = (5e-324).hex()", "x"), "'0x0.0000000000001p-1022'");
    assert_eq!(g("x = float.fromhex('0x1.8p+1')", "x"), "3.0");
    assert_eq!(g("x = float.fromhex('  0X1P4  ')", "x"), "16.0"); // no dot, uppercase, ws
    assert_eq!(g("x = float.fromhex('-inf')", "x"), "-inf");
    // Round-trip preserves the exact bits.
    assert_eq!(g("x = float.fromhex((0.1).hex()) == 0.1", "x"), "True");
}

#[test]
fn numeric_dunder_methods_int() {
    // The round-2 gap: numeric dunders are now callable bound methods on int.
    assert_eq!(g("x = (5).__index__()", "x"), "5");
    assert_eq!(g("x = (-3).__abs__()", "x"), "3");
    assert_eq!(g("x = (7).__floordiv__(2)", "x"), "3");
    assert_eq!(g("x = (1).__add__(2)", "x"), "3");
    assert_eq!(g("x = (5).__mul__(3)", "x"), "15");
    assert_eq!(g("x = (5).__mod__(3)", "x"), "2");
    assert_eq!(g("x = (5).__pow__(3)", "x"), "125");
    assert_eq!(g("x = (5).__neg__()", "x"), "-5");
    assert_eq!(g("x = (5).__invert__()", "x"), "-6");
    assert_eq!(g("x = (5).__divmod__(3)", "x"), "(1, 2)");
    assert_eq!(g("x = (5).__and__(3)", "x"), "1");
    assert_eq!(g("x = (5).__lshift__(2)", "x"), "20");
    assert_eq!(g("x = (10).__truediv__(4)", "x"), "2.5");
    assert_eq!(g("x = (5).__int__()", "x"), "5");
    assert_eq!(g("x = (3).__float__()", "x"), "3.0");
    assert_eq!(g("x = (5).__round__(1)", "x"), "5");
    assert_eq!(g("x = (123).__round__(-1)", "x"), "120");
    assert_eq!(g("x = (5).__bool__()", "x"), "True");
    assert_eq!(g("x = (0).__bool__()", "x"), "False");
    // Reflected dunders compute `other OP self`.
    assert_eq!(g("x = (5).__radd__(2)", "x"), "7");
    assert_eq!(g("x = (5).__rsub__(2)", "x"), "-3");
    assert_eq!(g("x = (5).__rfloordiv__(2)", "x"), "0");
    // bool inherits int's dunders and normalizes to int.
    assert_eq!(g("x = True.__index__()", "x"), "1");
    assert_eq!(g("x = True.__add__(1)", "x"), "2");
}

#[test]
fn numeric_dunder_methods_float_and_notimplemented() {
    assert_eq!(g("x = (2.0).__round__()", "x"), "2");
    assert_eq!(g("x = (3.14159).__round__(2)", "x"), "3.14");
    assert_eq!(g("x = (5.0).__floordiv__(2)", "x"), "2.0");
    assert_eq!(g("x = (3.7).__floor__()", "x"), "3");
    assert_eq!(g("x = (3.7).__ceil__()", "x"), "4");
    assert_eq!(g("x = (3.5).__int__()", "x"), "3");
    // int declines a float operand (returns NotImplemented, not TypeError);
    // float accepts an int operand.
    assert_eq!(g("x = (5).__add__(2.0)", "x"), "NotImplemented");
    assert_eq!(g("x = (1).__eq__('x')", "x"), "NotImplemented");
    assert_eq!(g("x = (5).__eq__(5.0)", "x"), "NotImplemented");
    assert_eq!(g("x = (2.0).__lt__(3)", "x"), "True");
    assert_eq!(g("x = (2.0).__lt__('x')", "x"), "NotImplemented");
    assert_eq!(g("x = (1).__eq__(1)", "x"), "True");
    // A dunder that hits a zero divisor raises, mirroring the operator.
    let e = eval_str("x = (5).__mod__(0)").unwrap_err();
    assert!(
        e.contains("ZeroDivisionError: division by zero"),
        "got: {e}"
    );
}

#[test]
fn zero_division_messages_match_314() {
    // CPython 3.14 unified all these to the bare "division by zero".
    for expr in [
        "5 // 0",
        "5 % 0",
        "5.0 // 0.0",
        "5.0 % 0.0",
        "1 / 0",
        "divmod(5, 0)",
    ] {
        let e = eval_str(&format!("x = {expr}")).unwrap_err();
        assert!(
            e.contains("ZeroDivisionError: division by zero"),
            "{expr} -> {e}"
        );
    }
    // Zero to a negative power (int and float base word it identically in 3.14).
    let e = eval_str("x = 0 ** -1").unwrap_err();
    assert!(e.contains("zero to a negative power"), "got: {e}");
    let e = eval_str("x = 0.0 ** -1").unwrap_err();
    assert!(e.contains("zero to a negative power"), "got: {e}");
}

#[test]
fn sequence_index_and_concat_error_messages() {
    // Index-out-of-range names the sequence type (except bytes, which is bare).
    let e = eval_str("x = [][5]").unwrap_err();
    assert!(e.contains("list index out of range"), "got: {e}");
    let e = eval_str("x = (1, 2)[5]").unwrap_err();
    assert!(e.contains("tuple index out of range"), "got: {e}");
    let e = eval_str("x = bytearray(b'ab')[9]").unwrap_err();
    assert!(e.contains("bytearray index out of range"), "got: {e}");
    let e = eval_str("x = b'ab'[9]").unwrap_err();
    assert!(
        e.contains("IndexError: index out of range") && !e.contains("bytes index"),
        "got: {e}"
    );
    // Concatenating a sequence with a wrong-typed operand uses the type-specific
    // concat message, not the generic "unsupported operand type(s)" one.
    let e = eval_str("x = 'a' + 1").unwrap_err();
    assert!(
        e.contains("can only concatenate str (not \"int\") to str"),
        "got: {e}"
    );
    let e = eval_str("x = [1] + (2,)").unwrap_err();
    assert!(
        e.contains("can only concatenate list (not \"tuple\") to list"),
        "got: {e}"
    );
    let e = eval_str("x = (1,) + [2]").unwrap_err();
    assert!(
        e.contains("can only concatenate tuple (not \"list\") to tuple"),
        "got: {e}"
    );
    let e = eval_str("x = b'a' + 1").unwrap_err();
    assert!(e.contains("can't concat int to bytes"), "got: {e}");
    let e = eval_str("x = bytearray(b'a') + 1").unwrap_err();
    assert!(e.contains("can't concat int to bytearray"), "got: {e}");
    // A non-sequence left operand keeps the generic operand message.
    let e = eval_str("x = 5 + 'x'").unwrap_err();
    assert!(
        e.contains("unsupported operand type(s) for +: 'int' and 'str'"),
        "got: {e}"
    );
}

/// Collection literals whose stack-slot count exceeds the `CallBuiltin` u8 argc
/// cap (a 174-key dict literal in a real script raised "too many arguments
/// (>255) for one call"). The compiler now builds them in ≤255-slot chunks via
/// the `EXTEND_*` ops; verify each container type is correct at and around the
/// chunk boundaries (list/tuple/set/str-parts spill at >255, dict pairs at
/// >127). Values checked against CPython.
#[test]
fn large_collection_literals_exceed_u8_argc() {
    // 300-element list (spills once past the 255 mk-chunk).
    let lst = (0..300)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        g(
            &format!("a = [{lst}]\nx = (len(a), sum(a), a[0], a[-1])"),
            "x"
        ),
        "(300, 44850, 0, 299)"
    );

    // 300-element tuple (EXTEND_TUPLE rebuilds each chunk).
    assert_eq!(
        g(&format!("a = ({lst},)\nx = (len(a), sum(a), a[-1])"), "x"),
        "(300, 44850, 299)"
    );

    // 300-key dict literal — 600 stack slots, dict pairs spill past 127.
    let pairs = (0..300)
        .map(|i| format!("{i}: {}", i * i))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        g(
            &format!("d = {{{pairs}}}\nx = (len(d), sum(d.values()), d[0], d[299])"),
            "x"
        ),
        "(300, 8955050, 0, 89401)"
    );

    // Set literal with cross-chunk duplicates -> deduped (EXTEND_SET keying).
    let st = (0..300)
        .map(|i| (i % 250).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        g(&format!("s = {{{st}}}\nx = (len(s), sum(s))"), "x"),
        "(250, 31125)"
    );

    // f-string with 300 replacement fields spills EXTEND_STR; `{0}{1}...` are
    // integer-literal fields, so the result is "012...299".
    let fields = (0..300)
        .map(|i| format!("{{{i}}}"))
        .collect::<Vec<_>>()
        .concat();
    let expected: String = (0..300).map(|i| i.to_string()).collect();
    assert_eq!(
        g(&format!("x = f\"{fields}\""), "x"),
        format!("'{expected}'")
    );

    // Boundaries: exactly at, just over, and dict at its 127/128 pair edge.
    for n in [255usize, 256, 127, 128, 254] {
        let seq = (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
        let want = (n, n * (n.saturating_sub(1)) / 2);
        assert_eq!(
            g(&format!("a = [{seq}]\nx = (len(a), sum(a))"), "x"),
            format!("({}, {})", want.0, want.1),
            "list n={n}"
        );
        let dp = (0..n)
            .map(|i| format!("{i}: {i}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            g(&format!("d = {{{dp}}}\nx = (len(d), sum(d.values()))"), "x"),
            format!("({}, {})", want.0, want.1),
            "dict n={n}"
        );
    }
}

/// Attribute access directly on a float literal: `0.1.is_integer()` must lex as
/// `0.1` then `.is_integer` (a second `.` after the decimal point ends the
/// literal), not consume the dot into a malformed float. Regression for a
/// `SyntaxError: bad float` the lexer raised on this CPython-valid form.
#[test]
fn float_literal_attribute_access() {
    assert_eq!(g("x = 0.1.is_integer()", "x"), "False");
    assert_eq!(g("x = 2.0.is_integer()", "x"), "True");
    assert_eq!(g("x = 3.14.hex()", "x"), g("y = (3.14).hex()", "y"));
    // A float from an exponent also ends before a following dot.
    assert_eq!(g("x = 1e3.is_integer()", "x"), "True");
}

/// `type(x)` for values whose type is not a constructor builtin still reprs as
/// `<class '…'>`, not `<built-in function …>`. Regression: `type(None)` and
/// `type(len)` reported as built-in functions.
#[test]
fn type_object_repr() {
    assert_eq!(g("x = type(None)", "x"), "<class 'NoneType'>");
    assert_eq!(
        g("x = type(len)", "x"),
        "<class 'builtin_function_or_method'>"
    );
    assert_eq!(g("x = type(lambda: 0)", "x"), "<class 'function'>");
    assert_eq!(g("x = type(3)", "x"), "<class 'int'>");
    assert_eq!(g("x = type(int)", "x"), "<class 'type'>");
    assert_eq!(
        g("x = type(NotImplemented)", "x"),
        "<class 'NotImplementedType'>"
    );
    // A callable builtin still reprs as a function, not a class.
    assert_eq!(g("x = len", "x"), "<built-in function len>");
}

/// `sum()` uses Neumaier compensated summation for floats (CPython 3.12+), so
/// `sum([0.1]*10)` is exactly `1.0`, not `0.9999999999999999`. Also verifies the
/// exact integer prefix, mixed int/float, complex tail, and the str-start guard.
#[test]
fn sum_neumaier_and_paths() {
    assert_eq!(g("x = sum([0.1]*10)", "x"), "1.0");
    assert_eq!(g("x = sum([1e18, 1, -1e18])", "x"), "1.0");
    assert_eq!(g("x = sum([1, 2, 3])", "x"), "6");
    assert_eq!(g("x = sum([1, 2, 3.5])", "x"), "6.5");
    assert_eq!(g("x = sum([2**70, 1])", "x"), "1180591620717411303425");
    assert_eq!(g("x = sum([1, 2, complex(1, 1)])", "x"), "(4+1j)");
    let e = eval_str("x = sum(['a', 'b'], '')").unwrap_err();
    assert!(
        e.contains("sum() can't sum strings [use ''.join(seq) instead]"),
        "got: {e}"
    );
}

/// Non-finite floats format lowercase (`nan`/`inf`) for `f`/`e`/`g`/`%` and
/// uppercase (`NAN`/`INF`) for `F`/`E`/`G`, and still flow through width/sign/
/// zero-fill. Regression: `{nan:.2f}` rendered Rust's `NaN`.
#[test]
fn nonfinite_float_format() {
    assert_eq!(g("x = f'{float(\"nan\"):.2f}'", "x"), "'nan'");
    assert_eq!(g("x = f'{float(\"inf\"):f}'", "x"), "'inf'");
    assert_eq!(g("x = f'{float(\"-inf\"):.1f}'", "x"), "'-inf'");
    assert_eq!(g("x = f'{float(\"nan\"):.2F}'", "x"), "'NAN'");
    assert_eq!(g("x = f'{float(\"inf\"):E}'", "x"), "'INF'");
    assert_eq!(g("x = f'{float(\"nan\"):+g}'", "x"), "'+nan'");
    assert_eq!(g("x = f'{float(\"inf\"):%}'", "x"), "'inf%'");
    // Non-finite still honors width and zero-fill (CPython `00000inf`).
    assert_eq!(g("x = f'{float(\"inf\"):08.2f}'", "x"), "'00000inf'");
    assert_eq!(g("x = f'{float(\"nan\"):>8}'", "x"), "'     nan'");
    // …but the zero-fill is a plain block: `inf` has no DIGITS, so
    // `calc_number_widths` short-circuits grouping and the separators never
    // appear. CPython 3.14.6: `format(float('inf'), '012,f')` == '000000000inf'.
    assert_eq!(
        g("x = format(float('inf'), '012,f')", "x"),
        "'000000000inf'"
    );
    assert_eq!(
        g("x = format(float('nan'), '012,f')", "x"),
        "'000000000nan'"
    );
    assert_eq!(
        g("x = format(float('-inf'), '012,f')", "x"),
        "'-00000000inf'"
    );
    assert_eq!(
        g("x = format(float('inf'), '012,g')", "x"),
        "'000000000inf'"
    );
    // `#` on a non-finite adds nothing — there is no fraction to point at.
    assert_eq!(g("x = format(float('inf'), '#.0f')", "x"), "'inf'");
}

/// The `n` presentation type: `d` for an int, `g` for a float, and BOTH read
/// their separator, group widths and decimal point from the process locale
/// (`LT_CURRENT_LOCALE` in `Python/formatter_unicode.c`).
///
/// The tests below pin only the locale-independent half. `LC_NUMERIC` is
/// whatever the test runner inherited and the `C` locale groups nothing, so
/// asserting `1.234.567` here would pass or fail by machine; the locale-varying
/// side is measured by the differential sweep against `python3` instead.
/// Values from CPython 3.14.6 under `LC_ALL=C`.
#[test]
fn locale_aware_n_presentation_type() {
    // Float `n` is `g`, NOT `str()`: at the default precision of 6 a
    // seven-digit value goes scientific. This was the whole bug — `n` fell
    // through to the no-type arm and printed the repr.
    assert_eq!(g("x = format(1234567.891, 'n')", "x"), "'1.23457e+06'");
    assert_eq!(g("x = format(123456.7, 'n')", "x"), "'123457'");
    assert_eq!(g("x = format(100.0, 'n')", "x"), "'100'");
    assert_eq!(g("x = format(3.14159265358979, 'n')", "x"), "'3.14159'");
    assert_eq!(g("x = format(1234567.891, '.10n')", "x"), "'1234567.891'");
    // Int `n` is `d`, and a `bool` goes through the int path like every other
    // integer spec (`format(True, 'd')` is already `'1'`).
    assert_eq!(g("x = format(1234567, 'n')", "x"), "'1234567'");
    assert_eq!(g("x = format(True, 'n')", "x"), "'1'");
    assert_eq!(g("x = format(-1234567, 'n')", "x"), "'-1234567'");
    // Width/align/sign all still apply.
    assert_eq!(g("x = format(1234567, '>12n')", "x"), "'     1234567'");
    assert_eq!(g("x = format(1234567, '+n')", "x"), "'+1234567'");
    // `n` brings its own separator from the locale, so asking for a second one
    // is rejected — for BOTH spellings, and a precision is still illegal on an
    // integer.
    assert_eq!(
        pythonrs::eval_str("x = format(1234, ',n')").unwrap_err(),
        "ValueError: Cannot specify ',' with 'n'."
    );
    assert_eq!(
        pythonrs::eval_str("x = format(1234, '_n')").unwrap_err(),
        "ValueError: Cannot specify '_' with 'n'."
    );
    assert_eq!(
        pythonrs::eval_str("x = format(1234, '.2n')").unwrap_err(),
        "ValueError: Precision not allowed in integer format specifier"
    );
    // A float with a precision and `n` is fine — the same spec, judged by the
    // value's type rather than by the spec alone.
    assert_eq!(g("x = format(1.5, '.2n')", "x"), "'1.5'");
}

/// `#` on a FLOAT conversion is `Py_DTSF_ALT`: the decimal point survives even
/// when the precision rounded every fractional digit away, and `g` keeps its
/// trailing zeros. Values from CPython 3.14.6.
#[test]
fn alternate_form_keeps_the_decimal_point() {
    assert_eq!(g("x = format(1.0, '#.0f')", "x"), "'1.'");
    assert_eq!(g("x = format(1.0, '#.0e')", "x"), "'1.e+00'");
    assert_eq!(g("x = format(1.0, '#.0%')", "x"), "'100.%'");
    assert_eq!(g("x = format(1.0, '#.0g')", "x"), "'1.'");
    assert_eq!(g("x = format(255.0, '#.0E')", "x"), "'3.E+02'");
    assert_eq!(g("x = format(2.0, '#.3')", "x"), "'2.00'");
    assert_eq!(g("x = format(2.0, '#.1')", "x"), "'2.e+00'");
    // An INT with a float presentation type takes the same path…
    assert_eq!(g("x = format(1, '#.0f')", "x"), "'1.'");
    // …but `#` on an int-rendered type is either the radix prefix or nothing.
    assert_eq!(g("x = format(255, '#x')", "x"), "'0xff'");
    assert_eq!(g("x = format(1234, '#n')", "x"), "'1234'");
    assert_eq!(g("x = format(1234, '#d')", "x"), "'1234'");
    // `g` at zero must not short-circuit: the sign of `-0.0` and the `#` flag
    // both survive. Regression — `fmt_g` returned a bare `"0"` for any zero.
    assert_eq!(g("x = format(0.0, '#g')", "x"), "'0.00000'");
    assert_eq!(g("x = format(-0.0, 'g')", "x"), "'-0'");
    assert_eq!(g("x = format(-0.0, '#g')", "x"), "'-0.00000'");
    assert_eq!(g("x = format(-0.0, '.3g')", "x"), "'-0'");
    // The printf-style path shares the rule.
    assert_eq!(g("x = '%#.0e' % 1.0", "x"), "'1.e+00'");
    assert_eq!(g("x = '%#.0f' % 1.0", "x"), "'1.'");
}

/// A bignum reaching a FLOAT presentation type converts through `f64` like
/// CPython's `PyNumber_Float`, and raises when the magnitude is past `f64`.
/// Before this, `as_f` returned `None` for a bignum and the `.unwrap_or(0.0)`
/// printed a silent zero. Values from CPython 3.14.6.
#[test]
fn bignum_through_a_float_presentation_type() {
    assert_eq!(
        g("x = format(10**20, 'f')", "x"),
        "'100000000000000000000.000000'"
    );
    assert_eq!(g("x = format(10**20, '.3e')", "x"), "'1.000e+20'");
    assert_eq!(g("x = format(10**20, 'g')", "x"), "'1e+20'");
    assert_eq!(
        g("x = format(-(10**25), '.2f')", "x"),
        "'-10000000000000000905969664.00'"
    );
    assert_eq!(
        pythonrs::eval_str("x = format(10**400, 'f')").unwrap_err(),
        "OverflowError: int too large to convert to float"
    );
    // `%d` of a float is a truncation, and it must stay exact past `i64` and
    // raise on a non-finite rather than saturating.
    assert_eq!(
        g("x = '%d' % 1e30", "x"),
        "'1000000000000000019884624838656'"
    );
    assert_eq!(
        pythonrs::eval_str("x = '%d' % float('inf')").unwrap_err(),
        "OverflowError: cannot convert float infinity to integer"
    );
    assert_eq!(
        pythonrs::eval_str("x = '%d' % float('nan')").unwrap_err(),
        "ValueError: cannot convert float NaN to integer"
    );
    assert_eq!(
        pythonrs::eval_str("x = '%c' % (10**20)").unwrap_err(),
        "OverflowError: %c arg not in range(0x110000)"
    );
}

/// A builtin exception class is a type object, so `repr(ValueError)` is
/// `<class 'ValueError'>`, not `<built-in function ValueError>`.
#[test]
fn exception_class_repr() {
    assert_eq!(g("x = ValueError", "x"), "<class 'ValueError'>");
    assert_eq!(g("x = KeyError", "x"), "<class 'KeyError'>");
    assert_eq!(g("x = Exception", "x"), "<class 'Exception'>");
    assert_eq!(g("x = type(ValueError)", "x"), "<class 'type'>");
}

/// The `...` (`Ellipsis`) singleton is a distinct truthy object of type
/// `ellipsis`, never `None`: `... is ...`, `... == ...`, `... is not None`,
/// hashable (usable as a dict/set key), and repr/str `Ellipsis`. The bare name
/// `Ellipsis` resolves to the same singleton.
#[test]
fn ellipsis_singleton() {
    assert_eq!(g("x = ...", "x"), "Ellipsis");
    assert_eq!(g("x = type(...).__name__", "x"), "'ellipsis'");
    assert_eq!(g("x = (... is ...)", "x"), "True");
    assert_eq!(g("x = (... == ...)", "x"), "True");
    assert_eq!(g("x = (... is None)", "x"), "False");
    assert_eq!(g("x = (... == None)", "x"), "False");
    assert_eq!(g("x = bool(...)", "x"), "True");
    assert_eq!(g("x = (... is Ellipsis)", "x"), "True");
    // Hashable: works as a dict key and dedupes in a set.
    assert_eq!(g("x = {...: 'e'}[...]", "x"), "'e'");
    assert_eq!(g("x = len({..., ..., None})", "x"), "2");
    // Equality drives `count`.
    assert_eq!(g("x = [..., 1, ...].count(...)", "x"), "2");
}

/// A builtin exception instance exposes `__class__` as its type object, so
/// `e.__class__ is ValueError`, `e.__class__.__name__`, and the `__cause__`/
/// `__context__`/`__suppress_context__` chaining attributes all resolve.
#[test]
fn exception_class_and_chain_attrs() {
    assert_eq!(
        g("x = ValueError('x').__class__.__name__", "x"),
        "'ValueError'"
    );
    assert_eq!(
        g("x = (ValueError('x').__class__ is ValueError)", "x"),
        "True"
    );
    assert_eq!(
        g(
            "try:\n try: int('x')\n except ValueError: raise RuntimeError('a') from None\nexcept RuntimeError as e:\n x = (e.__suppress_context__, e.__cause__, type(e.__context__).__name__)",
            "x"
        ),
        "(True, None, 'ValueError')"
    );
    assert_eq!(
        g(
            "try:\n try: int('x')\n except ValueError: raise RuntimeError('b')\nexcept RuntimeError as e:\n x = e.__suppress_context__",
            "x"
        ),
        "False"
    );
}

/// Unbound builtin methods reached via a type object: `str.lower`, `list.append`,
/// `dict.get`. Callable with an explicit receiver (`str.lower("HI")`), usable as
/// a `key=`/`map` function, and repr as `<method '…' of '…' objects>`. Also the
/// bound-method `__name__`.
#[test]
fn unbound_builtin_methods() {
    assert_eq!(g("x = str.lower('HELLO')", "x"), "'hello'");
    assert_eq!(
        g("x = sorted(['B', 'a', 'C'], key=str.lower)", "x"),
        "['a', 'B', 'C']"
    );
    assert_eq!(g("x = list(map(str.upper, ['a', 'b']))", "x"), "['A', 'B']");
    assert_eq!(g("x = list.count([1, 1, 2], 1)", "x"), "2");
    assert_eq!(g("x = dict.get({'a': 1}, 'a')", "x"), "1");
    assert_eq!(g("x = str.upper", "x"), "<method 'upper' of 'str' objects>");
    // A bad attribute on a type object is still an AttributeError.
    assert!(eval_str("x = str.nonesuch").is_err());
    // Bound builtin method dunders.
    assert_eq!(g("x = [].append.__name__", "x"), "'append'");
    assert_eq!(g("x = [].append.__qualname__", "x"), "'list.append'");
}

/// The `__import__` builtin imports a module by name (native `sys` here, so the
/// test holds without the FFI bridge). An empty `fromlist` with a dotted name
/// returns the top-level package.
#[test]
fn dunder_import_builtin() {
    assert_eq!(g("x = __import__('sys').maxsize > 0", "x"), "True");
    assert_eq!(g("m = __import__('math')\nx = m.floor(3.7)", "x"), "3");
    // Dotted name, empty fromlist -> top package name.
    assert_eq!(
        g(
            "x = __import__('sys').__name__ if hasattr(__import__('sys'), '__name__') else 'sys'",
            "x"
        ),
        "'sys'"
    );
}

/// A class body captures its simple annotations into `__annotations__` (so
/// `@dataclass`/`typing.NamedTuple` and `Cls.__annotations__` see the fields);
/// an annotated assignment still binds the value, and a nested function's local
/// annotation does not leak into the class dict.
#[test]
fn class_body_annotations() {
    assert_eq!(
        g(
            "class C:\n    x: int\n    y: str = 'hi'\nz = C.__annotations__",
            "z"
        ),
        "{'x': <class 'int'>, 'y': <class 'str'>}"
    );
    assert_eq!(g("class C:\n    y: str = 'hi'\nv = C.y", "v"), "'hi'");
    // A forward-reference string annotation is stored verbatim.
    assert_eq!(
        g("class C:\n    a: 'Later'\nz = C.__annotations__['a']", "z"),
        "'Later'"
    );
    // A method-local annotation is not recorded in the class's __annotations__.
    assert_eq!(
        g("class C:\n    x: int\n    def m(self):\n        y: int = 1\n        return y\nz = sorted(C.__annotations__)", "z"),
        "['x']"
    );
}

/// `def` parameter/return annotations build the function's `__annotations__`
/// dict at def time (keys in source order, `"return"` last), matching CPython.
#[test]
fn function_annotations() {
    // Positional, keyword-only, `*args`/`**kwargs`, and return, in source order.
    assert_eq!(
        g(
            "def f(a: int, b: 'str', *args: float, c: bool = True, **kw: bytes) -> list:\n    return a\nz = f.__annotations__",
            "z",
        ),
        "{'a': <class 'int'>, 'b': 'str', 'args': <class 'float'>, 'c': <class 'bool'>, 'kw': <class 'bytes'>, 'return': <class 'list'>}",
    );
    // An unannotated function has an empty (but real, mutable) dict.
    assert_eq!(
        g("def g(x, y):\n    return x\nz = g.__annotations__", "z"),
        "{}"
    );
    // A method's annotations are reachable both unbound (`C.m`) and bound (`c.m`).
    assert_eq!(
        g("class C:\n    def m(self, n: int) -> 'C':\n        return self\nz = C.m.__annotations__", "z"),
        "{'n': <class 'int'>, 'return': 'C'}",
    );
    assert_eq!(
        g("class C:\n    def m(self, n: int) -> 'C':\n        return self\nz = C().m.__annotations__", "z"),
        "{'n': <class 'int'>, 'return': 'C'}",
    );
    // The dict is live: annotations can be introspected and mutated.
    assert_eq!(
        g("def f(x: int) -> str:\n    return ''\nf.__annotations__['x'] = 99\nz = f.__annotations__['x']", "z"),
        "99",
    );
}

/// Native `copy.copy`/`copy.deepcopy` (routing through CPython would deep-copy by
/// value, losing shallow sharing and instance identity). Shallow shares nested
/// refs; deep is independent and preserves shared/cyclic references.
#[test]
fn copy_module_native() {
    // Shallow copy shares the nested list.
    assert_eq!(
        g(
            "import copy\na=[1,[2]]\nb=copy.copy(a)\na[1].append(3)\nx=(b[1], a[1] is b[1])",
            "x"
        ),
        "([2, 3], True)"
    );
    // Deep copy is independent.
    assert_eq!(
        g(
            "import copy\na=[1,[2]]\nb=copy.deepcopy(a)\na[1].append(3)\nx=b[1]",
            "x"
        ),
        "[2]"
    );
    // Deepcopy preserves shared references (one copied object, referenced twice).
    assert_eq!(
        g(
            "import copy\ns=[0]\ny=copy.deepcopy([s, s])\nx=y[0] is y[1]",
            "x"
        ),
        "True"
    );
    // Deepcopy of an instance copies its attributes independently.
    assert_eq!(
        g("import copy\nclass N:\n    def __init__(s,v):\n        s.v=v\nn=N([1])\nm=copy.deepcopy(n)\nn.v.append(2)\nx=m.v", "x"),
        "[1]"
    );
}

#[test]
fn t_strings_build_templates_not_strings() {
    // PEP 750: a `t"..."` literal evaluates to a `string.templatelib.Template`,
    // and its fields are NOT formatted — the consumer decides. `annotationlib`
    // does `_Template = type(t"")`, so `inspect` and `dataclasses` transitively
    // depend on the literal existing at all.
    assert_eq!(
        g("name = 'world'\nt = t'Hello {name}!'\nx = t.strings", "x"),
        "('Hello ', '!')"
    );
    assert_eq!(
        g("name = 'world'\nt = t'Hello {name}!'\nx = t.values", "x"),
        "('world',)"
    );
    // An interpolation keeps the SOURCE text of its expression, its conversion,
    // and its format spec — none of which survive f-string formatting.
    assert_eq!(
        g("n = 3\ni = t'{n + 1!r:>5}'.interpolations[0]\nx = (i.value, i.expression, i.conversion, i.format_spec)", "x"),
        "(4, 'n + 1', 'r', '>5')"
    );
    // Iteration interleaves literals and interpolations, skipping empty pieces.
    assert_eq!(
        g("x = [type(p).__name__ for p in t'{1}{2}']", "x"),
        "['Interpolation', 'Interpolation']"
    );
    assert_eq!(g("x = t'{1}{2}'.strings", "x"), "('', '', '')");
    // Concatenation joins at the seam and keeps strings == interpolations + 1.
    assert_eq!(
        g("t = t'a{1}' + t'b{2}'\nx = (t.strings, t.values)", "x"),
        "(('a', 'b', ''), (1, 2))"
    );
    // An empty template still has one static piece.
    assert_eq!(g("x = (t''.strings, t''.values)", "x"), "(('',), ())");
    // A t-string is a distinct type from a str.
    assert_eq!(
        g("x = type(t'').__module__ + '.' + type(t'').__name__", "x"),
        "'string.templatelib.Template'"
    );
}

#[test]
fn t_string_and_f_string_literals_cannot_be_concatenated() {
    // Adjacent literals concatenate only within one type. A t-string joins other
    // t-strings and nothing else; bytes join bytes and nothing else. The pieces
    // produce different types, so there is nothing to concatenate and CPython
    // rejects the group rather than silently picking one half.
    //
    // Asserting only `contains("SyntaxError")` here would pass on ANY syntax
    // error, including one raised for an unrelated reason and one carrying a
    // message CPython never produced — this test read that way while the message
    // said "cannot mix t-string and f-string literals", which no CPython emits.
    // Each expectation below is byte-checked against `python3 -c` on 3.14.6.
    const T_MIX: &str = "SyntaxError: cannot mix t-string literals with string or bytes literals";
    const B_MIX: &str = "SyntaxError: cannot mix bytes and nonbytes literals";
    let rejected = [
        ("x = t'a' f'b'", T_MIX),
        ("x = f'a' t'b'", T_MIX),
        ("x = t'a' 'b'", T_MIX),
        ("x = 'a' t'b'", T_MIX),
        // A t-string beside bytes is reported as the t-string error, not the
        // bytes one — CPython checks in that order.
        ("x = t'a' b'b'", T_MIX),
        ("x = 'a' b'b'", B_MIX),
        ("x = b'a' 'b'", B_MIX),
        ("x = f'a' b'b'", B_MIX),
        ("x = b'a' f'b'", B_MIX),
    ];
    for (src, want) in rejected {
        let e = eval_str(src).expect_err("must be rejected");
        assert_eq!(e, want, "for {src}");
    }
    // …and the same-type groups still concatenate. Before the check above
    // existed, `'a' b'b'` produced `b'b'` — the text half was silently dropped —
    // so the accepted cases are pinned by VALUE, not merely by "did not raise".
    assert_eq!(g("x = 'a' 'b'", "x"), "'ab'");
    assert_eq!(g("x = b'a' b'b'", "x"), "b'ab'");
    assert_eq!(g("x = f'a{1}' f'b'", "x"), "'a1b'");
    assert_eq!(g("x = f'a' 'b'", "x"), "'ab'");
    assert_eq!(
        g("x = repr(t'a' t'b')", "x"),
        "\"Template(strings=('ab',), interpolations=())\""
    );
}

#[test]
fn attribute_lookup_through_a_deep_class_chain_is_not_quadratic_work() {
    // `mro_of` is on the path of every attribute read, method dispatch and
    // `isinstance`. It used to re-run the full C3 linearization per call —
    // recursing into each base and allocating a fresh name vector at every level
    // — so one `obj.attr` through a 21-deep chain cost ~45us. Memoized, the same
    // program runs ~47x faster; this test pins the behavior that made it correct
    // to cache: a class registered later must still be visible.
    assert_eq!(
        g(
            "C = type('B0', (), {'v': 1})\n\
             for k in range(20):\n\
             \x20   C = type('B%d' % (k + 1), (C,), {})\n\
             o = C()\n\
             x = sum(o.v for _ in range(50))",
            "x"
        ),
        "50"
    );
    // Registering a new class after a lookup must invalidate the memo: `Sub`
    // resolves `v` through `Base`, and redefining `Base` changes the answer.
    assert_eq!(
        g(
            "class Base:\n\
             \x20   v = 1\n\
             class Sub(Base):\n\
             \x20   pass\n\
             first = Sub().v\n\
             class Other(Sub):\n\
             \x20   v = 2\n\
             x = (first, Other().v, Sub().v)",
            "x"
        ),
        "(1, 2, 1)"
    );
    // Diamond inheritance still linearizes by C3, not by depth-first order.
    assert_eq!(
        g(
            "class A: pass\n\
             class B(A): pass\n\
             class C(A): pass\n\
             class D(B, C): pass\n\
             x = [c.__name__ for c in D.__mro__]",
            "x"
        ),
        "['D', 'B', 'C', 'A', 'object']"
    );
}

#[test]
fn function_locals_live_in_frame_slots_without_changing_semantics() {
    // An eligible function keeps its locals in `Vec<Value>` slots instead of
    // hashing each name against the environment chain. The observable behavior
    // must be identical — including the parts that make slotting hard.
    //
    // A read before any assignment is still `UnboundLocalError`, not `None`:
    // an unassigned slot holds a marker distinct from Python's `None`.
    assert_eq!(
        g(
            "def f():\n\
             \x20   try:\n\
             \x20       print(x)\n\
             \x20   except UnboundLocalError:\n\
             \x20       return 'unbound'\n\
             \x20   x = 1\n\
             x = f()",
            "x"
        ),
        "'unbound'"
    );
    // A local bound only on one branch is still unbound on the other.
    assert_eq!(
        g(
            "def f(c):\n\
             \x20   if c:\n\
             \x20       y = 1\n\
             \x20   try:\n\
             \x20       return y\n\
             \x20   except UnboundLocalError:\n\
             \x20       return 'unbound'\n\
             x = (f(True), f(False))",
            "x"
        ),
        "(1, 'unbound')"
    );
    // A loop target is bound inside the body but not after an empty loop.
    assert_eq!(
        g(
            "def f(items):\n\
             \x20   for i in items:\n\
             \x20       pass\n\
             \x20   try:\n\
             \x20       return i\n\
             \x20   except UnboundLocalError:\n\
             \x20       return 'unbound'\n\
             x = (f([1, 2]), f([]))",
            "x"
        ),
        "(2, 'unbound')"
    );
    // A local that shadows a global does not leak into it.
    assert_eq!(
        g(
            "v = 'global'\ndef f():\n\x20   v = 'local'\n\x20   return v\nx = (f(), v)",
            "x"
        ),
        "('local', 'global')"
    );
    // A closure still sees the enclosing local, so names a nested scope reads
    // must stay name-resolved rather than moving into a slot.
    assert_eq!(
        g(
            "def outer():\n\
             \x20   a = 10\n\
             \x20   def inner():\n\
             \x20       return a + 1\n\
             \x20   a = 20\n\
             \x20   return inner()\n\
             x = outer()",
            "x"
        ),
        "21"
    );
    // Recursion gives each activation its own slots.
    assert_eq!(
        g(
            "def fact(n):\n\
             \x20   if n <= 1:\n\
             \x20       return 1\n\
             \x20   acc = n * fact(n - 1)\n\
             \x20   return acc\n\
             x = fact(10)",
            "x"
        ),
        "3628800"
    );
    // Parameters reach the slots, including defaults, `*args` and `**kwargs`.
    assert_eq!(
        g(
            "def f(a, b=2, *rest, c=3, **kw):\n\
             \x20   total = a + b + c + len(rest) + len(kw)\n\
             \x20   return total\n\
             x = (f(1), f(1, 5, 9, 9, c=0, z=1))",
            "x"
        ),
        "(6, 9)"
    );
    // The same loop with a VARIABLE bound must produce the same answer as a
    // literal one — this is the shape that used to fall off the fast path.
    assert_eq!(
        g(
            "def lit():\n\
             \x20   s = 0\n\
             \x20   for i in range(1000): s += i * 3 - 1\n\
             \x20   return s\n\
             def var(n):\n\
             \x20   s = 0\n\
             \x20   for i in range(n): s += i * 3 - 1\n\
             \x20   return s\n\
             x = (lit(), var(1000), lit() == var(1000))",
            "x"
        ),
        "(1497500, 1497500, True)"
    );
}

#[test]
fn dir_lists_builtin_type_methods_and_scope_names() {
    // `dir(type)` and `dir(value)` both list the methods the type responds to,
    // so a `"append" in dir(list)` capability check works.
    assert_eq!(
        g(
            "x = ('append' in dir(list), 'upper' in dir(str), 'upper' in dir('a'))",
            "x"
        ),
        "(True, True, True)"
    );
    // Bare `dir()` is the names bound in the current scope: module globals at
    // module level, the frame's locals inside a function. The function case is
    // what forced `dir` into the slot-optimization opt-out — with locals in
    // fusevm slots there is no namespace left to enumerate.
    assert_eq!(g("q = 1\nx = 'q' in dir()", "x"), "True");
    assert_eq!(
        g(
            "def f():\n\
             \x20   inner = 1\n\
             \x20   return sorted(dir())\n\
             x = f()",
            "x"
        ),
        "['inner']"
    );
}

#[test]
fn vars_with_no_argument_is_locals() {
    // CPython's `vars()` == `locals()`; it used to return an empty dict.
    assert_eq!(g("q = 1\nx = 'q' in vars()", "x"), "True");
    assert_eq!(
        g(
            "def f():\n\
             \x20   inner = 2\n\
             \x20   return sorted(vars())\n\
             x = f()",
            "x"
        ),
        "['inner']"
    );
}

#[test]
fn str_maketrans_is_reachable_off_an_instance() {
    // `str.maketrans` is a staticmethod, so an instance reaches it too and gets
    // the same table (the receiver string is ignored).
    assert_eq!(
        g(
            "x = 'abc'.maketrans('ab', 'xy') == str.maketrans('ab', 'xy')",
            "x"
        ),
        "True"
    );
    assert_eq!(
        g("x = 'abc'.translate('zzz'.maketrans('ab', 'xy'))", "x"),
        "'xyc'"
    );
}

#[test]
fn break_continue_return_cross_a_try_boundary() {
    // A try body compiles to its own chunk, so `break`/`continue`/`return`
    // inside it have to propagate out to the ENCLOSING loop/frame rather than
    // just ending the chunk. Sibling fusevm frontends have shipped a compiler
    // panic (break to an outer loop) and an infinite loop (return from inside a
    // try inside a loop) on exactly these shapes; each assertion below is the
    // value CPython 3.14 produces.
    assert_eq!(
        g(
            "def f():\n\
             \x20   for i in range(5):\n\
             \x20       try:\n\
             \x20           if i == 2: break\n\
             \x20       finally:\n\
             \x20           pass\n\
             \x20   return i\n\
             x = f()",
            "x"
        ),
        "2"
    );
    // `continue` from inside a try, with a `finally` that must still run.
    assert_eq!(
        g(
            "def f():\n\
             \x20   out = []\n\
             \x20   for i in range(4):\n\
             \x20       try:\n\
             \x20           if i % 2 == 0: continue\n\
             \x20           out.append(i)\n\
             \x20       finally:\n\
             \x20           out.append(-i)\n\
             \x20   return out\n\
             x = f()",
            "x"
        ),
        "[0, 1, -1, -2, 3, -3]"
    );
    // `return` from inside a try inside a loop must end the FRAME, not spin.
    assert_eq!(
        g(
            "def f():\n\
             \x20   i = 0\n\
             \x20   while True:\n\
             \x20       try:\n\
             \x20           i += 1\n\
             \x20           if i > 2: return i\n\
             \x20       except ValueError:\n\
             \x20           pass\n\
             x = f()",
            "x"
        ),
        "3"
    );
    // `break` from an inner try targets the INNER loop only; the outer keeps going.
    assert_eq!(
        g(
            "def f():\n\
             \x20   out = []\n\
             \x20   for i in range(3):\n\
             \x20       for j in range(3):\n\
             \x20           try:\n\
             \x20               if j == 1: break\n\
             \x20               out.append((i, j))\n\
             \x20           except Exception:\n\
             \x20               pass\n\
             \x20   return out\n\
             x = f()",
            "x"
        ),
        "[(0, 0), (1, 0), (2, 0)]"
    );
    // `break` inside an `except` handler, and a generator that breaks out of a try.
    assert_eq!(
        g(
            "def f():\n\
             \x20   for i in range(4):\n\
             \x20       try:\n\
             \x20           raise ValueError(i)\n\
             \x20       except ValueError:\n\
             \x20           if i == 2: break\n\
             \x20   return i\n\
             def gen():\n\
             \x20   for i in range(4):\n\
             \x20       try:\n\
             \x20           if i == 2: break\n\
             \x20           yield i\n\
             \x20       finally:\n\
             \x20           pass\n\
             x = (f(), list(gen()))",
            "x"
        ),
        "(2, [0, 1])"
    );
}

/// PEP 634 forbids a pattern that matches everything anywhere another pattern
/// could still be reached. pythonrs used to accept every one of these and run
/// the dead branches. Ported from CPython's `pattern_context.allow_irrefutable`
/// (compile.c): false for a case that is neither last nor guarded, and for every
/// OR alternative but the last; inherited through `p as n`; reset to true inside
/// a sequence/mapping/class sub-pattern, where the enclosing pattern is what can
/// fail. Messages are CPython's verbatim.
#[test]
fn an_irrefutable_pattern_may_not_shadow_a_later_one() {
    let rejected = [
        // A case that matches everything, above another case.
        ("match 1:\n case _:\n  pass\n case 1:\n  pass", "wildcard"),
        (
            "match 1:\n case y:\n  pass\n case 1:\n  pass",
            "name capture 'y'",
        ),
        // `_ as z` is exactly as irrefutable as `_`.
        (
            "match 1:\n case _ as z:\n  pass\n case 1:\n  pass",
            "wildcard",
        ),
        // An OR alternative that is not the last one.
        ("match 1:\n case _ | 1:\n  pass", "wildcard"),
        ("match 1:\n case 1 | _ | 2:\n  pass", "wildcard"),
        ("match 1:\n case y | [x]:\n  pass", "name capture 'y'"),
        ("match 1:\n case y | y:\n  pass", "name capture 'y'"),
        // Nested: the sub-pattern context allows one, so only the non-last
        // alternative inside the sequence is rejected.
        ("match 1:\n case [x | _]:\n  pass", "name capture 'x'"),
        // The or-pattern is refutable on its own, but a following case removes
        // the position that permitted its trailing capture.
        (
            "match 1:\n case [x] | y:\n  pass\n case _:\n  pass",
            "name capture 'y'",
        ),
        (
            "match 1:\n case (1 | y) as z:\n  pass\n case 1:\n  pass",
            "name capture 'y'",
        ),
    ];
    for (src, needle) in rejected {
        let e = eval_str(src).expect_err(src);
        assert!(
            e.starts_with("SyntaxError:") && e.contains(needle) && e.contains("unreachable"),
            "{src}\n  expected a SyntaxError naming {needle}, got: {e}"
        );
    }
    let accepted = [
        // Last case, so nothing follows it.
        "match 1:\n case 1:\n  pass\n case y:\n  pass",
        "match 1:\n case 1 | _:\n  pass",
        // A guard can still fall through to the next case.
        "match 1:\n case _ if 0:\n  pass\n case 1:\n  pass",
        "match 1:\n case y if 0:\n  pass\n case 1:\n  pass",
        // Refutable containers: the capture inside cannot shadow anything.
        "match 1:\n case [x]:\n  pass\n case 1:\n  pass",
        "match 1:\n case [_]:\n  pass\n case 1:\n  pass",
        "match 1:\n case [*rest]:\n  pass\n case 1:\n  pass",
        "match 1:\n case {}:\n  pass\n case 1:\n  pass",
        "match 1:\n case {'a': y}:\n  pass\n case 1:\n  pass",
        "match 1:\n case {**rest}:\n  pass\n case 1:\n  pass",
        "match 1:\n case 1 as z:\n  pass\n case 1:\n  pass",
    ];
    for src in accepted {
        if let Err(e) = eval_str(src) {
            panic!("{src}\n  must be accepted, got: {e}");
        }
    }
}

/// A tokenizer error must not pre-empt a parse error on an EARLIER line.
///
/// CPython pulls tokens lazily, so `match -3:` with a non-`case` body reports
/// `SyntaxError: invalid syntax` at the body — even though the line AFTER it
/// dedents to a column that matches no open block. pythonrs tokenizes the whole
/// module up front, so the later `IndentationError` used to win. Tokenizing now
/// stops at the bad dedent and parks the message until the parser has consumed
/// everything before it.
#[test]
fn a_bad_dedent_does_not_pre_empt_an_earlier_syntax_error() {
    // The parser rejects line 2 (`print` is not a `case`) before line 3's dedent
    // can matter.
    let e = eval_str("match -3:\n        print('bad')\n    case _:\n")
        .expect_err("a match body that is not a case must be rejected");
    assert!(
        e.starts_with("SyntaxError:") && !e.contains("IndentationError"),
        "expected the line-2 SyntaxError, got: {e}"
    );
    // With the body well-formed, the dedent IS the only problem and must still
    // be reported — the deferral must not swallow it.
    let e = eval_str("match -3:\n        case 1:\n            pass\n    case _:\n        pass\n")
        .expect_err("an unmatched dedent must still be rejected");
    assert!(
        e.starts_with("IndentationError: unindent does not match"),
        "expected the IndentationError, got: {e}"
    );
    // A bad dedent with no match statement anywhere behaves exactly as before.
    let e = eval_str("if 1:\n        x = 1\n    y = 2\n")
        .expect_err("an unmatched dedent must be rejected");
    assert!(
        e.starts_with("IndentationError: unindent does not match"),
        "expected the IndentationError, got: {e}"
    );
}

/// The builtin types `dir()` is expected to enumerate. Kept explicit so a new
/// entry in the dispatch tables has to be added here consciously.
#[cfg(test)]
const DIR_AUDIT_TYPES: &[&str] = &[
    "str",
    "bytes",
    "bytearray",
    "list",
    "dict",
    "set",
    "frozenset",
    "tuple",
    "range",
    "int",
    "float",
    "bool",
    "complex",
    "deque",
    "OrderedDict",
    "defaultdict",
    "Counter",
    "generator",
    "coroutine",
    "async_generator",
    "zip",
    "map",
    "filter",
    "enumerate",
    "iterator",
    "lock",
    "RLock",
    "Event",
    "Lock",
    "Queue",
    "Context",
    "property",
    "memoryview",
    "slice",
    // Exception types: `BaseException`'s two methods, plus PEP 654's group
    // protocol on the two group classes and NOT on any other exception.
    "ValueError",
    "BaseException",
    "ExceptionGroup",
    "BaseExceptionGroup",
];

/// `dir()` must never name something `getattr` cannot produce.
///
/// `dir()` used to read `type_method_names`, which is `None` for every type
/// whose method set is a RULE rather than a table — so `dir(int)` came back
/// EMPTY while `(5).bit_length` and `(5).__add__` dispatched fine. It now reads
/// `type_dir_names`, which reproduces those rules. This asserts the direction
/// that matters: a listed name is a name that dispatches.
#[test]
fn builtin_dir_lists_only_dispatchable_names() {
    let mut checked = 0usize;
    for t in DIR_AUDIT_TYPES {
        let listed = pythonrs::builtins::type_dir_names(t);
        // The regression this test exists for made `type_dir_names` return an
        // EMPTY list for a rule-driven type, which would satisfy the loop below
        // vacuously — the failure mode and the pass condition were the same
        // thing. Every audited type must name at least one attribute.
        assert!(
            !listed.is_empty(),
            "dir({t}) is empty — the rule that produces its names is not being read"
        );
        for n in listed {
            assert!(
                pythonrs::builtins::type_has_method(t, n),
                "dir({t}) lists {n}, which dispatch does not accept"
            );
            checked += 1;
        }
    }
    // A whole-surface floor, so a table that stopped being parsed cannot shrink
    // the audit silently. The measured total is 500+; the floor is deliberately
    // slack so adding or removing a method never fails this line.
    assert!(
        checked > 300,
        "only {checked} (type, name) pairs audited across {} types — the dispatch \
         tables are not being read",
        DIR_AUDIT_TYPES.len()
    );
}

/// And the other direction: dispatch must never accept a name `dir()` hides.
///
/// The candidate universe is every name any type is known to answer to, which
/// makes the check enumerable — a name that dispatch accepts for a type but
/// `type_dir_names` omits is an invisible attribute.
#[test]
fn builtin_dispatch_is_fully_listed_by_dir() {
    let universe: Vec<&'static str> = {
        let mut u: Vec<&'static str> = Vec::new();
        for t in DIR_AUDIT_TYPES {
            u.extend(pythonrs::builtins::type_dir_names(t));
        }
        u.sort_unstable();
        u.dedup();
        u
    };
    assert!(
        universe.len() > 100,
        "the candidate universe collapsed to {} names — the tables are not being read",
        universe.len()
    );
    for t in DIR_AUDIT_TYPES {
        let listed = pythonrs::builtins::type_dir_names(t);
        for n in &universe {
            if pythonrs::builtins::type_has_method(t, n) {
                assert!(
                    listed.contains(n),
                    "{t} dispatches {n}, but dir({t}) does not list it"
                );
            }
        }
    }
}

/// Compile-time syntax checks pythonrs used to skip, letting an invalid program
/// run. Each message is CPython 3.14.6's, byte-checked against `python3 -c`.
#[test]
fn invalid_programs_are_rejected_at_compile_time() {
    let cases = [
        // `return` outside a function ran silently and produced no error.
        ("return 1", "SyntaxError: 'return' outside function"),
        ("return", "SyntaxError: 'return' outside function"),
        (
            "if 1:\n    return 2",
            "SyntaxError: 'return' outside function",
        ),
        // A class body does not inherit the enclosing function's scope.
        (
            "class C:\n    return 1",
            "SyntaxError: 'return' outside function",
        ),
        (
            "def f():\n    class C:\n        return 1",
            "SyntaxError: 'return' outside function",
        ),
        // `yield` raised a runtime TypeError instead, so output could precede it.
        ("yield 1", "SyntaxError: 'yield' outside function"),
        ("x = [(yield 1)]", "SyntaxError: 'yield' outside function"),
        (
            "yield from [1]",
            "SyntaxError: 'yield from' outside function",
        ),
        // A `try` with neither handler nor cleanup parsed and ran its body.
        (
            "try:\n    pass",
            "SyntaxError: expected 'except' or 'finally' block",
        ),
        (
            "try:\n    pass\nelse:\n    pass",
            "SyntaxError: expected 'except' or 'finally' block",
        ),
        // Message wording: CPython names neither the keyword nor the decorator.
        ("continue", "SyntaxError: 'continue' not properly in loop"),
        ("@dec", "SyntaxError: invalid syntax"),
        ("@dec\nx = 1", "SyntaxError: invalid syntax"),
        ("x = else", "SyntaxError: invalid syntax"),
        ("print(pass)", "SyntaxError: invalid syntax"),
        (
            "except ValueError:\n    pass",
            "SyntaxError: invalid syntax",
        ),
        ("finally:\n    pass", "SyntaxError: invalid syntax"),
    ];
    for (src, want) in cases {
        assert_eq!(eval_str(src).expect_err(src), want, "for {src:?}");
    }
    // The legal forms of each construct still compile and run.
    let ok = [
        "def f():\n    return 1\nassert f() == 1",
        "def g():\n    yield 1\n    yield from [2, 3]\nassert list(g()) == [1, 2, 3]",
        "f = lambda a: a + 1\nassert f(1) == 2",
        "try:\n    pass\nexcept ValueError:\n    pass",
        "try:\n    pass\nfinally:\n    pass",
        "for i in [1, 2]:\n    continue",
        "class C:\n    xs = [i for i in range(3)]\nassert C.xs == [0, 1, 2]",
        "assert sum(i for i in range(4)) == 6",
        "def d(fn):\n    return fn\n@d\ndef h():\n    return 3\nassert h() == 3",
    ];
    for src in ok {
        if let Err(e) = eval_str(src) {
            panic!("{src:?} must compile, got: {e}");
        }
    }
}

/// PEP 654 exception groups: the object model and the group protocol
/// (`split`/`subgroup`/`derive`), each value byte-checked against CPython 3.14.6.
#[test]
fn exception_groups_split_and_derive() {
    // `str` counts members; `repr` shows the constructor arguments; `.message`
    // and `.exceptions` read them back (the latter always as a tuple).
    let setup = "eg = ExceptionGroup('g', [ValueError(1), TypeError(2)])\n";
    assert_eq!(
        g(&format!("{setup}x = str(eg)"), "x"),
        "'g (2 sub-exceptions)'"
    );
    assert_eq!(
        g("x = str(ExceptionGroup('one', [ValueError(1)]))", "x"),
        "'one (1 sub-exception)'"
    );
    assert_eq!(
        g(&format!("{setup}x = repr(eg)"), "x"),
        "\"ExceptionGroup('g', [ValueError(1), TypeError(2)])\""
    );
    assert_eq!(g(&format!("{setup}x = eg.message"), "x"), "'g'");
    assert_eq!(
        g(&format!("{setup}x = repr(eg.exceptions)"), "x"),
        "'(ValueError(1), TypeError(2))'"
    );
    // `ExceptionGroup` is BOTH an `Exception` and a `BaseExceptionGroup`; only
    // the second base needed adding, and neither may leak onto a plain
    // exception.
    assert_eq!(
        g(
            &format!("{setup}x = (isinstance(eg, Exception), isinstance(eg, BaseExceptionGroup))"),
            "x"
        ),
        "(True, True)"
    );
    assert_eq!(
        g("x = isinstance(ValueError(1), BaseExceptionGroup)", "x"),
        "False"
    );
    // `split` returns (match, rest); each half keeps the group's message and is
    // `None` when empty.
    assert_eq!(
        g(&format!("{setup}x = repr(eg.split(ValueError))"), "x"),
        "\"(ExceptionGroup('g', [ValueError(1)]), ExceptionGroup('g', [TypeError(2)]))\""
    );
    assert_eq!(
        g(&format!("{setup}x = repr(eg.split(KeyError))"), "x"),
        "\"(None, ExceptionGroup('g', [ValueError(1), TypeError(2)]))\""
    );
    // A nested group is rebuilt with its own nesting on BOTH sides.
    assert_eq!(
        g(
            "eg = ExceptionGroup('out', [ValueError(1), ExceptionGroup('in', [TypeError(2), \
             ValueError(3)])])\nx = repr(eg.split(ValueError))",
            "x"
        ),
        concat!(
            "\"(ExceptionGroup('out', [ValueError(1), ExceptionGroup('in', [ValueError(3)])]), ",
            "ExceptionGroup('out', [ExceptionGroup('in', [TypeError(2)])]))\""
        )
    );
    // `subgroup` is `split` without the remainder, and takes a predicate too.
    assert_eq!(
        g(&format!("{setup}x = repr(eg.subgroup(TypeError))"), "x"),
        "\"ExceptionGroup('g', [TypeError(2)])\""
    );
    assert_eq!(
        g(
            &format!("{setup}x = repr(eg.subgroup(lambda e: isinstance(e, ValueError)))"),
            "x"
        ),
        "\"ExceptionGroup('g', [ValueError(1)])\""
    );
    assert_eq!(g(&format!("{setup}x = eg.subgroup(KeyError)"), "x"), "None");
    // `derive` keeps the message and takes new members.
    assert_eq!(
        g(&format!("{setup}x = repr(eg.derive([KeyError('k')]))"), "x"),
        "\"ExceptionGroup('g', [KeyError('k')])\""
    );
    // `BaseExceptionGroup` narrows to `ExceptionGroup` unless it holds a bare
    // `BaseException`.
    assert_eq!(
        g(
            "x = type(BaseExceptionGroup('b', [ValueError(1)])).__name__",
            "x"
        ),
        "'ExceptionGroup'"
    );
    assert_eq!(
        g(
            "x = type(BaseExceptionGroup('b', [KeyboardInterrupt()])).__name__",
            "x"
        ),
        "'BaseExceptionGroup'"
    );
    // Constructor validation, message for message.
    for (src, want) in [
        (
            "ExceptionGroup('g', [])",
            "ValueError: second argument (exceptions) must be a non-empty sequence",
        ),
        (
            "ExceptionGroup('g', [KeyboardInterrupt()])",
            "TypeError: Cannot nest BaseExceptions in an ExceptionGroup",
        ),
        (
            "ExceptionGroup(1, [ValueError()])",
            "TypeError: BaseExceptionGroup.__new__() argument 1 must be str, not int",
        ),
        (
            "ExceptionGroup('g', ValueError())",
            "TypeError: second argument (exceptions) must be a sequence",
        ),
        (
            "ExceptionGroup('g')",
            "TypeError: BaseExceptionGroup.__new__() takes exactly 2 arguments (1 given)",
        ),
        (
            "ExceptionGroup('g', [ValueError, TypeError()])",
            "ValueError: Item 0 of second argument (exceptions) is not an exception",
        ),
    ] {
        assert_eq!(
            eval_str(&format!("x = {src}")).expect_err(src),
            want,
            "for {src:?}"
        );
    }
}

/// PEP 654 `except*`: which clause claims which part of the group, and what is
/// rebuilt from what the handlers left behind. Every expectation is CPython
/// 3.14.6's, byte-checked against `python3`.
#[test]
fn except_star_matches_and_reraises_the_unhandled_part() {
    // Each clause runs AT MOST ONCE, bound to the subgroup it matched, and the
    // unmatched remainder propagates as a group.
    assert_eq!(
        g(
            "log = []\n\
             try:\n\
             \x20   try:\n\
             \x20       raise ExceptionGroup('g', [ValueError(1), TypeError(2), KeyError(3)])\n\
             \x20   except* ValueError as e:\n\
             \x20       log.append(repr(e))\n\
             \x20   except* TypeError as e:\n\
             \x20       log.append(repr(e))\n\
             except BaseException as outer:\n\
             \x20   log.append('escaped ' + repr(outer))\n\
             x = log",
            "x"
        ),
        "[\"ExceptionGroup('g', [ValueError(1)])\", \"ExceptionGroup('g', [TypeError(2)])\", \
         \"escaped ExceptionGroup('g', [KeyError(3)])\"]"
    );
    // A naked exception that matches is wrapped in a one-element group.
    assert_eq!(
        g(
            "try:\n    raise ValueError('bare')\nexcept* ValueError as e:\n    x = repr(e)",
            "x"
        ),
        "\"ExceptionGroup('', (ValueError('bare'),))\""
    );
    // A handler that raises becomes a SIBLING of the unhandled remainder...
    assert_eq!(
        g(
            "try:\n\
             \x20   try:\n\
             \x20       raise ExceptionGroup('g', [ValueError(1), TypeError(2)])\n\
             \x20   except* ValueError:\n\
             \x20       raise RuntimeError('boom')\n\
             except BaseException as outer:\n\
             \x20   x = repr(outer)",
            "x"
        ),
        "\"ExceptionGroup('', [RuntimeError('boom'), ExceptionGroup('g', [TypeError(2)])])\""
    );
    // ...but a BARE re-raise is merged back into the original group's nesting,
    // which is the whole point of the projection step.
    assert_eq!(
        g(
            "try:\n\
             \x20   try:\n\
             \x20       raise ExceptionGroup('g', [ValueError(1), TypeError(2)])\n\
             \x20   except* ValueError:\n\
             \x20       raise\n\
             except BaseException as outer:\n\
             \x20   x = repr(outer)",
            "x"
        ),
        "\"ExceptionGroup('g', [ValueError(1), TypeError(2)])\""
    );
    // A lone raising handler with nothing left over propagates unwrapped.
    assert_eq!(
        g(
            "try:\n\
             \x20   try:\n\
             \x20       raise ExceptionGroup('g', [ValueError(1)])\n\
             \x20   except* ValueError:\n\
             \x20       raise RuntimeError('boom')\n\
             except BaseException as outer:\n\
             \x20   x = repr(outer)",
            "x"
        ),
        "\"RuntimeError('boom')\""
    );
    // `else`/`finally` still work, and the `as` name is unbound afterwards.
    assert_eq!(
        g(
            "log = []\n\
             try:\n\
             \x20   pass\n\
             except* ValueError:\n\
             \x20   log.append('no')\n\
             else:\n\
             \x20   log.append('else')\n\
             finally:\n\
             \x20   log.append('finally')\n\
             x = log",
            "x"
        ),
        "['else', 'finally']"
    );
    assert_eq!(
        g(
            "try:\n\
             \x20   raise ExceptionGroup('g', [ValueError(1)])\n\
             except* ValueError as e:\n\
             \x20   pass\n\
             try:\n\
             \x20   e\n\
             \x20   x = 'still bound'\n\
             except NameError:\n\
             \x20   x = 'unbound'",
            "x"
        ),
        "'unbound'"
    );
}

/// PEP 654's compile-time rules for `except*`, which pythonrs used to accept.
#[test]
fn except_star_syntax_rules_are_enforced() {
    for (src, want) in [
        (
            "try:\n    pass\nexcept ValueError:\n    pass\nexcept* TypeError:\n    pass",
            "SyntaxError: cannot have both 'except' and 'except*' on the same 'try'",
        ),
        (
            "try:\n    pass\nexcept*:\n    pass",
            "SyntaxError: expected one or more exception types",
        ),
        (
            "def f():\n    try:\n        pass\n    except* ValueError:\n        return 1",
            "SyntaxError: 'break', 'continue' and 'return' cannot appear in an except* block",
        ),
        (
            "for i in [1]:\n    try:\n        pass\n    except* ValueError:\n        continue",
            "SyntaxError: 'break', 'continue' and 'return' cannot appear in an except* block",
        ),
        (
            "for i in [1]:\n    try:\n        pass\n    except* ValueError:\n        break",
            "SyntaxError: 'break', 'continue' and 'return' cannot appear in an except* block",
        ),
    ] {
        assert_eq!(eval_str(src).expect_err(src), want, "for {src:?}");
    }
    // A loop written INSIDE the handler owns its own `break`/`continue`, and a
    // nested `def` owns its own `return` — neither leaves the handler.
    for src in [
        "try:\n    raise ExceptionGroup('g', [ValueError(1)])\nexcept* ValueError:\n    \
         for i in range(2):\n        break",
        "try:\n    raise ExceptionGroup('g', [ValueError(1)])\nexcept* ValueError:\n    \
         def h():\n        return 1\n    assert h() == 1",
    ] {
        if let Err(e) = eval_str(src) {
            panic!("{src:?} must compile and run, got: {e}");
        }
    }
}

/// A call evaluates its CALLEE before its arguments (CPython's order). The
/// bare-name form used to push the name and let the `CALL` op resolve it after
/// the arguments were on the stack, so `aa(bb)` blamed `bb`.
#[test]
fn a_call_resolves_a_bare_name_callee_before_its_arguments() {
    assert_eq!(
        eval_str("aa(bb)").expect_err("aa(bb)"),
        "NameError: name 'aa' is not defined"
    );
    assert_eq!(
        eval_str("aa(bb, cc)").expect_err("aa(bb, cc)"),
        "NameError: name 'aa' is not defined"
    );
    assert_eq!(
        eval_str("aa(k=bb)").expect_err("aa(k=bb)"),
        "NameError: name 'aa' is not defined"
    );
    // An argument that raises no longer masks the undefined callee.
    assert_eq!(
        eval_str("def boom():\n    raise ValueError('arg')\naa(boom())").expect_err("aa(boom())"),
        "NameError: name 'aa' is not defined"
    );
    // The callee is resolved once, before the arguments run.
    assert_eq!(
        g(
            "log = []\n\
             def f(*a):\n\
             \x20   log.append('call')\n\
             \x20   return 0\n\
             def arg():\n\
             \x20   log.append('arg')\n\
             \x20   return 1\n\
             f(arg())\n\
             x = log",
            "x"
        ),
        "['arg', 'call']"
    );
    // The builtin type objects are interned, so a builtin is one object as in
    // CPython (`id(len) == id(len)`), not a fresh allocation per read.
    assert_eq!(
        g("a = len\nb = len\nx = (a is b, id(a) == id(b))", "x"),
        "(True, True)"
    );
}

// A user-`__hash__` object NESTED inside a `tuple`/`frozenset` key. The key is
// hashed element-wise, so the element is a key in its own right — but only the
// TOP-LEVEL object was ever prepared outside the host borrow, so the borrowed
// `to_key` hit the nested one with no resolved key and raised
// `TypeError: unhashable type: 'P'`. CPython answers every line below.
#[test]
fn value_keyed_objects_nested_inside_tuple_and_frozenset_keys() {
    let p = "\
class P:\n\
\x20   def __init__(self, v): self.v = v\n\
\x20   def __hash__(self): return hash(self.v)\n\
\x20   def __eq__(self, o): return isinstance(o, P) and self.v == o.v\n";
    // A tuple key: built, looked up, re-assigned, and membership-tested — each
    // through an independently constructed but value-equal key.
    assert_eq!(
        g(
            &format!(
                "{p}d = {{(P(1),): 'a', (P(2), P(3)): 'b'}}\n\
                 d[(P(1),)] = 'c'\n\
                 x = [d[(P(1),)], d[(P(2), P(3))], len(d), (P(1),) in d, (P(9),) in d]"
            ),
            "x"
        ),
        "['c', 'b', 2, True, False]"
    );
    // Two equal elements in ONE key must collapse onto each other, or two
    // independently built equal keys take different heap ids and never match.
    assert_eq!(
        g(
            &format!(
                "{p}x = [{{(P(1), P(1)): 7}}[(P(1), P(1))], len({{(P(1), P(1)), (P(1), P(1))}})]"
            ),
            "x"
        ),
        "[7, 1]"
    );
    // Deeper nesting, and a tuple key alongside the bare instance: the nested
    // element collapses onto the top-level key's object WITHOUT the two keys
    // merging into one slot.
    assert_eq!(
        g(
            &format!("{p}d = {{((P(1),),): 1, P(1): 2}}\nx = [len(d), d[((P(1),),)], d[P(1)]]"),
            "x"
        ),
        "[2, 1, 2]"
    );
    // A `frozenset` key's element keys are computed when the frozenset is BUILT,
    // so they have to be recomputed against the destination container.
    assert_eq!(
        g(
            &format!(
                "{p}d = {{frozenset([P(1), P(2)]): 'f'}}\n\
                 d[frozenset([P(2), P(1)])] = 'g'\n\
                 x = [len(d), d[frozenset([P(1), P(2)])], {{frozenset([P(1)])}} == {{frozenset([P(1)])}}]"
            ),
            "x"
        ),
        "[1, 'g', True]"
    );
    // `hash()` of a container holding a value-keyed element: CPython derives it
    // from `__hash__()` alone, so the heap id the key carries for slot
    // discrimination must not reach it.
    assert_eq!(
        g(
            &format!(
                "{p}x = [hash((P(1),)) == hash((P(1),)), \
                 hash(((P(1),),)) == hash(((P(1),),)), \
                 hash(frozenset([P(1)])) == hash(frozenset([P(1)]))]"
            ),
            "x"
        ),
        "[True, True, True]"
    );
    // The set algebra over tuple elements: `align_operand` has to recognise a
    // tuple key as value-keyed before it will re-key the right operand.
    assert_eq!(
        g(
            &format!(
                "{p}A = {{(P(1),), (P(2),)}}\nB = {{(P(2),), (P(3),)}}\n\
                 x = [sorted(e[0].v for e in A & B), sorted(e[0].v for e in A | B), \
                 A == {{(P(1),), (P(2),)}}]"
            ),
            "x"
        ),
        "[[2], [1, 2, 3], True]"
    );
}

// A `list`/`tuple`/`deque`/`dict` compares element-wise INSIDE the host borrow,
// where a user `__eq__` cannot run — so `P(1) == P(1)` was True while
// `(P(1),) == (P(1),)` and `[P(1)] == [P(1)]` were False. Same shape in
// `tuple.index`/`tuple.count`, which used the borrowed comparison while their
// `list` counterparts already used the rich one.
#[test]
fn container_equality_runs_the_elements_user_eq() {
    let p = "\
from collections import deque\n\
class P:\n\
\x20   def __init__(self, v): self.v = v\n\
\x20   def __hash__(self): return hash(self.v)\n\
\x20   def __eq__(self, o): return isinstance(o, P) and self.v == o.v\n";
    assert_eq!(
        g(
            &format!(
                "{p}x = [(P(1),) == (P(1),), [P(1)] == [P(1)], [[P(1)]] == [[P(1)]], \
                 [(P(1),)] == [(P(1),)], deque([P(1)]) == deque([P(1)]), \
                 {{1: P(1)}} == {{1: P(1)}}, {{P(1): P(2)}} == {{P(1): P(2)}}]"
            ),
            "x"
        ),
        "[True, True, True, True, True, True, True]"
    );
    // The negative side: unequal elements, a length mismatch, and the
    // list-vs-tuple kind mismatch CPython never calls equal.
    assert_eq!(
        g(
            &format!(
                "{p}x = [[P(1)] == [P(2)], [P(1)] == [P(1), P(2)], [P(1)] == (P(1),), \
                 [P(1)] != [P(2)]]"
            ),
            "x"
        ),
        "[False, False, False, True]"
    );
    // `tuple.index`/`tuple.count` against the `list` forms that already worked.
    assert_eq!(
        g(
            &format!(
                "{p}t = (P(1), P(2), P(1))\n\
                 x = [t.index(P(2)), t.count(P(1)), P(2) in t, list(t).index(P(2))]"
            ),
            "x"
        ),
        "[1, 2, True, 1]"
    );
}

// Operator slots are dispatched natively rather than through per-type descriptor
// objects, so only `int`/`float`/`bool` (which carry an explicit dunder table)
// answered one as a BOUND METHOD. Every other builtin raised
// `AttributeError: 'dict' object has no attribute '__ior__'` while the `d |= …`
// syntax worked. `dir()` must list exactly what dispatch accepts, so the same
// table drives both (asserted by `builtin_dispatch_is_fully_listed_by_dir`).
#[test]
fn builtin_operator_dunders_are_callable_as_bound_methods() {
    // dict: the merge operators, with `__ior__` mutating in place and returning
    // the SAME object (CPython returns self) and taking any `update` argument.
    assert_eq!(
        g(
            "d = {'a': 1}\n\
             r = d.__ior__({'b': 2})\n\
             x = [r is d, d, {'a': 1}.__or__({'b': 2}), {'a': 1}.__ror__({'b': 2})]",
            "x"
        ),
        "[True, {'a': 1, 'b': 2}, {'a': 1, 'b': 2}, {'b': 2, 'a': 1}]"
    );
    // set: all four operators, forward, reflected and in-place.
    assert_eq!(
        g(
            "s = {1, 2}\n\
             r = s.__ior__({3})\n\
             x = [r is s, sorted(s), sorted({1, 2}.__and__({2, 3})), \
             sorted({1, 2}.__sub__({2})), sorted({1, 2}.__xor__({2, 3}))]",
            "x"
        ),
        "[True, [1, 2, 3], [2], [1], [1, 3]]"
    );
    // frozenset has no in-place halves; its `__ror__` keeps the LEFT operand's
    // type, so `{1}.__ror__(frozenset({2}))` is a frozenset.
    assert_eq!(
        g(
            "x = [sorted(frozenset({1}).__or__({2})), type({1}.__ror__(frozenset({2}))).__name__]",
            "x"
        ),
        "[[1, 2], 'frozenset']"
    );
    // Sequences and text: concatenation, repetition (both directions), and `%`.
    assert_eq!(
        g(
            "l = [1]\n\
             r = l.__iadd__((2,))\n\
             x = [r is l, l, [1].__add__([2]), (1,).__add__((2,)), 'a'.__mul__(3), \
             'a'.__rmul__(2), '%d' .__mod__(7), b'a'.__add__(b'b')]",
            "x"
        ),
        "[True, [1, 2], [1, 2], (1, 2), 'aaa', 'aa', '7', b'ab']"
    );
    // An operand of the wrong kind answers NotImplemented for the set/dict and
    // complex operators (CPython returns it rather than raising, which is what
    // lets `{1} | [2]` report `unsupported operand type(s)`).
    assert_eq!(
        g(
            "x = [{1: 2}.__or__([1]), {1}.__or__([2]), {1}.__sub__([1]), (1j).__add__('a')]",
            "x"
        ),
        "[NotImplemented, NotImplemented, NotImplemented, NotImplemented]"
    );
    // The type a builtin does NOT expose stays an AttributeError that names the
    // type, as CPython's does.
    assert_eq!(
        g(
            "try:\n\
             \x20   (1).__iadd__(2)\n\
             except AttributeError as e:\n\
             \x20   x = str(e)",
            "x"
        ),
        "\"'int' object has no attribute '__iadd__'\""
    );
}

// `x %= args` shared a fallback with `x = x % args` that carried the `str %`
// branch but NOT the `bytes`/`bytearray` one (PEP 461), so `b'%d' % 1` formatted
// while `x %= 1` on the same receiver raised `unsupported operand type(s)`.
#[test]
fn bytes_percent_works_in_place_too() {
    assert_eq!(
        g(
            "x = b'%d-%s'\n\
             x %= (1, b'z')\n\
             y = bytearray(b'%d')\n\
             y %= 5\n\
             x = [x, y, b'%d'.__mod__(3)]",
            "x"
        ),
        "[b'1-z', bytearray(b'5'), b'3']"
    );
}

// PEP 617 (CPython 3.10+) accepts a parenthesized with-item list. The frontend
// rejected it outright (`SyntaxError: expected ')' but found Name("as")`), which
// is a hard stop on any modern script that wraps a long `with` header. The
// parenthesized alternative also has to WIN over the tuple reading, so
// `with (a, b):` is two context managers.
#[test]
fn parenthesized_with_items_parse_as_separate_managers() {
    assert_eq!(
        g(
            "log = []\n\
             class C:\n\
             \x20   def __init__(self, n): self.n = n\n\
             \x20   def __enter__(self):\n\
             \x20       log.append(('in', self.n))\n\
             \x20       return self.n\n\
             \x20   def __exit__(self, *a):\n\
             \x20       log.append(('out', self.n))\n\
             \x20       return False\n\
             with (C(1) as a, C(2) as b):\n\
             \x20   log.append(('body', a, b))\n\
             x = log",
            "x"
        ),
        "[('in', 1), ('in', 2), ('body', 1, 2), ('out', 2), ('out', 1)]"
    );
    // A trailing comma is allowed, and an item with no `as` still runs.
    assert_eq!(
        g(
            "n = 0\n\
             class C:\n\
             \x20   def __enter__(self):\n\
             \x20       global n\n\
             \x20       n += 1\n\
             \x20   def __exit__(self, *a): return False\n\
             with (C(), C(),):\n\
             \x20   pass\n\
             x = n",
            "x"
        ),
        "2"
    );
    // `with (expr) as v:` is still ONE item whose context is the parenthesized
    // expression — the group does not close right before the `:`, so the
    // item-list reading must not fire.
    assert_eq!(
        g(
            "class C:\n\
             \x20   def __enter__(self): return 7\n\
             \x20   def __exit__(self, *a): return False\n\
             with (C()) as v:\n\
             \x20   x = v",
            "x"
        ),
        "7"
    );
}

// `divmod` was computed as `(a // b, a % b)`, so a class defining `__divmod__`
// (and nothing else) raised `unsupported operand type(s) for //`. CPython
// dispatches it as a binary operator in its own right.
#[test]
fn divmod_dispatches_the_divmod_dunders() {
    assert_eq!(
        g(
            "class V:\n\
             \x20   def __divmod__(self, o): return ('dm', o)\n\
             \x20   def __rdivmod__(self, o): return ('rdm', o)\n\
             x = [divmod(V(), 3), divmod(3, V())]",
            "x"
        ),
        "[('dm', 3), ('rdm', 3)]"
    );
    // `__divmod__` wins over `__floordiv__`/`__mod__` when both are defined.
    assert_eq!(
        g(
            "class M:\n\
             \x20   def __floordiv__(self, o): return 'fd'\n\
             \x20   def __mod__(self, o): return 'md'\n\
             \x20   def __divmod__(self, o): return 'DM'\n\
             x = divmod(M(), 1)",
            "x"
        ),
        "'DM'"
    );
    // Neither dunder: CPython names the builtin in the message.
    assert_eq!(
        g(
            "class V: pass\n\
             try:\n\
             \x20   divmod(V(), 3)\n\
             except TypeError as e:\n\
             \x20   x = str(e)",
            "x"
        ),
        "\"unsupported operand type(s) for divmod(): 'V' and 'int'\""
    );
    // The native path is untouched.
    assert_eq!(
        g("x = [divmod(-7, 2), divmod(7, -2)]", "x"),
        "[(-4, 1), (-4, -1)]"
    );
}

// `dir(obj)` listed the class/instance dict and ignored a user `__dir__`.
// CPython calls `type(obj).__dir__(obj)` and only sorts what comes back — no
// dedup, and the sort's own errors surface.
#[test]
fn dir_dispatches_the_dir_hook() {
    assert_eq!(
        g(
            "class C:\n\
             \x20   def __dir__(self): return ['z', 'a', 'a']\n\
             x = dir(C())",
            "x"
        ),
        "['a', 'a', 'z']"
    );
    // Any iterable is accepted and becomes a sorted list.
    assert_eq!(
        g(
            "class D:\n\
             \x20   def __dir__(self): return ('q', 'b')\n\
             x = dir(D())",
            "x"
        ),
        "['b', 'q']"
    );
    // The hook belongs to instances: `dir(TheClass)` still lists the class.
    assert_eq!(
        g(
            "class C:\n\
             \x20   def __dir__(self): return ['z']\n\
             x = '__dir__' in dir(C)",
            "x"
        ),
        "True"
    );
}

// `obj.__class__ = C` stored a shadowing entry in the instance dict and left
// `type(obj)` alone — a silently wrong retype. CPython's setter swaps the type
// when the layouts match and raises otherwise.
#[test]
fn class_assignment_retypes_the_instance() {
    assert_eq!(
        g(
            "class A:\n\
             \x20   def hi(self): return 'A'\n\
             class B:\n\
             \x20   def hi(self): return 'B'\n\
             a = A()\n\
             a.v = 3\n\
             a.__class__ = B\n\
             x = [type(a).__name__, a.__class__ is B, isinstance(a, B), isinstance(a, A), a.hi(), a.v]",
            "x"
        ),
        "['B', True, True, False, 'B', 3]"
    );
    // Two classes adding the same slots share a layout; different slot names do
    // not, and a `__dict__`-carrying class never matches a fully slotted one.
    assert_eq!(
        g(
            "class S1:\n\
             \x20   __slots__ = ('x',)\n\
             class S2:\n\
             \x20   __slots__ = ('x',)\n\
             s = S1()\n\
             s.__class__ = S2\n\
             x = type(s).__name__",
            "x"
        ),
        "'S2'"
    );
    assert_eq!(
        g(
            "class S1:\n\
             \x20   __slots__ = ('x',)\n\
             class S3:\n\
             \x20   __slots__ = ('y',)\n\
             try:\n\
             \x20   S1().__class__ = S3\n\
             except TypeError as e:\n\
             \x20   x = str(e)",
            "x"
        ),
        "\"__class__ assignment: 'S3' object layout differs from 'S1'\""
    );
    // A non-class value, a static builtin type, and deletion each have their own
    // CPython message — and the value check runs before the mutability check.
    assert_eq!(
        g(
            "class A: pass\n\
             out = []\n\
             for f in (lambda o: setattr(o, '__class__', 5),\n\
             \x20         lambda o: setattr(o, '__class__', int),\n\
             \x20         lambda o: delattr(o, '__class__')):\n\
             \x20   try:\n\
             \x20       f(A())\n\
             \x20   except TypeError as e:\n\
             \x20       out.append(str(e))\n\
             x = out",
            "x"
        ),
        "[\"__class__ must be set to a class, not 'int' object\", \
         '__class__ assignment only supported for mutable types or ModuleType subclasses', \
         \"can't delete __class__ attribute\"]"
    );
}

// A value that is not a context manager was entered anyway: the `with` desugar
// called `ctx.__enter__()` directly, so an object carrying only `__enter__` ran
// it AND the whole body before failing on the way out with an `AttributeError`.
// CPython looks up `__exit__` first and refuses to enter, with a `TypeError`.
#[test]
fn with_checks_the_context_manager_protocol_before_entering() {
    assert_eq!(
        g(
            "log = []\n\
             class E:\n\
             \x20   def __enter__(self):\n\
             \x20       log.append('entered')\n\
             \x20       return 1\n\
             try:\n\
             \x20   with E():\n\
             \x20       log.append('body')\n\
             except TypeError as e:\n\
             \x20   log.append(str(e))\n\
             x = log",
            "x"
        ),
        "[\"'E' object does not support the context manager protocol \
         (missed __exit__ method)\"]"
    );
    // With `__exit__` present and `__enter__` missing, the other half is named.
    assert_eq!(
        g(
            "class X:\n\
             \x20   def __exit__(self, *a): return False\n\
             try:\n\
             \x20   with X(): pass\n\
             except TypeError as e:\n\
             \x20   x = str(e)",
            "x"
        ),
        "\"'X' object does not support the context manager protocol \
         (missed __enter__ method)\""
    );
    // A plain value reports against `__exit__`, the half CPython checks first.
    assert_eq!(
        g(
            "try:\n\
             \x20   with 1: pass\n\
             except TypeError as e:\n\
             \x20   x = str(e)",
            "x"
        ),
        "\"'int' object does not support the context manager protocol \
         (missed __exit__ method)\""
    );
    // The check belongs to the `with` statement: calling the dunder by hand
    // still raises the ordinary AttributeError CPython raises for it.
    assert_eq!(
        g(
            "try:\n\
             \x20   (1).__enter__()\n\
             except AttributeError as e:\n\
             \x20   x = str(e).split('.')[0]",
            "x"
        ),
        "\"'int' object has no attribute '__enter__'\""
    );
    // A working manager is untouched, and its `__exit__` still runs LIFO.
    assert_eq!(
        g(
            "log = []\n\
             class C:\n\
             \x20   def __init__(self, n): self.n = n\n\
             \x20   def __enter__(self): return self.n\n\
             \x20   def __exit__(self, *a):\n\
             \x20       log.append(self.n)\n\
             \x20       return False\n\
             with C(1) as a, C(2) as b:\n\
             \x20   log.append((a, b))\n\
             x = log",
            "x"
        ),
        "[(1, 2), 2, 1]"
    );
}

// `frozenset` is immutable, so it must not advertise the mutating half of the
// `set` method table. Sharing one table made `hasattr(frozenset(), "add")`
// answer `True` while the call raised `AttributeError`, so duck-typed code
// (`if hasattr(s, "add"): s.add(x)`) chose the mutable branch and then died.
// Expected values are CPython 3.14's.
#[test]
fn frozenset_advertises_only_its_immutable_methods() {
    // Every mutator `set` has and `frozenset` does not.
    for m in [
        "add",
        "remove",
        "discard",
        "pop",
        "clear",
        "update",
        "intersection_update",
        "difference_update",
        "symmetric_difference_update",
    ] {
        assert_eq!(
            g(&format!("x = hasattr(frozenset(), {m:?})"), "x"),
            "False",
            "frozenset must not advertise the set mutator {m:?}"
        );
        assert_eq!(
            g(&format!("x = {m:?} in dir(frozenset())"), "x"),
            "False",
            "dir(frozenset()) must not list the set mutator {m:?}"
        );
        // `set` keeps every one of them.
        assert_eq!(
            g(&format!("x = hasattr(set(), {m:?})"), "x"),
            "True",
            "set must still advertise {m:?}"
        );
    }
    // The query half survives on both.
    assert_eq!(
        g(
            "x = sorted(n for n in dir(frozenset()) if not n.startswith('_'))",
            "x"
        ),
        "['copy', 'difference', 'intersection', 'isdisjoint', \
         'issubset', 'issuperset', 'symmetric_difference', 'union']"
    );
    // What it advertises, it can actually do — the property the whole table
    // exists to promise.
    assert_eq!(
        g("x = frozenset([1, 2]).union([3])", "x"),
        "frozenset({1, 2, 3})"
    );
}

// A name-dispatched method called with too few arguments must raise TypeError,
// never abort the process. These six indexed `args[0]`/`args[1]` unchecked; the
// resulting Rust panic exits 1 — the same status a normal uncaught Python
// exception uses — so it was invisible to exit-status checks and uncatchable by
// `except BaseException`. Messages are CPython 3.14's verbatim.
#[test]
fn missing_required_argument_raises_typeerror_instead_of_aborting() {
    let cases = [
        (
            "''.join()",
            "str.join() takes exactly one argument (0 given)",
        ),
        (
            "''.zfill()",
            "str.zfill() takes exactly one argument (0 given)",
        ),
        (
            "str.maketrans()",
            "maketrans expected at least 1 argument, got 0",
        ),
        ("''.center()", "center expected at least 1 argument, got 0"),
        ("''.ljust()", "ljust expected at least 1 argument, got 0"),
        ("''.rjust()", "rjust expected at least 1 argument, got 0"),
        ("filter(1)", "filter expected 2 arguments, got 1"),
    ];
    for (expr, want) in cases {
        // The exception must be catchable at all — a panic never reaches here.
        assert_eq!(
            g(
                &format!(
                    "try:\n\x20   {expr}\n\x20   x = 'no error'\nexcept TypeError as e:\n\x20   x = str(e)"
                ),
                "x"
            ),
            // `repr` of a str with no apostrophe in it — as every message here is.
            format!("'{want}'"),
            "{expr} must raise TypeError with CPython's message"
        );
    }
    // The same call one argument later still works, so the arity check did not
    // swallow the happy path.
    assert_eq!(g("x = '-'.join('ab')", "x"), "'a-b'");
    assert_eq!(g("x = '7'.zfill(3)", "x"), "'007'");
    assert_eq!(g("x = 'ab'.center(6, '.')", "x"), "'..ab..'");
    assert_eq!(g("x = list(filter(None, [0, 1, 2]))", "x"), "[1, 2]");
}

// Private-name mangling (CPython `_Py_Mangle`): every `__name` written inside a
// class body compiles as `_Class__name`. Without it a subclass's `__x` aliases
// its base's and the privacy guarantee is gone. Expected values are CPython
// 3.14's for the same program.
#[test]
fn private_names_mangle_against_the_enclosing_class() {
    // The attribute a method stores lands under the mangled key.
    assert_eq!(
        g(
            "class C:\n\x20   def __init__(self): self.__x = 1\nx = C().__dict__",
            "x"
        ),
        "{'_C__x': 1}"
    );
    // A class variable mangles, and the body still reads it back.
    assert_eq!(
        g(
            "class D:\n\x20   __y = 2\n\x20   def get(self): return D.__y\n\
             x = (sorted(n for n in D.__dict__ if 'y' in n), D().get())",
            "x"
        ),
        "(['_D__y'], 2)"
    );
    // The AttributeError names the mangled attribute, as CPython's does.
    assert_eq!(
        g(
            "class F:\n\x20   def m(self): return self.__missing\n\
             try:\n\x20   F().m()\nexcept AttributeError as e:\n\x20   x = str(e)",
            "x"
        ),
        "\"'F' object has no attribute '_F__missing'\""
    );
    // Leading underscores are stripped from the class name...
    assert_eq!(
        g(
            "class _K:\n\x20   def __init__(self): self.__v = 1\nx = sorted(_K().__dict__)",
            "x"
        ),
        "['_K__v']"
    );
    // ...including when the class name is itself dunder-ish.
    assert_eq!(
        g(
            "class __L:\n\x20   def __init__(self): self.__v = 1\nx = sorted(__L().__dict__)",
            "x"
        ),
        "['_L__v']"
    );
    // The INNERMOST enclosing class wins.
    assert_eq!(
        g(
            "class M:\n\x20   class N:\n\x20       def __init__(self): self.__w = 1\n\
             x = sorted(M.N().__dict__)",
            "x"
        ),
        "['_N__w']"
    );
    // `global __g` inside a method declares the MANGLED global: the module
    // never gains a plain `__g`, so code outside the class cannot reach it by
    // the name the class wrote.
    assert_eq!(
        g(
            "class P:\n\x20   def m(self):\n\x20       global __g\n\x20       __g = 42\n\
             \x20       return __g\n\
             r = P().m()\nx = (r, '_P__g' in globals(), _P__g, '__g' in globals())",
            "x"
        ),
        "(42, True, 42, False)"
    );
}

// The exemptions matter as much as the rule: mangling everything that starts
// with two underscores would break every dunder in the language.
#[test]
fn mangling_skips_dunders_single_underscores_and_call_keywords() {
    // `__x__` has two trailing underscores -> untouched. `__y_` has one -> mangled.
    // `_z` has one leading -> untouched.
    assert_eq!(
        g(
            "class J:\n\x20   def __init__(self):\n\x20       self.__x__ = 1\n\
             \x20       self.__y_ = 2\n\x20       self._z = 3\nx = sorted(J().__dict__)",
            "x"
        ),
        "['_J__y_', '__x__', '_z']"
    );
    // A CALL keyword is not an identifier reference, so it does not mangle.
    assert_eq!(
        g(
            "class H:\n\x20   def m(self, **kw): return kw\nx = H().m(__k=1)",
            "x"
        ),
        "{'__k': 1}"
    );
    // `__init__` itself still binds under its own name — the whole object
    // protocol depends on dunders surviving the pass.
    assert_eq!(
        g(
            "class Q:\n\x20   def __init__(self): self.v = 1\n\x20   def __len__(self): return 7\n\
             x = (len(Q()), Q().v)",
            "x"
        ),
        "(7, 1)"
    );
    // Nothing outside a class body mangles.
    assert_eq!(g("__m = 5\nx = __m", "x"), "5");
}

// A slot name is an identifier in the class body, so it mangles for the
// descriptor while `__slots__` keeps the name as written. Getting only one half
// right makes a private slot unusable.
#[test]
fn slot_names_mangle_for_the_descriptor_but_not_in_the_tuple() {
    assert_eq!(
        g(
            "class G:\n\x20   __slots__ = ('__s',)\n\x20   def __init__(self): self.__s = 9\n\
             x = (G().__slots__, G()._G__s)",
            "x"
        ),
        "(('__s',), 9)"
    );
    // And the mangled name is what the conflict check compares, so a `_G__x`
    // class variable beside a `__x` slot is the collision CPython reports.
    assert_eq!(
        g(
            "try:\n\x20   class C:\n\x20       __slots__ = ('__x',)\n\x20       _C__x = 1\n\
             except ValueError as e:\n\x20   x = str(e)",
            "x"
        ),
        "\"'_C__x' in __slots__ conflicts with class variable\""
    );
}

/// Diagnostics whose wording was transcribed from memory or an older CPython.
///
/// Each row was classified by running the same probe under CPython 3.9.25,
/// 3.10.20, 3.11.15, 3.12.13, 3.13.15 and 3.14.6 and asking two questions: does
/// 3.14.6 produce this today, and did ANY of those six ever produce it. A row
/// marked `fabricated` failed the second question — the text was in the
/// implementation source but in no CPython. `stale` matched an older release.
///
/// Expected values are 3.14.6's, captured with `python3 -c`.
#[test]
fn diagnostics_match_the_reference_wording() {
    let cases: &[(&str, &str)] = &[
        // fabricated: "re-raise" is hyphenated in no CPython 3.9-3.14.
        ("raise", "RuntimeError: No active exception to reraise"),
        // fabricated: borrowed factorial's message for a function that reports
        // WHICH parameter was negative.
        (
            "import math\nmath.comb(-1, 2)",
            "ValueError: n must be a non-negative integer",
        ),
        (
            "import math\nmath.comb(2, -1)",
            "ValueError: k must be a non-negative integer",
        ),
        (
            "import math\nmath.perm(-1, 2)",
            "ValueError: n must be a non-negative integer",
        ),
        // …but factorial really does word it that way; do not unify them.
        (
            "import math\nmath.factorial(-1)",
            "ValueError: factorial() not defined for negative values",
        ),
        // fabricated: echoed the offending string, which CPython never does.
        (
            "float.fromhex('zzz')",
            "ValueError: invalid hexadecimal floating-point string",
        ),
        // fabricated: a TypeError for arguments of the right type.
        (
            "'ab'.maketrans('ab', 'c')",
            "ValueError: the first two maketrans arguments must have equal length",
        ),
        // …and bytes.maketrans genuinely words it differently. Verified, not a
        // copy-paste of the line above.
        (
            "bytes.maketrans(b'ab', b'c')",
            "ValueError: maketrans arguments must have same length",
        ),
        // fabricated: the generic operand-type message for a sequence repeat.
        (
            "'a' * 'b'",
            "TypeError: can't multiply sequence by non-int of type 'str'",
        ),
        (
            "[1] * [2]",
            "TypeError: can't multiply sequence by non-int of type 'list'",
        ),
        (
            "'a' * None",
            "TypeError: can't multiply sequence by non-int of type 'NoneType'",
        ),
        // …and a non-sequence still gets the generic one.
        (
            "1 * None",
            "TypeError: unsupported operand type(s) for *: 'int' and 'NoneType'",
        ),
        // fabricated: dropped the code point and the index in the template.
        (
            "'%z' % 1",
            "ValueError: unsupported format character 'z' (0x7a) at index 1",
        ),
        (
            "'ab%q' % 1",
            "ValueError: unsupported format character 'q' (0x71) at index 3",
        ),
        (
            "b'%z' % 1",
            "ValueError: unsupported format character 'z' (0x7a) at index 1",
        ),
        // fabricated: named neither the item index nor the offending type.
        (
            "','.join([1])",
            "TypeError: sequence item 0: expected str instance, int found",
        ),
        (
            "','.join(['a', 2])",
            "TypeError: sequence item 1: expected str instance, int found",
        ),
        // stale (3.9/3.10): 3.11 added the offending type.
        (
            "'abc'['a']",
            "TypeError: string indices must be integers, not 'str'",
        ),
        // stale (3.9/3.10): 3.11 named the method, like list/tuple/deque.
        (
            "range(3).index(9)",
            "ValueError: range.index(x): x not in range",
        ),
        // stale (3.9-3.11): 3.12 added the surplus count…
        (
            "a, b = [1, 2, 3]",
            "ValueError: too many values to unpack (expected 2, got 3)",
        ),
        (
            "a, b = (1, 2, 3)",
            "ValueError: too many values to unpack (expected 2, got 3)",
        ),
        (
            "a, b = {1: 0, 2: 0, 3: 0}",
            "ValueError: too many values to unpack (expected 2, got 3)",
        ),
        // …but ONLY for an exact list/tuple/dict. A str, set, range, generator
        // or list SUBCLASS keeps the bare form, and adding the count everywhere
        // would be its own fabrication.
        (
            "a, b = 'xyz'",
            "ValueError: too many values to unpack (expected 2)",
        ),
        (
            "a, b = {1, 2, 3}",
            "ValueError: too many values to unpack (expected 2)",
        ),
        (
            "a, b = range(3)",
            "ValueError: too many values to unpack (expected 2)",
        ),
        (
            "a, b = iter([1, 2, 3])",
            "ValueError: too many values to unpack (expected 2)",
        ),
        (
            "class L(list): pass\na, b = L([1, 2, 3])",
            "ValueError: too many values to unpack (expected 2)",
        ),
        // fabricated: the star form never reported what it got.
        (
            "a, b, *c = []",
            "ValueError: not enough values to unpack (expected at least 2, got 0)",
        ),
        // the non-star short form always reports it, for every iterable.
        (
            "a, b = 'x'",
            "ValueError: not enough values to unpack (expected 2, got 1)",
        ),
        // fabricated: reported `'type' object`, never naming WHICH type.
        (
            "str.nonesuch",
            "AttributeError: type object 'str' has no attribute 'nonesuch'",
        ),
        (
            "int.nonesuch",
            "AttributeError: type object 'int' has no attribute 'nonesuch'",
        ),
        // …an instance still reports its own type, not `type object`.
        (
            "(1).nonesuch",
            "AttributeError: 'int' object has no attribute 'nonesuch'",
        ),
    ];
    for (src, want) in cases {
        assert_eq!(&eval_str(src).expect_err("must raise"), want, "for {src:?}");
    }
}

/// `KeyError.__str__` reprs its single argument, so a message built by hand with
/// the quotes ALREADY in it gets re-quoted: `str(e)` came out as
/// `"'pop from an empty set'"` and `e.args` held the quoted text. Three sites
/// built the string that way and a fourth omitted the quotes entirely, so its
/// uncaught line was missing them. All four now go through `key_error`, which
/// stores the bare key.
///
/// The traceback line and `str(e)` disagree for a `KeyError` — that is the point
/// of the repr — so both are pinned, against CPython 3.14.6.
#[test]
fn key_error_carries_the_bare_key_as_its_argument() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "set().pop()",
            "('pop from an empty set',)",
            "'pop from an empty set'",
        ),
        (
            "{}.popitem()",
            "('popitem(): dictionary is empty',)",
            "'popitem(): dictionary is empty'",
        ),
        ("'{a}'.format()", "('a',)", "'a'"),
        ("{}['k']", "('k',)", "'k'"),
        // A non-string key is stored, and repr'd, as itself.
        ("{1: 2}[3]", "(3,)", "3"),
    ];
    // `g` returns the value's PYTHON repr, which prefers single quotes and
    // switches to double only when the text itself contains an apostrophe —
    // exactly the case for the `KeyError` args, whose repr is already quoted.
    fn py_quoted(s: &str) -> String {
        if s.contains('\'') && !s.contains('"') {
            format!("\"{s}\"")
        } else {
            format!("'{s}'")
        }
    }
    for (src, want_args, want_str) in cases {
        let prog = format!(
            "try:\n    {src}\nexcept KeyError as e:\n    args = repr(e.args)\n    text = str(e)\n"
        );
        assert_eq!(g(&prog, "args"), py_quoted(want_args), "args for {src}");
        assert_eq!(g(&prog, "text"), py_quoted(want_str), "str for {src}");
    }
}

/// Rust primitives that carry the reference's NAME but not its behaviour.
///
/// Each row is a place where a Rust `f64`/`str`/parse method was wired to a
/// Python builtin of the same name. The Rust method type-checks, returns a
/// plausible value, and is wrong — nothing in the build can see it, and the
/// probe that catches it has to compare against the reference.
///
/// Expected values are CPython 3.14.6's, captured with `python3 -c`.
#[test]
fn rust_lookalikes_do_not_stand_in_for_the_reference_semantics() {
    // `f64::sqrt`/`ln`/`asin`/… answer NaN or an infinity where CPython raises.
    // Every one of these used to return a value.
    let raises: &[(&str, &str)] = &[
        (
            "import math\nmath.sqrt(-1)",
            "ValueError: expected a nonnegative input, got -1.0",
        ),
        (
            "import math\nmath.sqrt(float('-inf'))",
            "ValueError: expected a nonnegative input, got -inf",
        ),
        (
            "import math\nmath.log(0)",
            "ValueError: expected a positive input",
        ),
        (
            "import math\nmath.log(-1)",
            "ValueError: expected a positive input",
        ),
        (
            "import math\nmath.log10(0)",
            "ValueError: expected a positive input",
        ),
        (
            "import math\nmath.log2(0)",
            "ValueError: expected a positive input",
        ),
        // The BASE has the same domain as the value…
        (
            "import math\nmath.log(8, 0)",
            "ValueError: expected a positive input",
        ),
        // …and base 1 divides by ln(1), which CPython reports as the division.
        (
            "import math\nmath.log(8, 1)",
            "ZeroDivisionError: division by zero",
        ),
        (
            "import math\nmath.log1p(-1)",
            "ValueError: expected argument value > -1, got -1.0",
        ),
        (
            "import math\nmath.asin(2)",
            "ValueError: expected a number in range from -1 up to 1, got 2.0",
        ),
        (
            "import math\nmath.acos(2)",
            "ValueError: expected a number in range from -1 up to 1, got 2.0",
        ),
        (
            "import math\nmath.acosh(0)",
            "ValueError: expected argument value not less than 1, got 0.0",
        ),
        (
            "import math\nmath.atanh(1)",
            "ValueError: expected a number between -1 and 1, got 1.0",
        ),
        (
            "import math\nmath.gamma(0)",
            "ValueError: expected a noninteger or positive integer, got 0.0",
        ),
        (
            "import math\nmath.lgamma(-1)",
            "ValueError: expected a noninteger or positive integer, got -1.0",
        ),
        // `math.pow` keeps the flat wording — only the one-argument functions
        // were reworded in 3.14, and copying the new text here would be a
        // fabrication of its own.
        (
            "import math\nmath.pow(0.0, -1)",
            "ValueError: math domain error",
        ),
        (
            "import math\nmath.pow(-1.0, 0.5)",
            "ValueError: math domain error",
        ),
        // A finite argument that overflows to infinity is a RANGE error.
        (
            "import math\nmath.exp(10000)",
            "OverflowError: math range error",
        ),
        (
            "import math\nmath.exp2(10000)",
            "OverflowError: math range error",
        ),
        (
            "import math\nmath.cosh(10000)",
            "OverflowError: math range error",
        ),
        (
            "import math\nmath.gamma(1e300)",
            "OverflowError: math range error",
        ),
        // `BigInt::parse_bytes` PANICS outside radix 2..=36 — this aborted the
        // whole interpreter rather than raising.
        (
            "int('12', 40)",
            "ValueError: int() base must be >= 2 and <= 36, or 0",
        ),
        (
            "int('12', 1)",
            "ValueError: int() base must be >= 2 and <= 36, or 0",
        ),
        // `str::replace('_', "")` deleted underscores CPython rejects.
        (
            "int('_1')",
            "ValueError: invalid literal for int() with base 10: '_1'",
        ),
        (
            "int('1_')",
            "ValueError: invalid literal for int() with base 10: '1_'",
        ),
        (
            "int('1__2')",
            "ValueError: invalid literal for int() with base 10: '1__2'",
        ),
        // Base 0 means "read the prefix"; a leading zero without one is the
        // Python-2 octal spelling and is rejected. The message names the base as
        // PASSED, not the auto-detected one.
        (
            "int('010', 0)",
            "ValueError: invalid literal for int() with base 0: '010'",
        ),
        (
            "int(b'x')",
            "ValueError: invalid literal for int() with base 10: b'x'",
        ),
        // …and a non-DECIMAL numeric (Nl `Ⅰ`, No `²`) is still not a digit, so
        // widening to "any numeric character" would be its own wrong answer.
        (
            "int('\u{2160}', 16)",
            "ValueError: invalid literal for int() with base 16: '\u{2160}'",
        ),
        (
            "int('\u{00b2}')",
            "ValueError: invalid literal for int() with base 10: '\u{00b2}'",
        ),
    ];
    for (src, want) in raises {
        assert_eq!(&eval_str(src).expect_err("must raise"), want, "for {src:?}");
    }

    // Values, not errors. A NaN ARGUMENT propagates through the domain guards
    // rather than tripping them, so adding the guards must not have made these
    // raise.
    let values: &[(&str, &str)] = &[
        ("import math\nx = math.sqrt(float('nan'))", "nan"),
        ("import math\nx = math.asin(float('nan'))", "nan"),
        ("import math\nx = math.log(float('inf'))", "inf"),
        ("import math\nx = math.gamma(float('inf'))", "inf"),
        ("import math\nx = math.exp(float('inf'))", "inf"),
        ("import math\nx = math.exp(-10000)", "0.0"),
        ("import math\nx = math.sqrt(4)", "2.0"),
        ("import math\nx = math.log(8, 2)", "3.0"),
        // IEEE-754 hypot: an infinite coordinate wins over a NaN one. Squaring
        // and summing propagated the NaN instead.
        (
            "import math\nx = math.hypot(float('inf'), float('nan'))",
            "inf",
        ),
        ("import math\nx = math.hypot(3, 4)", "5.0"),
        // `char::is_whitespace` omits U+001C..U+001F, which CPython counts as
        // whitespace for str (but NOT for bytes, which is ASCII-only).
        ("x = '\\x1c\\x1d'.lstrip()", "''"),
        (
            "x = '\\x0b\\x0c\\x1c\\x1d\\x1e\\x85\\xa0 y '.strip()",
            "'y'",
        ),
        ("x = 'a\\x1cb'.split()", "['a', 'b']"),
        ("x = 'a\\x1cb'.rsplit()", "['a', 'b']"),
        ("x = '  a\\x1cb  '.split(None, 1)", "['a', 'b  ']"),
        ("x = b'a\\x1cb'.split()", "[b'a\\x1cb']"),
        // Any Unicode DECIMAL character is a digit to `int()`; `to_digit` sees
        // only ASCII, so these were rejected outright.
        ("x = int('\\u0661\\u0662')", "12"),
        ("x = int('\\u0661\\u0662', 16)", "18"),
        ("x = int('\\u0661\\u0660', 2)", "2"),
        ("x = int(b'12')", "12"),
        ("x = int(bytearray(b'ff'), 16)", "255"),
        ("x = int(memoryview(b'12'))", "12"),
        ("x = int('0x_10', 16)", "16"),
        ("x = int('00', 0)", "0"),
    ];
    for (src, want) in values {
        assert_eq!(&g(src, "x"), want, "for {src:?}");
    }
}

/// Codec diagnostics name the offending character or byte, WHERE it is, and why
/// the codec rejected it. pythonrs emitted only the first fragment —
/// `'ascii' codec can't encode character '\xe9'` and, worse, a bare
/// `'ascii' codec can't decode byte` with no byte at all — neither of which any
/// CPython produces. A run of un-encodable characters is one report naming a
/// span, not one report per character.
///
/// Expected values are CPython 3.14.6's, captured with `python3 -c`.
#[test]
fn codec_errors_name_the_position_and_the_reason() {
    let raises: &[(&str, &str)] = &[
        (
            "'\\u00e9'.encode('ascii')",
            "UnicodeEncodeError: 'ascii' codec can't encode character '\\xe9' in position 0: \
             ordinal not in range(128)",
        ),
        // A RUN is merged into one report naming the span, and switches to the
        // plural "characters".
        (
            "'\\u00e9\\u00e9a'.encode('ascii')",
            "UnicodeEncodeError: 'ascii' codec can't encode characters in position 0-1: \
             ordinal not in range(128)",
        ),
        (
            "'a\\u20ac'.encode('latin-1')",
            "UnicodeEncodeError: 'latin-1' codec can't encode character '\\u20ac' in position 1: \
             ordinal not in range(256)",
        ),
        // Above U+FFFF the escape widens to \\U........
        (
            "'\\U0001F600'.encode('ascii')",
            "UnicodeEncodeError: 'ascii' codec can't encode character '\\U0001f600' in position 0: \
             ordinal not in range(128)",
        ),
        (
            "b'a\\xffb'.decode('ascii')",
            "UnicodeDecodeError: 'ascii' codec can't decode byte 0xff in position 1: \
             ordinal not in range(128)",
        ),
        // utf-8 distinguishes a bad lead byte, a bad continuation, and a
        // truncated sequence — three different reasons.
        (
            "b'\\xff\\xfe'.decode('utf-8')",
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 0: \
             invalid start byte",
        ),
        (
            "b'\\xc3\\x28'.decode('utf-8')",
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xc3 in position 0: \
             invalid continuation byte",
        ),
        (
            "b'\\xc3'.decode('utf-8')",
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xc3 in position 0: \
             unexpected end of data",
        ),
        (
            "b'\\xe2\\x82'.decode('utf-8')",
            "UnicodeDecodeError: 'utf-8' codec can't decode bytes in position 0-1: \
             unexpected end of data",
        ),
        // An unknown codec fell through to utf-8 and silently succeeded on the
        // DECODE side; encode already raised.
        ("b'abc'.decode('nope')", "LookupError: unknown encoding: nope"),
        ("'abc'.encode('nope')", "LookupError: unknown encoding: nope"),
    ];
    for (src, want) in raises {
        assert_eq!(&eval_str(src).expect_err("must raise"), want, "for {src:?}");
    }

    // The non-strict handlers still work, and each applies PER CHARACTER even
    // though the error is reported per run.
    let values: &[(&str, &str)] = &[
        ("x = '\\u00e9'.encode('ascii', 'ignore')", "b''"),
        ("x = '\\u00e9\\u00e9a'.encode('ascii', 'replace')", "b'??a'"),
        (
            "x = '\\u00e9'.encode('ascii', 'backslashreplace')",
            "b'\\\\xe9'",
        ),
        (
            "x = '\\u00e9'.encode('ascii', 'xmlcharrefreplace')",
            "b'&#233;'",
        ),
        (
            "x = '\\u00e9'.encode('ascii', 'namereplace')",
            "b'\\\\N{LATIN SMALL LETTER E WITH ACUTE}'",
        ),
        ("x = b'\\xff'.decode('ascii', 'replace')", "'�'"),
        ("x = b'\\xff'.decode('ascii', 'ignore')", "''"),
        // A recognized codec still decodes; the LookupError above must not have
        // swallowed the aliases.
        ("x = b'\\xc3\\xa9'.decode()", "'é'"),
        ("x = b'abc'.decode('UTF8')", "'abc'"),
        ("x = b'abc'.decode('cp65001')", "'abc'"),
    ];
    for (src, want) in values {
        assert_eq!(&g(src, "x"), want, "for {src:?}");
    }
}
