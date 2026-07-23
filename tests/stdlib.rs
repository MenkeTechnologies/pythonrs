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
    assert_eq!(
        g(
            "from collections import namedtuple\nPt = namedtuple('Point', ['x','y'])\nx = Pt(1, 2)",
            "x"
        ),
        "Point(x=1, y=2)"
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
