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
