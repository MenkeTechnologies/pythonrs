//! Headless stdlib tests: import a stdlib module, bind a global from it, and read
//! that global's `repr` back from the host. Expected values are what CPython
//! produces for the same call.
//!
//! Modules provided natively by pythonrs (`collections`, `bytes`/`bytearray`,
//! file I/O) run in every build. Modules that used to have hand-rolled shadows
//! (`json`/`os`/`random`/`string`/`itertools`/`functools`) now come from the real
//! CPython stdlib through the `stdlib-ffi` bridge; their tests are gated on that
//! feature (compiled out of the default, no-libpython build where those modules
//! intentionally do not exist) and run against CPython under
//! `PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 cargo test --features stdlib-ffi`.

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

/// The `TypeError` message for running `src` (which must fail).
fn err(src: &str) -> String {
    eval_str(src).expect_err("program should raise")
}

#[test]
fn comparison_typeerror_names_the_operator() {
    // The `'<' not supported …` message must reflect the actual operator, and the
    // OUTER operator even for a failing list-element compare (CPython behavior).
    assert_eq!(
        err("x = 1 < 'a'"),
        "TypeError: '<' not supported between instances of 'int' and 'str'"
    );
    assert_eq!(
        err("x = 1 <= 'a'"),
        "TypeError: '<=' not supported between instances of 'int' and 'str'"
    );
    assert_eq!(
        err("x = 1 >= 'a'"),
        "TypeError: '>=' not supported between instances of 'int' and 'str'"
    );
    assert_eq!(
        err("x = [1] >= ['a']"),
        "TypeError: '>=' not supported between instances of 'int' and 'str'"
    );
}

#[test]
fn builtin_method_arity_is_enforced() {
    // Fixed-arity builtin methods/functions reject wrong positional counts with
    // CPython's exact wording (METH_O / METH_NOARGS / METH_VARARGS forms).
    assert_eq!(
        err("[].append(1, 2)"),
        "TypeError: list.append() takes exactly one argument (2 given)"
    );
    assert_eq!(
        err("[].clear(1)"),
        "TypeError: list.clear() takes no arguments (1 given)"
    );
    assert_eq!(
        err("[].pop(1, 2)"),
        "TypeError: pop expected at most 1 argument, got 2"
    );
    assert_eq!(
        err("[].insert(1, 2, 3)"),
        "TypeError: insert expected 2 arguments, got 3"
    );
    assert_eq!(
        err("import math\nx = math.sqrt(1, 2)"),
        "TypeError: math.sqrt() takes exactly one argument (2 given)"
    );
    assert_eq!(
        err("{}.get(1, 2, 3)"),
        "TypeError: get expected at most 2 arguments, got 3"
    );
    assert_eq!(
        err("set().add(1, 2)"),
        "TypeError: set.add() takes exactly one argument (2 given)"
    );
    assert_eq!(
        err("(1,).count(1, 2)"),
        "TypeError: tuple.count() takes exactly one argument (2 given)"
    );
    // A frozenset mutator is still an AttributeError, not an arity error.
    assert_eq!(
        err("frozenset().add(1, 2)"),
        "AttributeError: 'frozenset' object has no attribute 'add'"
    );
}

#[cfg(feature = "stdlib-ffi")]
#[test]
fn dataclass_user_dunders_do_not_reenter_host() {
    // Regression: a `@dataclass` (a CPython-bridge class) with user-defined
    // dunders panicked with `RefCell already borrowed` on `print`/`repr`/
    // arithmetic/`len`/`bool`/`in`, because the operation ran CPython (which calls
    // the pythonrs dunder back) while the host was borrowed. Each now dispatches
    // outside the borrow.
    let src = "\
from dataclasses import dataclass
@dataclass
class V:
    x: int
    def __repr__(self): return f'V{self.x}'
    def __neg__(self): return V(-self.x)
    def __add__(self, o): return V(self.x + o.x)
    def __len__(self): return self.x
    def __bool__(self): return self.x > 0
    def __contains__(self, i): return i == self.x
r = [repr([V(1), V(2)]), repr(-V(5)), repr(V(1) + V(2)), len(V(4)), bool(V(0)), 3 in V(3)]";
    assert_eq!(g(src, "r"), "['[V1, V2]', 'V-5', 'V3', 4, False, True]");
}

#[cfg(feature = "stdlib-ffi")]
#[test]
fn json_dumps_loads_roundtrip() {
    // Insertion order preserved; None/bool lowered to null/true; int stays int.
    assert_eq!(
        g(
            "import json\nx = json.dumps({\"b\": 2, \"a\": [1, None, True]})",
            "x"
        ),
        "'{\"b\": 2, \"a\": [1, null, true]}'"
    );
    assert_eq!(
        g(
            "import json\nx = json.loads('{\"k\": [1, 2.5, false, null]}')",
            "x"
        ),
        "{'k': [1, 2.5, False, None]}"
    );
}

#[cfg(feature = "stdlib-ffi")]
#[test]
fn enum_member_container_membership_uses_python_equality() {
    // Regression: `PyHost::equal` compared two `Foreign` handles by raw handle id,
    // so an enum member fetched twice read as unequal — `member in (A, B)`,
    // `.index`, `.count`, and list/tuple `==` over foreign elements all failed
    // while `==`/`is` on the same members succeeded (a different code path). They
    // now route through CPython's identity-then-`__eq__` (`ffi::foreign_eq`).
    let src = "\
from enum import Enum, auto
class S(Enum):
    A = auto()
    B = auto()
    C = auto()
x = S.A
r = [
    x in (S.A, S.B),
    x in [S.A, S.B],
    S.C in (S.A, S.B),
    (S.A, S.B).index(S.A),
    [S.A, S.A, S.B].count(S.A),
    [S.A, S.B] == [S.A, S.B],
]";
    assert_eq!(g(src, "r"), "[True, True, False, 0, 2, True]");
}

#[cfg(feature = "stdlib-ffi")]
#[test]
fn foreign_objects_as_set_and_dict_keys() {
    // Regression: a CPython Foreign object (enum member, Decimal, datetime, …) had
    // no `to_key` arm, so ANY set/dict keyed by one raised `unhashable type`. It
    // now keys by CPython's hash with value-equal collapse (`prepare_key` +
    // `ffi::foreign_eq`), matching CPython dict/set semantics.
    let src = "\
from enum import Enum, auto
from decimal import Decimal
class C(Enum):
    A = auto()
    B = auto()
A = C.A
d1 = Decimal('1.5')
s = {C.A, C.B}
r = [
    A in {C.A, C.B},          # set membership
    len({C.A, C.B, C.A}),     # dedup within one construction
    {C.A: 1, C.B: 2}[C.B],    # dict lookup
    d1 in {Decimal('1.5')},   # fresh value-equal handle collapses on lookup
    len({d1, Decimal('1.5')}),# dedup of equal fresh handles
    C.B in s,                 # membership against a bound set
    hash(C.A) == hash(C.A),
    hash(d1) == hash(Decimal('1.5')),
]";
    assert_eq!(
        g(src, "r"),
        "[True, 2, 2, True, 1, True, True, True]"
    );
}

#[cfg(feature = "stdlib-ffi")]
#[test]
fn foreign_vs_native_equality_and_ordering_in_containers() {
    // Regression (cat. 2 + 3): IntEnum-vs-int equality inside `in`/`.index`, and
    // ordering of Foreign elements inside a list/tuple sort or `<`, both failed
    // (False / TypeError). Now route through CPython `__eq__` / rich comparison.
    let src = "\
from enum import IntEnum
class Pri(IntEnum):
    LOW = 1
    MID = 2
    HIGH = 3
r = [
    Pri.HIGH in [1, 2, 3],                              # IntEnum member == int
    3 in (Pri.LOW, Pri.HIGH),                           # int == IntEnum member
    [1, 2, 3].index(Pri.MID),
    sorted([Pri.HIGH, Pri.LOW, Pri.MID]),               # foreign elements order
    [Pri.LOW] < [Pri.HIGH],                             # sequence compare
    (Pri.LOW, Pri.HIGH) < (Pri.MID, Pri.LOW),
]";
    assert_eq!(
        g(src, "r"),
        "[True, True, 1, [<Pri.LOW: 1>, <Pri.MID: 2>, <Pri.HIGH: 3>], True, True]"
    );
}

#[cfg(feature = "stdlib-ffi")]
#[test]
fn itertools_eager_combinatorics() {
    assert_eq!(
        g(
            "import itertools\nx = list(itertools.chain([1, 2], [3, 4]))",
            "x"
        ),
        "[1, 2, 3, 4]"
    );
    assert_eq!(
        g(
            "import itertools\nx = list(itertools.combinations([1, 2, 3], 2))",
            "x"
        ),
        "[(1, 2), (1, 3), (2, 3)]"
    );
    assert_eq!(
        g(
            "import itertools\nx = list(itertools.permutations([1, 2], 2))",
            "x"
        ),
        "[(1, 2), (2, 1)]"
    );
}

#[cfg(feature = "stdlib-ffi")]
#[test]
fn functools_reduce() {
    assert_eq!(
        g(
            "import functools\nx = functools.reduce(lambda a, b: a + b, [1, 2, 3, 4], 100)",
            "x"
        ),
        "110",
    );
}

#[cfg(feature = "stdlib-ffi")]
#[test]
fn os_path_posix() {
    assert_eq!(
        g("import os\nx = os.path.join('a', 'b', 'c')", "x"),
        "'a/b/c'"
    );
    assert_eq!(
        g("import os\nx = os.path.basename('/x/y/z.txt')", "x"),
        "'z.txt'"
    );
    assert_eq!(
        g("import os\nx = os.path.splitext('f.tar.gz')", "x"),
        "('f.tar', '.gz')"
    );
}

#[cfg(feature = "stdlib-ffi")]
#[test]
fn string_constants() {
    assert_eq!(
        g("import string\nx = string.ascii_lowercase", "x"),
        "'abcdefghijklmnopqrstuvwxyz'"
    );
    assert_eq!(g("import string\nx = string.digits", "x"), "'0123456789'");
}

#[cfg(feature = "stdlib-ffi")]
#[test]
fn random_is_deterministic_after_seed() {
    // pythonrs's own PRNG (not CPython-bit-identical), but stable across runs for
    // a fixed seed — so two seeded sequences in one program must match.
    let src = "import random\n\
               random.seed(42)\n\
               a = [random.randint(1, 100) for _ in range(5)]\n\
               random.seed(42)\n\
               b = [random.randint(1, 100) for _ in range(5)]\n\
               same = a == b";
    assert_eq!(g(src, "same"), "True");
}

// ── collections (host-backed types) ──────────────────────────────────────────

#[test]
fn collections_deque_ops() {
    assert_eq!(
        g(
            "from collections import deque\nd = deque([1,2,3])\nd.appendleft(0)\nd.append(4)\nd.rotate(1)\nx = d",
            "x"
        ),
        "deque([4, 0, 1, 2, 3])"
    );
    // maxlen drops from the opposite end on overflow.
    assert_eq!(
        g(
            "from collections import deque\nd = deque([1,2,3], 3)\nd.append(4)\nx = list(d)",
            "x"
        ),
        "[2, 3, 4]"
    );
    assert_eq!(
        g(
            "from collections import deque\nd = deque([1,2])\nx = d.popleft()",
            "x"
        ),
        "1"
    );
    // `maxlen` passed as a keyword (not just positional) is honored on construction
    // and by later appends; `.maxlen` reads it back (`None` when unbounded).
    assert_eq!(
        g(
            "from collections import deque\nd = deque([1,2,3], maxlen=4)\nd.appendleft(0)\nd.append(4)\nx = list(d)",
            "x"
        ),
        "[1, 2, 3, 4]"
    );
    assert_eq!(
        g(
            "from collections import deque\nx = deque([1,2,3], maxlen=2).maxlen",
            "x"
        ),
        "2"
    );
    assert_eq!(
        g(
            "from collections import deque\nx = deque([1,2,3]).maxlen",
            "x"
        ),
        "None"
    );
    assert_eq!(
        g(
            "from collections import deque\nx = list(deque(iterable=[9,8,7], maxlen=2))",
            "x"
        ),
        "[8, 7]"
    );
}

#[test]
fn collections_counter() {
    assert_eq!(
        g(
            "from collections import Counter\nc = Counter('aabbbc')\nx = c.most_common(2)",
            "x"
        ),
        "[('b', 3), ('a', 2)]"
    );
    // Missing keys read as 0 (Counter.__missing__), not KeyError.
    assert_eq!(
        g(
            "from collections import Counter\nx = Counter('ab')['z']",
            "x"
        ),
        "0"
    );
    assert_eq!(
        g(
            "from collections import Counter\nx = isinstance(Counter(), dict)",
            "x"
        ),
        "True"
    );
}

#[test]
fn collections_defaultdict() {
    assert_eq!(
        g(
            "from collections import defaultdict\ndd = defaultdict(list)\ndd['k'].append(1)\ndd['k'].append(2)\nx = dd['k']",
            "x"
        ),
        "[1, 2]"
    );
    assert_eq!(
        g(
            "from collections import defaultdict\ndd = defaultdict(int)\ndd['a'] += 5\nx = dd['a']",
            "x"
        ),
        "5"
    );
}

#[test]
fn collections_ordereddict_move_to_end() {
    assert_eq!(
        g(
            "from collections import OrderedDict\nod = OrderedDict([('a',1),('b',2)])\nod.move_to_end('a')\nx = list(od.items())",
            "x"
        ),
        "[('b', 2), ('a', 1)]"
    );
}

#[test]
fn collections_namedtuple() {
    // `repr` is computed INSIDE the program: a namedtuple is now a real Python
    // class (the vendored `collections` builds it), so its `__repr__` is Python
    // code — and `PyHost::repr_of`, which the helper above uses, is a Rust-side
    // formatter that cannot call back into the interpreter.
    assert_eq!(
        g(
            "from collections import namedtuple\nPt = namedtuple('Point', ['x','y'])\nx = repr(Pt(1, 2))",
            "x"
        ),
        "'Point(x=1, y=2)'"
    );
    // Field access, indexing, and tuple-ness.
    assert_eq!(
        g(
            "from collections import namedtuple\nPt = namedtuple('Point', 'x y')\np = Pt(3, 4)\nx = p.y + p[0]",
            "x"
        ),
        "7"
    );
    assert_eq!(
        g(
            "from collections import namedtuple\nPt = namedtuple('P', 'a b')\nx = isinstance(Pt(1,2), tuple)",
            "x"
        ),
        "True"
    );
}

// ── functools.partial / lru_cache ────────────────────────────────────────────

#[cfg(feature = "stdlib-ffi")]
#[test]
fn functools_partial() {
    assert_eq!(
        g(
            "import functools\nadd = functools.partial(lambda a, b: a + b, 10)\nx = add(5)",
            "x"
        ),
        "15"
    );
    // A bound keyword arg is supplied at call time from the partial.
    assert_eq!(
        g(
            "import functools\nf = functools.partial(lambda a, b: a - b, b=3)\nx = f(10)",
            "x"
        ),
        "7"
    );
}

#[cfg(feature = "stdlib-ffi")]
#[test]
fn functools_lru_cache() {
    // Bare form: default maxsize 128; cache_info reports hits/misses/maxsize/currsize.
    assert_eq!(
        g(
            "import functools\nsq = functools.lru_cache(lambda n: n * n)\nsq(3)\nsq(3)\nsq(4)\nx = sq.cache_info()",
            "x"
        ),
        "CacheInfo(hits=1, misses=2, maxsize=128, currsize=2)"
    );
    // Parameterized decorator form carries the maxsize through the partial.
    assert_eq!(
        g(
            "import functools\nsq = functools.lru_cache(maxsize=2)(lambda n: n * n)\nsq(1)\nsq(2)\nsq(3)\nx = sq.cache_info()",
            "x"
        ),
        "CacheInfo(hits=0, misses=3, maxsize=2, currsize=2)"
    );
    // Cached values are correct.
    assert_eq!(
        g(
            "import functools\nsq = functools.lru_cache(lambda n: n * n)\nx = sq(5) + sq(5)",
            "x"
        ),
        "50"
    );
}

// ── bytes / bytearray ────────────────────────────────────────────────────────

#[test]
fn bytes_methods() {
    assert_eq!(g("x = b'\\xff\\x00'.hex()", "x"), "'ff00'");
    assert_eq!(g("x = b'hi'.decode()", "x"), "'hi'");
    assert_eq!(g("x = b'abcabc'.index(b'c')", "x"), "2");
    assert_eq!(g("x = b'abcabc'.count(b'bc')", "x"), "2");
    assert_eq!(g("x = bytes([104, 105]).decode()", "x"), "'hi'");
    // str.encode -> bytes -> decode round-trips through UTF-8 (non-ASCII).
    assert_eq!(
        g("s = 'ni\\u00f1o'\nx = (s.encode().decode() == s)", "x"),
        "True"
    );
}

#[test]
fn bytearray_mutation() {
    assert_eq!(
        g("ba = bytearray(b'ab')\nba.append(99)\nx = ba.decode()", "x"),
        "'abc'"
    );
    assert_eq!(
        g(
            "ba = bytearray(b'ab')\nba.extend(b'cd')\nx = ba.decode()",
            "x"
        ),
        "'abcd'"
    );
    assert_eq!(g("x = b'a' == bytearray(b'a')", "x"), "True");
}

// ── file I/O (`open`, read/write/with) ───────────────────────────────────────

/// A unique temp path for a file-I/O test (removed by the caller afterward).
fn tmp_path(tag: &str) -> std::path::PathBuf {
    let pid = std::process::id();
    std::env::temp_dir().join(format!("pythonrs_io_{tag}_{pid}.txt"))
}

#[test]
fn file_write_read_with() {
    let path = tmp_path("wr");
    let p = path.to_str().unwrap();
    // `with open(...) as f:` must drive __enter__/__exit__ and close on exit.
    let src = format!(
        "with open('{p}', 'w') as f:\n    f.write('line1\\nline2\\n')\nwith open('{p}') as f:\n    x = f.read()\n"
    );
    eval_str(&src).expect("file program should run");
    let got = host::with_host(|h| {
        let v = h.read_global("x").expect("x unbound");
        h.repr_of(&v)
    });
    let _ = std::fs::remove_file(&path);
    assert_eq!(got, "'line1\\nline2\\n'");
}

#[test]
fn file_iterate_and_readlines() {
    let path = tmp_path("lines");
    let p = path.to_str().unwrap();
    let src = format!(
        "f = open('{p}', 'w')\nf.write('a\\nb\\nc\\n')\nf.close()\nf = open('{p}')\nx = f.readlines()\nf.close()\n"
    );
    eval_str(&src).expect("file program should run");
    let got = host::with_host(|h| {
        let v = h.read_global("x").expect("x unbound");
        h.repr_of(&v)
    });
    let _ = std::fs::remove_file(&path);
    // readlines keeps the trailing newline on each line.
    assert_eq!(got, "['a\\n', 'b\\n', 'c\\n']");
}

#[test]
fn file_for_loop_lines() {
    let path = tmp_path("forloop");
    let p = path.to_str().unwrap();
    let src = format!(
        "f = open('{p}', 'w')\nf.write('x\\ny\\n')\nf.close()\nout = []\nfor line in open('{p}'):\n    out.append(line.strip())\nx = out\n"
    );
    eval_str(&src).expect("file program should run");
    let got = host::with_host(|h| {
        let v = h.read_global("x").expect("x unbound");
        h.repr_of(&v)
    });
    let _ = std::fs::remove_file(&path);
    assert_eq!(got, "['x', 'y']");
}

// ── CPython stdlib FFI bridge (real re / hashlib / json C accelerators) ───────

/// Exercise the `stdlib-ffi` bridge end to end: import a pure module and two
/// C-accelerator modules, run a call on each, and marshal the result back. Values
/// are the exact CPython outputs (verified against `python3`).
#[cfg(feature = "stdlib-ffi")]
#[test]
fn ffi_c_accelerators_marshal_back() {
    // _sre: findall returns a real list of matched substrings.
    assert_eq!(
        g("import re\nx = re.findall(r'\\d+', 'a1b22')", "x"),
        "['1', '22']"
    );
    // _hashlib: sha256 hex digest of b"abc".
    assert_eq!(
        g(
            "import hashlib\nx = hashlib.sha256(b'abc').hexdigest()",
            "x"
        ),
        "'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad'"
    );
    // _json: dumps a list back to its compact-with-spaces text form.
    assert_eq!(
        g("import json\nx = json.dumps([1, 2, 3])", "x"),
        "'[1, 2, 3]'"
    );
}

/// A pythonrs lambda passed as a CPython stdlib callback must call back into
/// fusevm (`functools.reduce` folding a fusevm closure over CPython data).
#[cfg(feature = "stdlib-ffi")]
#[test]
fn ffi_reverse_callback_into_fusevm() {
    assert_eq!(
        g(
            "import functools\nx = functools.reduce(lambda a, b: a + b, [1, 2, 3, 4], 100)",
            "x"
        ),
        "110"
    );
}

/// A non-callable pythonrs object passed *into* a CPython stdlib call must
/// marshal to its CPython equivalent by value. `list`/`dict`/`tuple`/`set`/`str`/
/// `bytes`/`int`/`float`/`None` already crossed; this adds `range`, `complex`,
/// `collections.deque`, and `frozenset` — all previously rejected with
/// `cannot pass '<type>' to a CPython stdlib call`.
#[cfg(feature = "stdlib-ffi")]
#[test]
fn ffi_marshals_value_types_into_cpython_calls() {
    // range → CPython range.
    assert_eq!(
        g(
            "import functools\nx = functools.reduce(lambda a, b: a + b, range(1, 6))",
            "x"
        ),
        "15"
    );
    // complex → CPython complex.
    assert_eq!(g("import cmath\nx = cmath.sqrt(complex(-1, 0))", "x"), "1j");
    // collections.deque → CPython deque.
    assert_eq!(
        g(
            "import collections, functools\nx = functools.reduce(lambda a, b: a + b, collections.deque([1, 2, 3]))",
            "x"
        ),
        "6"
    );
    // frozenset → CPython frozenset (fold is order-independent).
    assert_eq!(
        g(
            "import functools\nx = functools.reduce(lambda a, b: a + b, frozenset([1, 2, 3]))",
            "x"
        ),
        "6"
    );
    // nested dict/list by value through json.
    assert_eq!(
        g(
            "import json\nx = json.dumps({\"a\": [1, 2], \"b\": {\"c\": 3}})",
            "x"
        ),
        "'{\"a\": [1, 2], \"b\": {\"c\": 3}}'"
    );
}

/// An in-place stdlib mutator (`heapq.heapify`, `random.shuffle`,
/// `struct.pack_into`) must reflect its mutation back into the pythonrs object —
/// by-value marshaling copies the argument, so without write-back the mutation
/// was silently lost. Aliases to the same object must observe it too (the heap
/// slot is overwritten in place, never reallocated).
#[cfg(feature = "stdlib-ffi")]
#[test]
fn ffi_inplace_mutation_writes_back() {
    // heapq.heapify mutates the list in place.
    assert_eq!(
        g("import heapq\nh = [5, 3, 8, 1, 2]\nheapq.heapify(h)", "h"),
        "[1, 2, 8, 3, 5]"
    );
    // an alias sees the same mutation.
    assert_eq!(
        g(
            "import heapq\nh = [5, 3, 8, 1, 2]\ng = h\nheapq.heapify(h)",
            "g"
        ),
        "[1, 2, 8, 3, 5]"
    );
    // random.shuffle (Mersenne-Twister stable across CPython versions).
    assert_eq!(
        g(
            "import random\nrandom.seed(42)\nx = list(range(10))\nrandom.shuffle(x)",
            "x"
        ),
        "[7, 3, 2, 8, 5, 6, 9, 4, 0, 1]"
    );
    // struct.pack_into writes into a bytearray in place.
    assert_eq!(
        g(
            "import struct\nb = bytearray(4)\nstruct.pack_into(\">I\", b, 0, 1000)\nr = list(b)",
            "r"
        ),
        "[0, 0, 3, 232]"
    );
}

/// A stdlib call marshaled purely by value (`heapq.heapify`: list in, `None` out,
/// mutation written back by value) must not allocate a `Foreign` side-table slot
/// per iteration — only the one-time module handle is stored. This bounds the
/// side-table for the value-marshaled churn path (the write-back marshaler never
/// calls `store`). Foreign-*returning* churn (e.g. `re.match` match objects) is a
/// separate, host-arena-lifetime matter documented in FFI_STDLIB.md.
#[cfg(feature = "stdlib-ffi")]
#[test]
fn ffi_value_marshaled_churn_is_bounded() {
    let before = pythonrs::ffi::table_len();
    eval_str(
        "import heapq\nfor i in range(2000):\n    h = [5, 3, 8, 1, 2, i]\n    heapq.heapify(h)",
    )
    .unwrap();
    let grew = pythonrs::ffi::table_len() - before;
    // 2000 iterations; a per-iteration leak would add ~2000. A small constant
    // (module handles, incl. any from other tests sharing the process) is fine.
    assert!(
        grew < 100,
        "value-marshaled churn grew the side-table by {grew} over 2000 iterations (expected a small constant, not O(iters))"
    );
}

/// Binary / comparison / unary operators where an operand is a CPython `Foreign`
/// object route through the bridge to the real CPython operation, so stdlib
/// arithmetic (`date + timedelta`, `Decimal + Decimal`, `Fraction + Fraction`),
/// comparisons (`date < date`), the `binop`-opcode ops (`Decimal % Decimal`),
/// and unary `abs` all match CPython 3.14.6 byte-for-byte.
#[cfg(feature = "stdlib-ffi")]
#[test]
fn ffi_foreign_operator_dispatch() {
    // `+`: date + timedelta → date (result kept as a fresh Foreign, repr from CPython).
    assert_eq!(
        g(
            "import datetime\nx = datetime.date(2024, 2, 28) + datetime.timedelta(days=2)",
            "x"
        ),
        "datetime.date(2024, 3, 1)"
    );
    // `-`: date - date → timedelta; `.days` marshals back by value.
    assert_eq!(
        g(
            "import datetime\nx = (datetime.date(2025, 1, 1) - datetime.date(2024, 1, 1)).days",
            "x"
        ),
        "366"
    );
    // comparison → bool.
    assert_eq!(
        g(
            "import datetime\nx = datetime.date(2024, 1, 1) < datetime.date(2024, 1, 2)",
            "x"
        ),
        "True"
    );
    // `*` with a native int operand marshaled across the boundary.
    assert_eq!(
        g(
            "import datetime\nx = (datetime.timedelta(days=1) * 3).days",
            "x"
        ),
        "3"
    );
    // Decimal exact arithmetic (the whole point of not reimplementing it).
    assert_eq!(
        g(
            "from decimal import Decimal\nx = Decimal('0.1') + Decimal('0.2')",
            "x"
        ),
        "Decimal('0.3')"
    );
    // `%` via the binop-opcode path.
    assert_eq!(
        g(
            "from decimal import Decimal\nx = Decimal('7') % Decimal('3')",
            "x"
        ),
        "Decimal('1')"
    );
    // Fraction arithmetic.
    assert_eq!(
        g(
            "from fractions import Fraction\nx = Fraction(1, 2) + Fraction(1, 3)",
            "x"
        ),
        "Fraction(5, 6)"
    );
    // unary `abs` on a Foreign object.
    assert_eq!(
        g("from decimal import Decimal\nx = abs(Decimal('-5'))", "x"),
        "Decimal('5')"
    );
}

#[test]
fn memoryview_over_bytes() {
    // Construction, len, indexing (incl. negative), and the read-only flag.
    assert_eq!(g("m = memoryview(b'abcde')\nx = len(m)", "x"), "5");
    assert_eq!(g("x = memoryview(b'abcde')[0]", "x"), "97");
    assert_eq!(g("x = memoryview(b'abcde')[-1]", "x"), "101");
    assert_eq!(g("x = memoryview(b'abcde').readonly", "x"), "True");
    // tobytes / hex / tolist over the whole view.
    assert_eq!(g("x = memoryview(b'abcde').tobytes()", "x"), "b'abcde'");
    assert_eq!(g("x = memoryview(b'abcde').hex()", "x"), "'6162636465'");
    assert_eq!(
        g("x = memoryview(b'abcde').tolist()", "x"),
        "[97, 98, 99, 100, 101]"
    );
    // hex with a separator reuses the bytes machinery.
    assert_eq!(
        g("x = memoryview(b'abcde').hex(' ')", "x"),
        "'61 62 63 64 65'"
    );
    // bytes()/list() conversions and iteration.
    assert_eq!(g("x = bytes(memoryview(b'abc'))", "x"), "b'abc'");
    assert_eq!(g("x = list(memoryview(b'abc'))", "x"), "[97, 98, 99]");
    // The descriptor attributes of a 1-D unsigned-byte view.
    assert_eq!(g("x = memoryview(b'abc').obj", "x"), "b'abc'");
    assert_eq!(g("x = memoryview(b'abc').nbytes", "x"), "3");
    assert_eq!(g("x = memoryview(b'abc').format", "x"), "'B'");
    assert_eq!(g("x = memoryview(b'abc').itemsize", "x"), "1");
    assert_eq!(g("x = memoryview(b'abc').ndim", "x"), "1");
    assert_eq!(g("x = memoryview(b'abc').shape", "x"), "(3,)");
    assert_eq!(g("x = memoryview(b'abc').strides", "x"), "(1,)");
    assert_eq!(g("x = memoryview(b'abc').contiguous", "x"), "True");
}

#[test]
fn memoryview_slicing_and_membership() {
    // A contiguous slice is a sub-view sharing the buffer.
    assert_eq!(g("x = memoryview(b'abcde')[1:3].tobytes()", "x"), "b'bc'");
    // A strided slice materializes a fresh view.
    assert_eq!(g("x = memoryview(b'abcde')[::2].tobytes()", "x"), "b'ace'");
    assert_eq!(
        g("x = memoryview(b'abcde')[::-1].tobytes()", "x"),
        "b'edcba'"
    );
    // Byte-value membership and equality against bytes.
    assert_eq!(g("x = 97 in memoryview(b'abc')", "x"), "True");
    assert_eq!(g("x = 200 in memoryview(b'abc')", "x"), "False");
    assert_eq!(g("x = memoryview(b'abc') == b'abc'", "x"), "True");
    assert_eq!(g("x = memoryview(b'abc') == b'abd'", "x"), "False");
    // bool() of an empty vs non-empty view.
    assert_eq!(g("x = bool(memoryview(b''))", "x"), "False");
    assert_eq!(g("x = bool(memoryview(b'a'))", "x"), "True");
}

#[test]
fn memoryview_reflects_bytearray_mutation() {
    // A view over a bytearray sees later mutations to the backing buffer.
    assert_eq!(
        g(
            "ba = bytearray(b'xyz')\nm = memoryview(ba)\nba[0] = 65\nx = m.tobytes()",
            "x"
        ),
        "b'Ayz'"
    );
    assert_eq!(
        g(
            "ba = bytearray(b'xyz')\nm = memoryview(ba)\nx = m.readonly",
            "x"
        ),
        "False"
    );
    assert_eq!(
        g("x = isinstance(memoryview(b'a'), memoryview)", "x"),
        "True"
    );
}

#[test]
fn memoryview_index_and_type_errors() {
    // Out-of-bounds index (CPython's exact dimension-aware message).
    let e = eval_str("x = memoryview(b'abc')[5]").unwrap_err();
    assert!(e.contains("index out of bounds on dimension 1"), "got: {e}");
    // A non-bytes-like constructor argument.
    let e = eval_str("x = memoryview(42)").unwrap_err();
    assert!(
        e.contains("a bytes-like object is required, not 'int'"),
        "got: {e}"
    );
}

// ── vendored stdlib importer (native, CPython-free build) ─────────────────────
// These run only in the `--no-default-features` build, where `import <mod>` is
// served by compiling the vendored `pylib/*.py` on pythonrs itself (no libpython).
// They prove a real CPython stdlib source file executes end-to-end on the Rust
// interpreter and produces native pythonrs objects.

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_future_runs_on_pythonrs() {
    // `__future__.py` is pure Python with no imports — the cleanest proof that a
    // vendored stdlib file is compiled and executed by pythonrs, not CPython.
    assert_eq!(
        g("import __future__\nx = __future__.division.optional", "x"),
        "(2, 2, 0, 'alpha', 2)"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_import_is_memoized() {
    // Second import of the same module returns the identical cached object
    // (pythonrs's `sys.modules`), so the vendored `.py` executes at most once.
    assert_eq!(
        g("import __future__ as a\nimport __future__ as b\nx = a is b", "x"),
        "True"
    );
}

#[test]
fn dunder_contains_is_callable_on_builtin_containers() {
    // `c.__contains__(x)` == `x in c` for every builtin container — the stdlib
    // `keyword.py` binds `frozenset(kwlist).__contains__` to build `iskeyword`.
    assert_eq!(g("x = frozenset([1, 2, 3]).__contains__(2)", "x"), "True");
    assert_eq!(g("x = {1, 2}.__contains__(5)", "x"), "False");
    assert_eq!(g("x = [1, 2].__contains__(2)", "x"), "True");
    assert_eq!(g("x = {'a': 1}.__contains__('a')", "x"), "True");
    assert_eq!(g("x = 'abc'.__contains__('b')", "x"), "True");
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_keyword_runs_on_pythonrs() {
    // `keyword.py` executes end-to-end on pythonrs (native build, no libpython):
    // its `iskeyword`/`issoftkeyword` are `frozenset(...).__contains__` bindings.
    assert_eq!(g("import keyword\nx = keyword.iskeyword('for')", "x"), "True");
    assert_eq!(
        g("import keyword\nx = keyword.iskeyword('banana')", "x"),
        "False"
    );
    assert_eq!(
        g("import keyword\nx = keyword.issoftkeyword('match')", "x"),
        "True"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn a_module_namespace_is_live_not_a_snapshot() {
    // `mod.attr = v` must be visible to the module's OWN functions: CPython has
    // one namespace per module and its functions resolve globals through it.
    // Holding a separate snapshot on the module object let the two drift, so
    // monkeypatching a module silently did nothing — `base64.binascii = None`
    // rebound the attribute while `b64encode` kept calling the real one.
    assert_eq!(
        g(
            "import base64\n\
             base64.binascii = None\n\
             try:\n\
             \x20   base64.b64encode(b'a')\n\
             \x20   x = 'stale'\n\
             except AttributeError:\n\
             \x20   x = 'live'",
            "x"
        ),
        "'live'"
    );
    // The same namespace reached through `__dict__` writes through as well.
    assert_eq!(
        g(
            "import string, sys\n\
             sys.modules['string'].__dict__['capwords'] = lambda s: 'PATCHED'\n\
             x = string.capwords('a b')",
            "x"
        ),
        "'PATCHED'"
    );
    // And `__dict__` is one stable object, not a fresh copy per access.
    assert_eq!(
        g("import string\nx = string.__dict__ is string.__dict__", "x"),
        "True"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn a_class_is_labelled_with_the_module_that_defined_it() {
    // `__module__` is the defining module, NOT whichever module is running when
    // a metaclass gets around to registering the class. Enum classes are built
    // inside `EnumType.__new__`, so reading the live scope labelled every enum
    // `enum` — and `global_enum` then published its members into `enum` instead
    // of into the module that declared them.
    assert_eq!(
        g("import calendar\nx = calendar.Month.__module__", "x"),
        "'calendar'"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_calendar_runs_on_pythonrs() {
    // calendar.py end-to-end: it needs `enum.global_enum`, which reaches through
    // `sys.modules[...].__dict__.update(...)` to inject `JANUARY`/`MONDAY` as
    // module globals — names calendar's own functions then read.
    // `repr()` runs inside the program: the enum's own `__repr__` (which
    // `global_enum` rebinds to name the module) only fires through the builtin.
    assert_eq!(
        g("import calendar\nx = repr(calendar.JANUARY)", "x"),
        "'calendar.JANUARY'"
    );
    assert_eq!(g("import calendar\nx = int(calendar.JANUARY)", "x"), "1");
    assert_eq!(g("import calendar\nx = int(calendar.SUNDAY)", "x"), "6");
    assert_eq!(
        g("import calendar\nx = repr(calendar.monthrange(2026, 2))", "x"),
        "'(calendar.SUNDAY, 28)'"
    );
    assert_eq!(
        g("import calendar\nx = calendar.isleap(2024)", "x"),
        "True"
    );
    assert_eq!(
        g("import calendar\nx = calendar.month_name[1]", "x"),
        "'January'"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn a_property_on_a_metaclass_fires_for_the_class() {
    // A `property` defined on the metaclass is a data descriptor for the class
    // OBJECT, so `Cls.prop` runs the getter with the class as `self`. Returning
    // the property object instead broke `EnumType.__members__`.
    assert_eq!(
        g(
            "class Meta(type):\n\
             \x20   @property\n\
             \x20   def label(cls):\n\
             \x20       return 'meta:' + cls.__name__\n\
             class C(metaclass=Meta):\n\
             \x20   pass\n\
             x = C.label",
            "x"
        ),
        "'meta:C'"
    );
    assert_eq!(
        g("import calendar\nx = list(calendar.Month.__members__)[0]", "x"),
        "'JANUARY'"
    );
}

#[test]
fn typing_union_subscript_matches_the_pep604_spelling() {
    // Since 3.14 `typing.Union` IS `types.UnionType`, so `Union[X, Y]` and
    // `X | Y` must build the identical object — same flatten, same dedupe, same
    // collapse of a one-member union to that member.
    assert_eq!(g("from _typing import Union\nx = Union[int, str]", "x"), "int | str");
    assert_eq!(g("from _typing import Union\nx = Union[int]", "x"), "<class 'int'>");
    assert_eq!(g("from _typing import Union\nx = Union[int, int]", "x"), "<class 'int'>");
    assert_eq!(
        g("from _typing import Union\nx = Union[Union[int, str], bytes]", "x"),
        "int | str | bytes"
    );
    assert_eq!(g("from _typing import Union\nx = Union[int, None]", "x"), "int | None");
    assert_eq!(g("from _typing import Union\nx = Union[int, str] == (int | str)", "x"), "True");
}

#[test]
fn regex_supports_lookaround_and_backreferences() {
    // Python's `re` has look-around and backreferences; a finite-automaton engine
    // structurally cannot. The compiler falls back to a backtracking engine for
    // exactly the patterns that need it — `_pydecimal` and `fractions` write their
    // number grammars with `(?=\d|\.\d)`, so `import decimal` depended on this.
    assert_eq!(
        g(r#"import re
x = re.findall(r'\d+(?= dollars)', '5 dollars 7 euros 9 dollars')"#, "x"),
        "['5', '9']"
    );
    assert_eq!(
        g(r#"import re
x = re.sub(r'(?<=a)b', 'X', 'ab cb ab')"#, "x"),
        "'aX cb aX'"
    );
    assert_eq!(
        g(r#"import re
x = re.search(r'(\w+) \1', 'hey hey there').group(0)"#, "x"),
        "'hey hey'"
    );
    // The fast engine must still handle everything it always did.
    assert_eq!(
        g(r#"import re
x = re.findall(r'\b\w+\b', 'one two three')"#, "x"),
        "['one', 'two', 'three']"
    );
}

#[test]
fn re_split_keeps_capture_groups() {
    // `re.split` is not the engine's `split`: a pattern with capture groups
    // interleaves each group's text between the pieces, and yields `None` for a
    // group that did not participate. `textwrap` splits on a pattern that is ALL
    // groups, so dropping them made every `fill`/`wrap` return blank.
    assert_eq!(
        g(r#"import re
x = re.split(r'(\s)', 'a b c')"#, "x"),
        "['a', ' ', 'b', ' ', 'c']"
    );
    assert_eq!(
        g(r#"import re
x = re.split(r'(a)|(b)', 'zazbz')"#, "x"),
        "['z', 'a', None, 'z', None, 'b', 'z']"
    );
    // `maxsplit` is the THIRD positional of `re.split` (where the others keep
    // `flags`) and is also accepted by keyword.
    assert_eq!(
        g("import re\nx = re.split(r',', 'a,b,c', maxsplit=1)", "x"),
        "['a', 'b,c']"
    );
    assert_eq!(
        g("import re\nx = re.sub(r'a', 'X', 'aaa', count=2)", "x"),
        "'XXa'"
    );
}

#[test]
fn large_integers_compare_exactly() {
    // Equality on integers must be exact at any size. Comparing through `f64` made
    // any two integers within one ULP equal — at 29 digits that is a gap of
    // billions, and `_pydecimal.sqrt` (which ends in `exact = n*n == c`) took the
    // "exact" branch on a wrong root and returned 1 for sqrt(2).
    assert_eq!(
        g("n = 14142135623730950488016887242\nc = 2 * 100**28\nx = (n*n == c)", "x"),
        "False"
    );
    assert_eq!(g("x = (10**30 + 1 == 10**30)", "x"), "False");
    // A bignum equals a float only when the float IS that integer exactly.
    assert_eq!(g("x = (10**30 == 1e30)", "x"), "False");
    assert_eq!(g("x = (2**53 == 2.0**53)", "x"), "True");
    assert_eq!(g("x = (10**30 == float(10**30))", "x"), "False");
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_decimal_and_fractions_run_on_pythonrs() {
    // The real `_pydecimal`/`fractions`, not a native subset: correctly-rounded
    // arbitrary-precision arithmetic all the way through.
    assert_eq!(
        g("import decimal\nx = str(decimal.Decimal('1.1') + decimal.Decimal('2.2'))", "x"),
        "'3.3'"
    );
    assert_eq!(
        g("import decimal\nx = str(decimal.Decimal(1) / decimal.Decimal(7))", "x"),
        "'0.1428571428571428571428571429'"
    );
    assert_eq!(
        g("import decimal\nx = str(decimal.Decimal('2').sqrt())", "x"),
        "'1.414213562373095048801688724'"
    );
    assert_eq!(
        g("from fractions import Fraction\nx = str(Fraction('3/7') + Fraction(1, 14))", "x"),
        "'1/2'"
    );
    assert_eq!(
        g("from fractions import Fraction\nx = str(sum(Fraction(1, n) for n in range(1, 10)))", "x"),
        "'7129/2520'"
    );
}

#[test]
fn math_sumprod_is_exact_for_ints_and_correctly_rounded_for_floats() {
    // Two int sequences dot-product EXACTLY (bignum); floats go through the same
    // compensated accumulator `fsum` uses, with `fma` recovering each product's
    // rounding error. `statistics.correlation` is built on it.
    assert_eq!(g("import math\nx = math.sumprod([1,2,3], [4,5,6])", "x"), "32");
    assert_eq!(
        g("import math\nx = math.sumprod([10**20, 1], [10**20, 1])", "x"),
        "10000000000000000000000000000000000000001"
    );
    // Catastrophic cancellation: the exact answer is 0, not 1e308-scale noise.
    assert_eq!(
        g("import math\nx = math.sumprod([1e308, 1], [1, -1e308])", "x"),
        "0.0"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_io_runs_on_pythonrs() {
    // `io.py` declares its ABCs over the concrete streams `_io` supplies, so this
    // exercises both halves: the native `StringIO`/`BytesIO` and the vendored
    // module that registers them with `TextIOBase`/`BufferedIOBase`.
    assert_eq!(
        g("import io\ns = io.StringIO()\ns.write('a')\ns.write('bc')\nx = s.getvalue()", "x"),
        "'abc'"
    );
    assert_eq!(
        g("import io\ns = io.StringIO('a\\nb\\n')\nx = [l for l in s]", "x"),
        "['a\\n', 'b\\n']"
    );
    assert_eq!(
        g("import io\nb = io.BytesIO(b'abcdef')\nb.read(2)\nb.seek(-2, 2)\nx = b.read()", "x"),
        "b'ef'"
    );
    // An overwrite in the middle of a BytesIO leaves the tail intact.
    assert_eq!(
        g("import io\nb = io.BytesIO(b'xyz')\nb.seek(1)\nb.write(b'Q')\nx = b.getvalue()", "x"),
        "b'xQz'"
    );
    // Text positions are CODE POINTS, not bytes: two 2-byte characters is 2.
    assert_eq!(
        g("import io\ns = io.StringIO()\ns.write('\\u00e9\\u00e9')\nx = s.tell()", "x"),
        "2"
    );
    assert_eq!(
        g("import io\nx = isinstance(io.StringIO(), io.TextIOBase)", "x"),
        "True"
    );
    assert_eq!(
        g("import io\nx = isinstance(io.BytesIO(), io.BufferedIOBase)", "x"),
        "True"
    );
    // A closed stream refuses every operation but `close`.
    assert_eq!(
        g(
            "import io\n\
             s = io.StringIO('x')\n\
             s.close()\n\
             try:\n\
             \x20   s.read()\n\
             \x20   x = 'no error'\n\
             except ValueError:\n\
             \x20   x = 'closed'",
            "x"
        ),
        "'closed'"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_pathlib_runs_on_pythonrs() {
    assert_eq!(
        g("import pathlib\np = pathlib.PurePosixPath('/a/b/f.tar.gz')\nx = (p.name, p.stem, p.suffix)", "x"),
        "('f.tar.gz', 'f.tar', '.gz')"
    );
    assert_eq!(
        g("import pathlib\nx = str(pathlib.PurePosixPath('a') / 'b' / 'c')", "x"),
        "'a/b/c'"
    );
    // `relative_to` chains over `_PathParents`, whose only iteration protocol is
    // `__len__`/`__getitem__` — it has no `__iter__` at all.
    assert_eq!(
        g("import pathlib\nx = str(pathlib.PurePosixPath('/a/b').relative_to('/a'))", "x"),
        "'b'"
    );
}

#[test]
fn a_getitem_only_sequence_iterates_in_lazy_iterators() {
    // The old-style sequence protocol (`__getitem__` from 0 until IndexError, no
    // `__iter__`) has to work as a SOURCE for the lazy iterators too, not just
    // for `list()`. `pathlib.relative_to` chains over exactly such an object.
    let seq = "class S:\n\
               \x20   def __getitem__(self, i):\n\
               \x20       if i >= 3: raise IndexError\n\
               \x20       return i * 10\n\
               s = S()\n";
    assert_eq!(
        g(&format!("{seq}from itertools import chain\nx = list(chain([1], s))"), "x"),
        "[1, 0, 10, 20]"
    );
    assert_eq!(g(&format!("{seq}x = list(zip(s, 'abc'))"), "x"), "[(0, 'a'), (10, 'b'), (20, 'c')]");
    assert_eq!(g(&format!("{seq}x = list(map(str, s))"), "x"), "['0', '10', '20']");
    assert_eq!(g(&format!("{seq}x = list(enumerate(s))"), "x"), "[(0, 0), (1, 10), (2, 20)]");
    assert_eq!(g(&format!("{seq}x = list(filter(None, s))"), "x"), "[10, 20]");
}

#[test]
fn python_character_class_syntax_survives_translation() {
    // Inside `[...]` Python treats almost everything as a literal, while the
    // regex crate reserves several constructs there. `glob` compiles `([*?[])`
    // — three literal metacharacters — and a bare `[` inside a class opened a
    // NESTED class instead, failing with "unclosed character class" and taking
    // `pathlib` down with it.
    assert_eq!(
        g(r"import re
x = re.findall('([*?[])', 'a[b*c')", "x"),
        "['[', '*']"
    );
    // `\b` is a backspace inside a class and a word boundary outside one.
    assert_eq!(g(r"import re
x = re.findall(r'[\b]', 'a\x08b')", "x"), "['\\x08']");
    assert_eq!(g(r"import re
x = re.findall(r'\bw\w*', 'a word')", "x"), "['word']");
    // `]` in first position is a literal member, not the close.
    assert_eq!(g(r"import re
x = re.findall(r'[]]', 'a]b')", "x"), "[']']");
    assert_eq!(g(r"import re
x = re.findall(r'[^]]+', 'ab]cd')", "x"), "['ab', 'cd']");
    // Ranges must keep working.
    assert_eq!(g(r"import re
x = re.findall(r'[a-z]+', 'abc123')", "x"), "['abc']");
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_tokenize_runs_on_pythonrs() {
    // `tokenize` is a source-fidelity tool: every character of the input has to
    // be attributable to some token. `traceback` imports it, so `logging`,
    // `unittest` and `hashlib` reach the interpreter through here.
    assert_eq!(
        g(
            "import tokenize, io, token\n\
             src = 'x = 1  # hi\\n'\n\
             x = [(token.tok_name[t.type], t.string) for t in tokenize.generate_tokens(io.StringIO(src).readline)]",
            "x"
        ),
        "[('NAME', 'x'), ('OP', '='), ('NUMBER', '1'), ('COMMENT', '# hi'), ('NEWLINE', '\\n'), ('ENDMARKER', '')]"
    );
    // Indentation is reported as INDENT/DEDENT tokens, and a DEDENT closes every
    // open level at end of input.
    assert_eq!(
        g(
            "import tokenize, io, token\n\
             src = 'if a:\\n    b\\n'\n\
             x = [token.tok_name[t.type] for t in tokenize.generate_tokens(io.StringIO(src).readline)]",
            "x"
        ),
        "['NAME', 'NAME', 'OP', 'NEWLINE', 'INDENT', 'NAME', 'NEWLINE', 'DEDENT', 'ENDMARKER']"
    );
    // PEP 701: an f-string comes APART into its literal and expression pieces,
    // rather than arriving as one STRING token.
    assert_eq!(
        g(
            "import tokenize, io, token\n\
             src = 'f\\\"a{b}c\\\"\\n'\n\
             x = [(token.tok_name[t.type], t.string) for t in tokenize.generate_tokens(io.StringIO(src).readline)][:6]",
            "x"
        ),
        "[('FSTRING_START', 'f\"'), ('FSTRING_MIDDLE', 'a'), ('OP', '{'), ('NAME', 'b'), ('OP', '}'), ('FSTRING_MIDDLE', 'c')]"
    );
    // A nested format spec is its own literal run, and the field's closing brace
    // flushes it even when empty.
    assert_eq!(
        g(
            "import tokenize, io, token\n\
             src = 'f\\\"{a:{w}}\\\"\\n'\n\
             x = [(token.tok_name[t.type], t.string) for t in tokenize.generate_tokens(io.StringIO(src).readline)][6:9]",
            "x"
        ),
        "[('OP', '}'), ('FSTRING_MIDDLE', ''), ('OP', '}')]"
    );
}

#[test]
fn a_slot_wrapper_is_hashable_by_what_it_names() {
    // CPython caches one descriptor object per type/slot, so `dict.__repr__` is
    // the same object every read; this runtime builds a fresh one each time.
    // Hashing by heap id would make `pprint`'s dispatch table — filled with
    // `_dispatch[dict.__repr__]` and read back via `type(obj).__repr__` — never
    // find its own entries.
    assert_eq!(
        g("d = {dict.__repr__: 'D', list.__repr__: 'L'}\nx = d[type({}).__repr__]", "x"),
        "'D'"
    );
    assert_eq!(g("x = repr(dict.__repr__)", "x"), "\"<slot wrapper '__repr__' of 'dict' objects>\"");
}

#[test]
fn a_class_named_after_a_builtin_type_does_not_replace_it() {
    // Classes live in one table keyed by bare name, so a user class named after a
    // builtin TYPE silently replaced it for the whole process. `enum.py` opens
    // with `class property(DynamicClassAttribute)`, and that one line broke every
    // namedtuple in the program: `collections` builds its field accessors with
    // `property(...)`, so they became enum's descriptor and `NT.field` raised
    // through ITS `__get__`.
    assert_eq!(
        g(
            "import enum, collections\n\
             NT = collections.namedtuple('NT', ['a'])\n\
             x = NT(7).a",
            "x"
        ),
        "7"
    );
    // The shadowing class is still reachable under its own module, and the enum
    // machinery that depends on it keeps working.
    assert_eq!(
        g(
            "import enum\n\
             class Color(enum.Enum):\n\
             \x20   RED = 1\n\
             x = (Color.RED.name, Color.RED.value)",
            "x"
        ),
        "('RED', 1)"
    );
    // A user class shadowing a builtin type leaves the builtin itself intact.
    assert_eq!(
        g("class dict: pass\nx = {'a': 1}.keys()", "x"), "dict_keys(['a'])"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_ast_node_types_run_on_pythonrs() {
    // `_ast` is pure data: ~130 classes in a shallow hierarchy, each with a
    // `_fields` tuple. `ast.py` supplies every traversal helper on top.
    assert_eq!(g("import ast\nx = ast.Name._fields", "x"), "('id', 'ctx')");
    assert_eq!(
        g("import ast\nn = ast.Name(id='x', ctx=ast.Load())\nx = (n.id, type(n.ctx).__name__)", "x"),
        "('x', 'Load')"
    );
    // The hierarchy has to be real: `isinstance` against the abstract bases is
    // how every AST consumer dispatches.
    assert_eq!(
        g("import ast\nn = ast.Name(id='x')\nx = (isinstance(n, ast.expr), isinstance(n, ast.AST))", "x"),
        "(True, True)"
    );
    assert_eq!(
        g("import ast\nx = repr(ast.BinOp(ast.Constant(1), ast.Add(), ast.Constant(2)))", "x"),
        "'BinOp(left=Constant(value=1), op=Add(), right=Constant(value=2))'"
    );
}

#[test]
fn a_class_annotation_below_top_level_still_records_annotations() {
    // `__annotations__` is seeded when a class body annotates a bare name at ANY
    // depth. Scanning only top-level statements made this shape — which is all
    // over the stdlib — a NameError at class-definition time.
    assert_eq!(
        g(
            "class C:\n\
             \x20   if True:\n\
             \x20       x: int\n\
             x = C.__annotations__",
            "x"
        ),
        "{'x': <class 'int'>}"
    );
    assert_eq!(
        g(
            "class C:\n\
             \x20   try:\n\
             \x20       y: str\n\
             \x20   except Exception:\n\
             \x20       pass\n\
             x = C.__annotations__",
            "x"
        ),
        "{'y': <class 'str'>}"
    );
    // A nested `def` opens its own scope; its annotations are not the class's.
    assert_eq!(
        g(
            "class C:\n\
             \x20   a: int\n\
             \x20   def m(self):\n\
             \x20       b: str = 'x'\n\
             \x20       return b\n\
             x = C.__annotations__",
            "x"
        ),
        "{'a': <class 'int'>}"
    );
}

#[test]
fn a_getset_descriptor_is_callable_through_the_descriptor_protocol() {
    // `annotationlib` binds `type.__dict__['__annotations__'].__get__` at import
    // time as its way to read a class's OWN annotations without tripping
    // `__getattr__`. Everything that imports it — `dataclasses`, `inspect`,
    // `traceback`, `logging`, `unittest` — depends on that line.
    assert_eq!(
        g(
            "g = type.__dict__['__annotations__'].__get__\n\
             class C:\n\
             \x20   v: int\n\
             x = g(C)",
            "x"
        ),
        "{'v': <class 'int'>}"
    );
    // A `property`/descriptor carries a writable `__doc__`; `dis.py` documents
    // every field of its `_Instruction` namedtuple that way.
    assert_eq!(
        g(
            "import collections\n\
             NT = collections.namedtuple('NT', ['f'])\n\
             NT.f.__doc__ = 'the field'\n\
             x = 'ok'",
            "x"
        ),
        "'ok'"
    );
}

#[test]
fn exec_binds_into_locals_when_given_both_namespaces() {
    // With separate `globals` and `locals`, the code's top-level namespace IS
    // `locals`: a `def` there lands in `locals`, not in `globals`. `dataclasses`
    // generates every `__init__`/`__repr__` with `exec(txt, self.globals, ns)`
    // and then reads `ns['__create_fn__']`.
    assert_eq!(
        g(
            "g = {}\n\
             ns = {}\n\
             exec('def f():\\n return 42', g, ns)\n\
             x = ('f' in ns, 'f' in g, ns['f']())",
            "x"
        ),
        "(True, False, 42)"
    );
    // A name merely READ from `globals` is not copied into `locals`.
    assert_eq!(
        g(
            "g = {'seen': 1}\n\
             ns = {}\n\
             exec('y = seen + 1', g, ns)\n\
             x = ('seen' in ns, ns['y'])",
            "x"
        ),
        "(False, 2)"
    );
    // A single namespace still receives the bindings.
    assert_eq!(g("d = {}\nexec('v = 5', d)\nx = d['v']", "x"), "5");
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_dataclasses_run_on_pythonrs() {
    // dataclasses generates its methods as SOURCE and execs them, so this covers
    // the whole path: `annotationlib` for the field types, `inspect` for the
    // signature, and `exec` with split namespaces for the generated code.
    assert_eq!(
        g(
            "from dataclasses import dataclass\n\
             @dataclass\n\
             class P:\n\
             \x20   a: int\n\
             \x20   b: int = 0\n\
             x = repr(P(1))",
            "x"
        ),
        "'P(a=1, b=0)'"
    );
    assert_eq!(
        g(
            "from dataclasses import dataclass\n\
             @dataclass\n\
             class P:\n\
             \x20   a: int\n\
             x = (P(1) == P(1), P(1) == P(2))",
            "x"
        ),
        "(True, False)"
    );
    assert_eq!(
        g(
            "import dataclasses\n\
             @dataclasses.dataclass\n\
             class P:\n\
             \x20   a: int\n\
             \x20   b: int\n\
             x = dataclasses.asdict(P(1, 2))",
            "x"
        ),
        "{'a': 1, 'b': 2}"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_inspect_and_traceback_run_on_pythonrs() {
    assert_eq!(
        g(
            "import inspect\n\
             def f(a, b=2, *args, **kw):\n\
             \x20   pass\n\
             x = str(inspect.signature(f))",
            "x"
        ),
        "'(a, b=2, *args, **kw)'"
    );
    assert_eq!(
        g("import inspect\nx = inspect.isfunction(inspect.signature)", "x"),
        "True"
    );
    assert_eq!(
        g(
            "import traceback\n\
             try:\n\
             \x20   raise ValueError('boom')\n\
             except ValueError as e:\n\
             \x20   x = traceback.format_exception_only(type(e), e)[0].strip()",
            "x"
        ),
        "'ValueError: boom'"
    );
}

#[test]
fn globals_is_the_live_module_namespace() {
    // `globals()` IS the running module's namespace, so writing through it binds
    // a module global. `inspect` builds its entire `CO_*` constant set that way
    // (`mod_dict = globals()`, then `mod_dict["CO_" + name] = flag`), and a
    // snapshot silently dropped every one of them.
    assert_eq!(g("g = globals()\ng['ZZ'] = 5\nx = ZZ", "x"), "5");
    assert_eq!(g("x = type(globals()).__name__", "x"), "'dict'");
    // `locals()` inside a function stays a snapshot, as CPython's is for an
    // optimized frame.
    assert_eq!(
        g("def f():\n\x20   a = 1\n\x20   return locals()\nx = f()", "x"),
        "{'a': 1}"
    );
}

#[test]
fn a_function_reports_its_keyword_only_defaults() {
    // `inspect.signature` reads `__kwdefaults__` for every function it describes.
    assert_eq!(
        g("def f(a, b=2, *args, c=3, d, **kw):\n\x20   pass\nx = f.__kwdefaults__", "x"),
        "{'c': 3}"
    );
    assert_eq!(g("def f(a, b=2):\n\x20   pass\nx = f.__defaults__", "x"), "(2,)");
    // No keyword-only defaults at all is `None`, not an empty dict.
    assert_eq!(g("def g(x):\n\x20   pass\nx = g.__kwdefaults__", "x"), "None");
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn a_generator_body_can_call_a_deep_chain() {
    // A generator runs on its OWN stack, and everything it calls runs there too,
    // so the size that matters is the whole Python call chain the body can reach
    // — not one frame. `traceback.format_exception_only` is a generator that
    // calls through `_format_final_exc_line` into `_colorize`, and it overflowed
    // corosensei's 1 MiB default outright.
    assert_eq!(
        g(
            "import traceback\n\
             x = traceback.format_exception_only(ValueError, ValueError('boom'))[0].strip()",
            "x"
        ),
        "'ValueError: boom'"
    );
    // Recursion inside a generator body has to be able to go deep too.
    assert_eq!(
        g(
            "def rec(n):\n\
             \x20   return 1 if n <= 0 else 1 + rec(n - 1)\n\
             def gen():\n\
             \x20   yield rec(200)\n\
             x = list(gen())",
            "x"
        ),
        "[201]"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_threading_runs_on_pythonrs() {
    // pythonrs runs user code on ONE thread, so a started thread runs its target
    // immediately and has finished by the time `start()` returns. Every thread
    // still needs its own identity while it runs, or a finished thread's
    // `_delete()` evicts the main thread from the registry.
    assert_eq!(
        g(
            "import threading\n\
             seen = []\n\
             t = threading.Thread(target=lambda: seen.append(threading.current_thread().name))\n\
             t.start()\n\
             t.join()\n\
             x = (seen[0], threading.current_thread().name, t.is_alive())",
            "x"
        ),
        "('Thread-1 (<lambda>)', 'MainThread', False)"
    );
    // A non-blocking acquire of a held lock FAILS, which is what
    // `threading.Condition._is_owned` probes with.
    assert_eq!(
        g(
            "import threading\n\
             lk = threading.Lock()\n\
             lk.acquire()\n\
             x = lk.acquire(False)",
            "x"
        ),
        "False"
    );
    assert_eq!(
        g("import threading\ne = threading.Event()\ne.set()\nx = e.is_set()", "x"),
        "True"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn hashlib_matches_the_reference_digests() {
    // The published test vectors for "abc". A hash is DEFINED by these, so a
    // wrong implementation is worse than a missing one.
    assert_eq!(
        g("import hashlib\nx = hashlib.sha256(b'abc').hexdigest()", "x"),
        "'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad'"
    );
    assert_eq!(
        g("import hashlib\nx = hashlib.md5(b'abc').hexdigest()", "x"),
        "'900150983cd24fb0d6963f7d28e17f72'"
    );
    assert_eq!(
        g("import hashlib\nx = hashlib.sha1(b'abc').hexdigest()", "x"),
        "'a9993e364706816aba3e25717850c26c9cd0d89d'"
    );
    assert_eq!(
        g("import hashlib\nx = hashlib.sha3_256(b'abc').hexdigest()", "x"),
        "'3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532'"
    );
    assert_eq!(
        g("import hashlib\nx = hashlib.shake_128(b'abc').hexdigest(8)", "x"),
        "'5881092dd818bf5c'"
    );
    // Feeding in pieces equals feeding at once, and reading a digest does not
    // finalize the object.
    assert_eq!(
        g(
            "import hashlib\n\
             h = hashlib.sha256()\n\
             h.update(b'a')\n\
             h.update(b'bc')\n\
             first = h.hexdigest()\n\
             x = (first == hashlib.sha256(b'abc').hexdigest(), first == h.hexdigest())",
            "x"
        ),
        "(True, True)"
    );
    // `copy()` forks the state rather than aliasing it.
    assert_eq!(
        g(
            "import hashlib\n\
             h = hashlib.sha256(b'abc')\n\
             c = h.copy()\n\
             c.update(b'd')\n\
             x = c.hexdigest() != h.hexdigest()",
            "x"
        ),
        "True"
    );
}

#[test]
fn base_exception_is_the_root_of_exceptions_only() {
    // `BaseException` is the root of the EXCEPTION hierarchy, not of everything.
    // Answering it unconditionally made `isinstance(True, BaseException)` true,
    // and `logging._log` reads exactly that to tell an exception object from the
    // flag `True` — so every `logger.exception(...)` took the wrong branch.
    assert_eq!(g("x = isinstance(True, BaseException)", "x"), "False");
    assert_eq!(g("x = isinstance('s', BaseException)", "x"), "False");
    assert_eq!(g("x = isinstance(1, Exception)", "x"), "False");
    assert_eq!(g("x = isinstance(ValueError('v'), BaseException)", "x"), "True");
    assert_eq!(
        g("class E(Exception): pass\nx = (isinstance(E(), BaseException), issubclass(E, BaseException))", "x"),
        "(True, True)"
    );
}

#[test]
fn a_type_stored_as_a_class_attribute_is_not_bound_as_a_method() {
    // Reading a class attribute off an instance binds FUNCTIONS, not types.
    // `unittest.TestCase.failureException = AssertionError` is a type, and
    // binding it made `issubclass(exc_type, self.failureException)` compare
    // against a bound method — so every assertion failure was filed as an ERROR.
    assert_eq!(
        g(
            "class C:\n\
             \x20   err = AssertionError\n\
             x = (C().err is AssertionError, issubclass(AssertionError, C().err))",
            "x"
        ),
        "(True, True)"
    );
}

#[test]
fn getattr_fallback_applies_to_method_calls() {
    // `__getattr__` supplies attributes the class does not define, and a METHOD
    // CALL has to consult it too. `unittest`'s `_WritelnDecorator` is nothing but
    // a `__getattr__` forwarding to a wrapped stream.
    assert_eq!(
        g(
            "import io\n\
             class D:\n\
             \x20   def __init__(self, s):\n\
             \x20       self.stream = s\n\
             \x20   def __getattr__(self, a):\n\
             \x20       if a == 'stream':\n\
             \x20           raise AttributeError(a)\n\
             \x20       return getattr(self.stream, a)\n\
             d = D(io.StringIO())\n\
             d.write('hi')\n\
             x = d.stream.getvalue()",
            "x"
        ),
        "'hi'"
    );
}

#[test]
fn an_exception_instance_carries_attributes() {
    // CPython exceptions take arbitrary attributes; `unittest` stamps its own
    // bookkeeping onto the exceptions it catches.
    assert_eq!(g("e = StopIteration()\ne.value = 5\nx = e.value", "x"), "5");
    assert_eq!(
        g("v = ValueError('x')\nv.custom = 'y'\nx = (v.custom, v.args)", "x"),
        "('y', ('x',))"
    );
    // `BaseException`'s own methods.
    assert_eq!(g("e = ValueError('x')\nx = e.with_traceback(None) is e", "x"), "True");
    assert_eq!(g("e = ValueError('x')\ne.add_note('n')\nx = e.__notes__", "x"), "['n']");
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_unittest_runs_on_pythonrs() {
    // An assertion failure must be filed as a FAILURE and an unexpected
    // exception as an ERROR — the distinction `unittest` exists to draw.
    assert_eq!(
        g(
            "import unittest, io\n\
             class T(unittest.TestCase):\n\
             \x20   def test_ok(self):\n\
             \x20       self.assertEqual(1 + 1, 2)\n\
             \x20   def test_fail(self):\n\
             \x20       self.assertIn(3, [1, 2])\n\
             \x20   def test_raises(self):\n\
             \x20       with self.assertRaises(ValueError):\n\
             \x20           raise ValueError('x')\n\
             r = unittest.TextTestRunner(verbosity=0, stream=io.StringIO())\n\
             res = r.run(unittest.TestLoader().loadTestsFromTestCase(T))\n\
             x = (res.testsRun, len(res.failures), len(res.errors), res.wasSuccessful())",
            "x"
        ),
        "(3, 1, 0, False)"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_logging_runs_on_pythonrs() {
    assert_eq!(
        g(
            "import logging, io\n\
             s = io.StringIO()\n\
             h = logging.StreamHandler(s)\n\
             h.setFormatter(logging.Formatter('%(levelname)s:%(name)s:%(message)s'))\n\
             log = logging.getLogger('demo')\n\
             log.addHandler(h)\n\
             log.setLevel(logging.DEBUG)\n\
             log.info('hello %s', 'world')\n\
             x = s.getvalue()",
            "x"
        ),
        "'INFO:demo:hello world\\n'"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn vendored_csv_runs_on_pythonrs() {
    // Writing: a field is quoted only when it would otherwise be ambiguous, an
    // embedded quote is doubled, and `None` writes as an empty field.
    assert_eq!(
        g(
            "import csv, io\n\
             s = io.StringIO()\n\
             w = csv.writer(s, lineterminator='\\n')\n\
             w.writerow(['a', 'b,c', 'd\"e', None])\n\
             x = s.getvalue()",
            "x"
        ),
        "'a,\"b,c\",\"d\"\"e\",\\n'"
    );
    // Reading: a doubled quote inside a quoted field is one literal quote.
    assert_eq!(
        g(
            "import csv, io\n\
             x = list(csv.reader(io.StringIO('\"he said \"\"hi\"\"\",,z\\r\\n')))",
            "x"
        ),
        "[['he said \"hi\"', '', 'z']]"
    );
    // A blank line is an EMPTY ROW, not a skipped one.
    assert_eq!(
        g("import csv, io\nx = list(csv.reader(io.StringIO('\\n')))", "x"),
        "[[]]"
    );
    assert_eq!(
        g(
            "import csv, io\n\
             x = list(csv.reader(io.StringIO('  a, b\\n'), skipinitialspace=True))",
            "x"
        ),
        "[['a', 'b']]"
    );
    // The dialect registry, and reading through a registered dialect by name.
    assert_eq!(
        g(
            "import csv, io\n\
             csv.register_dialect('pipe', delimiter='|')\n\
             rows = list(csv.reader(io.StringIO('a|b\\n'), 'pipe'))\n\
             csv.unregister_dialect('pipe')\n\
             x = rows",
            "x"
        ),
        "[['a', 'b']]"
    );
    // `DictReader`/`DictWriter` are pure `csv.py` over this module.
    assert_eq!(
        g(
            "import csv, io\n\
             x = [dict(r) for r in csv.DictReader(io.StringIO('x,y\\n1,2\\n3,4\\n'))]",
            "x"
        ),
        "[{'x': '1', 'y': '2'}, {'x': '3', 'y': '4'}]"
    );
    assert_eq!(
        g(
            "import csv, io\n\
             o = io.StringIO()\n\
             w = csv.DictWriter(o, fieldnames=['x', 'y'], lineterminator='\\n')\n\
             w.writeheader()\n\
             w.writerow({'x': 1, 'y': 2})\n\
             x = o.getvalue()",
            "x"
        ),
        "'x,y\\n1,2\\n'"
    );
}

#[test]
fn a_dict_view_takes_set_operations_against_any_iterable() {
    // A dict view's set operators accept any iterable, unlike a plain list's.
    // `csv.DictWriter` finds extra keys with `rowdict.keys() - self.fieldnames`,
    // where the right side is a LIST.
    assert_eq!(g("d = {'a': 1, 'b': 2}\nx = sorted(d.keys() - ['a'])", "x"), "['b']");
    assert_eq!(g("d = {'a': 1, 'b': 2}\nx = sorted(d.keys() & ['a'])", "x"), "['a']");
    assert_eq!(
        g("d = {'a': 1}\nx = sorted(d.keys() | {'z'})", "x"),
        "['a', 'z']"
    );
    // A list minus a list is still a TypeError.
    assert_eq!(
        g(
            "try:\n\
             \x20   [1] - [2]\n\
             \x20   x = 'no error'\n\
             except TypeError:\n\
             \x20   x = 'TypeError'",
            "x"
        ),
        "'TypeError'"
    );
}

#[test]
fn verbose_mode_keeps_whitespace_inside_a_character_class() {
    // Under `re.VERBOSE` Python ignores whitespace in the PATTERN but not inside
    // a character class; the regex crate's `x` flag ignores it in both. `json`'s
    // whitespace scanner is `[ \t\n\r]*` compiled with VERBOSE, so the space was
    // silently dropped from the class and `json.loads` could not skip the space
    // after a comma — every object with more than one key failed to parse.
    assert_eq!(
        g(r"import re
x = re.compile(r'[ \t\n\r]*', re.VERBOSE).match('a  b', 1).end()", "x"),
        "3"
    );
    assert_eq!(
        g(r"import re
x = re.findall(r'[ x]+', 'a x b', re.VERBOSE)", "x"),
        "[' x ']"
    );
    // Whitespace OUTSIDE a class is still ignored.
    assert_eq!(
        g(r"import re
x = re.findall(r'\d+  ', 'a12b', re.VERBOSE)", "x"),
        "['12']"
    );
}

#[cfg(not(feature = "stdlib-ffi"))]
#[test]
fn json_round_trips_objects_with_several_keys() {
    // The whole point of the VERBOSE fix above: a second key in an object needs
    // the whitespace scanner to skip the space after the comma.
    assert_eq!(
        g("import json\nx = json.loads('{\"a\": 1, \"bb\": 2}')", "x"),
        "{'a': 1, 'bb': 2}"
    );
    assert_eq!(
        g(
            "import json\n\
             o = {'users': [{'id': i, 'name': 'n%d' % i} for i in range(3)]}\n\
             x = json.loads(json.dumps(o)) == o",
            "x"
        ),
        "True"
    );
}
