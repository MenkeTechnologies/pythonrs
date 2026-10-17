//! The object-heap collector (`host::gc`). Each program runs on the built
//! `python` binary under `PYTHONRS_GC_STRESS=1`, which collects at every
//! safepoint that allows it, so an object the collector failed to reach is
//! freed while still in use — and its next read panics instead of the program
//! printing what CPython 3.15 prints for it. The collector exists only in the
//! libpython-free build.
#![cfg(not(feature = "stdlib-ffi"))]

use std::process::Command;

/// Run `src` on the built binary, collecting at every opportunity unless
/// `stress` is false; its stdout, after asserting it exited cleanly.
fn run(src: &str, stress: bool) -> String {
    let tmp = tempfile::tempdir().expect("temp dir");
    let path = tmp.path().join("main.py");
    std::fs::write(&path, src).expect("write source");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_python"));
    cmd.arg(&path).env("PYTHONRS_CACHE", "0");
    if stress {
        cmd.env("PYTHONRS_GC_STRESS", "1");
    }
    let out = cmd.output().expect("spawn python");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "exit {:?}: {stderr}", out.status);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Objects kept alive through every kind of reference the collector traces —
/// a global chain, a dict key, a closure cell, a bound method, a default
/// argument, a function attribute, a class attribute, an exception's
/// `__cause__`, an `lru_cache` entry, nested containers — survive while
/// garbage is made and collected around them, in module code and inside a
/// function called from it. Run normally, the garbage is collected: 300,000
/// short-lived objects reuse ids, as CPython's do.
#[test]
fn reachable_objects_survive_collection_and_garbage_is_reused() {
    let src = r#"
import functools
class Node:
    def __init__(self, v, nxt=None):
        self.v = v
        self.nxt = nxt
    def get(self):
        return self.v
def make_closure(x):
    box = [x]
    def inner():
        return box[0]
    return inner
def default_arg(d=Node('dflt')):
    return d.v
class Holder:
    shared = Node('class-attr')
chain = None
for i in range(5):
    chain = Node(i, chain)
keyed = {Node('as-key'): 'value'}
clo = make_closure(Node('closed-over'))
bound = Node('bound').get
default_arg.extra = Node('func-attr')
try:
    raise ValueError('inner')
except ValueError as e:
    try:
        raise KeyError('outer') from e
    except KeyError as e2:
        exc = e2
@functools.lru_cache(maxsize=None)
def cached(n):
    return Node(n)
cached(Node('lru-arg-unused') and 7)
tup = (Node('in-tuple'), [Node('in-list')], {'k': Node('in-dict')})
def churn(n):
    t = 0
    for i in range(n):
        t += len([Node(i), (i, str(i)), {i: i}])
    return t
churn(3000)
for i in range(3000):
    garbage = [Node(i)] * 3
walk = []
n = chain
while n is not None:
    walk.append(n.v)
    n = n.nxt
print(walk, [k.v for k in keyed], clo().v, bound(), default_arg(), default_arg.extra.v,
      Holder.shared.v, type(exc.__cause__).__name__, str(exc.__cause__), cached(7).v,
      tup[0].v, tup[1][0].v, tup[2]['k'].v, churn(10))
"#;
    assert_eq!(
        run(src, true),
        "[4, 3, 2, 1, 0] ['as-key'] closed-over bound dflt func-attr class-attr \
         ValueError inner 7 in-tuple in-list in-dict 30\n"
    );
    let reuse = "class Node:\n    pass\nids = set()\nfor i in range(300000):\n    \
                 ids.add(id(Node()))\nprint(len(ids) < 300000)\n";
    assert_eq!(run(reuse, false), "True\n");
}

/// A generator suspended mid-body keeps what its frame holds across a yield,
/// and one not yet started keeps its frame, while the program churns garbage
/// around both.
#[test]
fn generators_keep_their_state_through_collections() {
    let src = r#"
class Node:
    def __init__(self, v):
        self.v = v
def gen():
    held = Node('held-across-yield')
    yield held.v
    yield held.v + '!'
unstarted = gen()
g = gen()
first = next(g)
for i in range(3000):
    junk = [Node(i), {i: str(i)}]
print(first, next(g), list(unstarted))
"#;
    assert_eq!(
        run(src, true),
        "held-across-yield held-across-yield! ['held-across-yield', 'held-across-yield!']\n"
    );
}

/// Collection reaches into `try` bodies and around suspended generators: the
/// locals of a function's `try` and `except` blocks, a generator suspended
/// inside a `try`, and one delegating with `yield from` keep their objects
/// while garbage is collected around them — inside a module-level `try`, after
/// importing modules that leave generators suspended (`typing`, `enum`),
/// where the 3,000 short-lived objects then reuse ids.
#[test]
fn try_bodies_and_suspended_generators_collect_safely() {
    let src = r#"
import typing, enum
class Node:
    def __init__(self, v):
        self.v = v
def churn(n):
    for i in range(n):
        junk = [Node(i), {i: str(i)}]
def in_try():
    try:
        keep = Node('try-local')
        churn(2000)
        raise ValueError(keep.v)
    except ValueError as e:
        held = Node('handler-local')
        churn(2000)
        return str(e) + '/' + held.v
def gen_in_try():
    try:
        x = Node('gen-try')
        yield x.v
        yield x.v + '!'
    finally:
        pass
def inner():
    a = Node('inner')
    yield a.v
    yield a.v + '?'
    return Node('ret').v
def outer():
    r = yield from inner()
    yield r
g1 = gen_in_try(); g2 = outer()
first = (next(g1), next(g2))
churn(3000)
second = (next(g1), next(g2), next(g2))
try:
    ids = set()
    for i in range(3000):
        ids.add(id(Node(i)))
    reused = len(ids) < 3000
except Exception:
    reused = None
print(in_try(), first, second, reused)
"#;
    assert_eq!(
        run(src, true),
        "try-local/handler-local ('gen-try', 'inner') ('gen-try!', 'inner?', 'ret') True\n"
    );
}
