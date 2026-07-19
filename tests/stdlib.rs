//! Headless stdlib tests: import a native stdlib module, bind a global from it,
//! and read that global's `repr` back from the host. No `python3` required, so
//! these run in CI. Expected values are what CPython produces for the same call.

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

#[test]
fn string_constants() {
    assert_eq!(
        g("import string\nx = string.ascii_lowercase", "x"),
        "'abcdefghijklmnopqrstuvwxyz'"
    );
    assert_eq!(g("import string\nx = string.digits", "x"), "'0123456789'");
}

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
