//! Headless tests for augmented-assignment in-place semantics, `with`-statement
//! `__exit__` exception handling + suppression, and chained-comparison single
//! evaluation. Each program binds a global whose `repr` is the value CPython
//! (3.14.6) produces for the same source; no `python3` is required, so these run
//! in CI. See the sibling `lang.rs` for the shared harness rationale.

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

// ── augmented assignment: in-place dunders ───────────────────────────────────

#[test]
fn augassign_instance_iadd_mutates_and_rebinds_self() {
    // A class whose __iadd__ mutates in place and returns self: the value is
    // rebound to that same object (identity preserved).
    let src = "\
class C:
    def __init__(s): s.v = 0
    def __iadd__(s, o):
        s.v += o
        return s
c = C()
d = c
c += 5
same = d is c
val = c.v";
    assert_eq!(g(src, "same"), "True");
    assert_eq!(g(src, "val"), "5");
}

#[test]
fn augassign_falls_back_to_binary_add_when_no_iadd() {
    // No __iadd__, only __add__: `+=` becomes `c = c + o`, a NEW object.
    let src = "\
class A:
    def __init__(s, x): s.x = x
    def __add__(s, o): return A(s.x + o)
a = A(1)
b = a
a += 10
same = b is a
val = a.x";
    assert_eq!(g(src, "same"), "False");
    assert_eq!(g(src, "val"), "11");
}

#[test]
fn augassign_list_iadd_extends_in_place() {
    // `list += iterable` uses the in-place extend: identity is preserved and any
    // iterable (here a generator) is accepted.
    assert_eq!(
        g("l = [1, 2]\nm = l\nl += [3, 4]\nsame = m is l", "same"),
        "True"
    );
    assert_eq!(g("l = [1, 2]\nl += [3, 4]", "l"), "[1, 2, 3, 4]");
    assert_eq!(
        g("l = [1]\nl += (x for x in range(3))", "l"),
        "[1, 0, 1, 2]"
    );
    assert_eq!(g("l = [1]\nl += 'ab'", "l"), "[1, 'a', 'b']");
}

#[test]
fn augassign_list_imul_repeats_in_place() {
    assert_eq!(
        g("l = [1, 2]\nm = l\nl *= 3\nsame = m is l", "same"),
        "True"
    );
    assert_eq!(g("l = [1, 2]\nl *= 3", "l"), "[1, 2, 1, 2, 1, 2]");
}

#[test]
fn augassign_immutable_types_rebind_new_value() {
    assert_eq!(g("x = 5\nx += 3", "x"), "8");
    assert_eq!(g("s = 'a'\ns += 'b'", "s"), "'ab'");
    assert_eq!(g("t = (1,)\nt += (2,)", "t"), "(1, 2)");
    // A tuple never mutates: the original binding is untouched.
    assert_eq!(
        g("t = (1,)\nu = t\nt += (2,)\nsame = u is t", "same"),
        "False"
    );
}

#[test]
fn augassign_set_ops_mutate_in_place() {
    assert_eq!(
        g("s = {1, 2}\nm = s\ns |= {3}\nsame = m is s", "same"),
        "True"
    );
    assert_eq!(g("s = {1, 2}\ns |= {3}\nx = sorted(s)", "x"), "[1, 2, 3]");
    assert_eq!(g("s = {1, 2, 3}\ns -= {2}\nx = sorted(s)", "x"), "[1, 3]");
    assert_eq!(
        g("s = {1, 2, 3}\ns &= {2, 3, 4}\nx = sorted(s)", "x"),
        "[2, 3]"
    );
    assert_eq!(g("s = {1, 2}\ns ^= {2, 3}\nx = sorted(s)", "x"), "[1, 3]");
}

#[test]
fn augassign_dict_ior_updates_in_place() {
    assert_eq!(
        g("d = {'a': 1}\nm = d\nd |= {'b': 2}\nsame = m is d", "same"),
        "True"
    );
    assert_eq!(g("d = {'a': 1}\nd |= {'b': 2}", "d"), "{'a': 1, 'b': 2}");
}

#[test]
fn augassign_bytearray_iadd_extends_in_place() {
    assert_eq!(
        g(
            "b = bytearray(b'ab')\nm = b\nb += b'cd'\nsame = m is b",
            "same"
        ),
        "True"
    );
    assert_eq!(
        g("b = bytearray(b'ab')\nb += b'cd'", "b"),
        "bytearray(b'abcd')"
    );
}

#[test]
fn augassign_subscript_evaluates_receiver_and_index_once() {
    // `d[k()] += 5` must evaluate `k` exactly once (CPython semantics).
    let src = "\
calls = [0]
def k():
    calls[0] += 1
    return 0
d = {0: 10}
d[k()] += 5
n = calls[0]
val = d[0]";
    assert_eq!(g(src, "n"), "1");
    assert_eq!(g(src, "val"), "15");
}

#[test]
fn augassign_attribute_target_uses_iadd() {
    // Attribute target with an in-place list on the attribute preserves identity.
    let src = "\
class Box:
    def __init__(s): s.data = [1]
box = Box()
ref = box.data
box.data += [2, 3]
same = ref is box.data
val = box.data";
    assert_eq!(g(src, "same"), "True");
    assert_eq!(g(src, "val"), "[1, 2, 3]");
}

// ── chained comparison: single evaluation of interior operand ────────────────

#[test]
fn chained_compare_evaluates_interior_operand_once() {
    let src = "\
n = 0
def f():
    global n
    n += 1
    return 5
r = 1 < f() < 10";
    assert_eq!(g(src, "r"), "True");
    assert_eq!(g(src, "n"), "1");
}

#[test]
fn chained_compare_multi_link_each_operand_once() {
    let src = "\
calls = [0, 0]
def gg(i):
    calls[i] += 1
    return i + 1
r = 0 < gg(0) < gg(1) < 5
c = calls";
    assert_eq!(g(src, "r"), "True");
    assert_eq!(g(src, "c"), "[1, 1]");
}

#[test]
fn chained_compare_short_circuits_without_later_operands() {
    // First link false: later operands are NOT evaluated.
    let src = "\
c = [0]
def h():
    c[0] += 1
    return 100
r = 10 < 1 < h()
n = c[0]";
    assert_eq!(g(src, "r"), "False");
    assert_eq!(g(src, "n"), "0");
}

#[test]
fn chained_compare_mixed_operators() {
    assert_eq!(g("r = 1 <= 1 < 2 == 2", "r"), "True");
    assert_eq!(g("r = 3 == 3 != 4", "r"), "True");
}

// ── with-statement: real exception triple + suppression ──────────────────────

#[test]
fn with_exit_truthy_return_suppresses_exception() {
    // __exit__ returning True swallows the raised exception; execution continues
    // after the `with`.
    let src = "\
class CM:
    def __enter__(s): return s
    def __exit__(s, t, v, tb): return True
reached = False
with CM():
    raise ValueError('boom')
reached = True";
    assert_eq!(g(src, "reached"), "True");
}

#[test]
fn with_exit_falsy_return_reraises_exception() {
    // __exit__ returning False re-raises; the outer try sees it.
    let src = "\
class CM:
    def __enter__(s): return s
    def __exit__(s, t, v, tb): return False
caught = None
try:
    with CM():
        raise ValueError('boom')
except ValueError as e:
    caught = str(e)";
    assert_eq!(g(src, "caught"), "'boom'");
}

#[test]
fn with_exit_sees_real_exception_type_and_value() {
    // __exit__ receives the real exception type and value (not (None, None)).
    let src = "\
seen_type = None
seen_val = None
class CM:
    def __enter__(s): return s
    def __exit__(s, t, v, tb):
        global seen_type, seen_val
        seen_type = t is ValueError
        seen_val = str(v)
        return True
with CM():
    raise ValueError('boom')";
    assert_eq!(g(src, "seen_type"), "True");
    assert_eq!(g(src, "seen_val"), "'boom'");
}

#[test]
fn with_normal_exit_passes_none_triple_once() {
    // No exception: __exit__ is called exactly once with (None, None, None).
    let src = "\
count = 0
sawnone = False
class CM:
    def __enter__(s): return s
    def __exit__(s, t, v, tb):
        global count, sawnone
        count += 1
        sawnone = t is None and v is None
        return None
with CM():
    pass";
    assert_eq!(g(src, "count"), "1");
    assert_eq!(g(src, "sawnone"), "True");
}

#[test]
fn with_nested_inner_suppress_hides_from_outer() {
    // `with A, B:` nests; the inner manager suppresses, so the outer never sees
    // the exception.
    let src = "\
log = []
class Q:
    def __init__(s, n, sup): s.n = n; s.sup = sup
    def __enter__(s): return s
    def __exit__(s, t, v, tb):
        log.append((s.n, t is None))
        return s.sup
with Q(1, False), Q(2, True):
    raise KeyError('k')
result = log";
    // Inner (2) sees the KeyError (t is None -> False) and suppresses; outer (1)
    // then exits normally (t is None -> True).
    assert_eq!(g(src, "result"), "[(2, False), (1, True)]");
}

#[test]
fn with_exit_receives_the_exception_traceback() {
    // `WITH_EXCEPT_START` passes `(type, value, value.__traceback__)`: the third
    // argument is a traceback whose innermost node is the raising frame, not
    // `None`.
    let src = "\
import traceback
seen = None
class CM:
    def __enter__(s): return s
    def __exit__(s, t, v, tb):
        global seen
        names = [f.name for f in traceback.extract_tb(tb)]
        seen = (type(tb).__name__, tb.tb_frame.f_code.co_name, tb.tb_lineno,
                names == [f.name for f in traceback.extract_tb(v.__traceback__)], names)
        return True
def boom():
    raise ValueError('v')
def f():
    with CM():
        boom()
f()
result = seen";
    assert_eq!(g(src, "result"), "('traceback', 'f', 15, True, ['f', 'boom'])");
}

#[test]
fn sys_exc_info_carries_the_traceback() {
    // `sys.exc_info()[2]` is the handled exception's `__traceback__`, and the
    // triple is all `None` again once the handler is left.
    let src = "\
import sys
def boom():
    raise KeyError('k')
try:
    boom()
except KeyError:
    t, v, tb = sys.exc_info()
    inside = (t.__name__, tb is not None, tb.tb_lineno, tb.tb_next.tb_frame.f_code.co_name)
after = sys.exc_info()";
    assert_eq!(g(src, "inside"), "('KeyError', True, 5, 'boom')");
    assert_eq!(g(src, "after"), "(None, None, None)");
}

#[cfg(feature = "stdlib-ffi")]
#[test]
fn exit_traceback_crosses_into_cpython_traceback_module() {
    // A CPython-raised exception's value routes `traceback.format_exception`
    // to CPython, so the native traceback object handed to `__exit__` has to
    // cross the bridge; it used to raise `cannot pass 'traceback'`.
    let src = "\
import json, traceback
last = None
class Log:
    def __enter__(s): return s
    def __exit__(s, t, v, tb):
        global last
        last = ''.join(traceback.format_exception(t, v, tb)).splitlines()[-1]
        return True
with Log():
    json.loads('{')
result = last";
    assert_eq!(
        g(src, "result"),
        "'json.decoder.JSONDecodeError: Expecting property name enclosed in double quotes: line 1 column 2 (char 1)'"
    );
}
