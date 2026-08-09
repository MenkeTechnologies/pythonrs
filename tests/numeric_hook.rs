//! The fusevm numeric hook on a mixed `int`/`float` pair.
//!
//! Python compares `int` against `float` EXACTLY at any magnitude — unlike the
//! JVM languages, which promote the integer to a double and are therefore right
//! by accident. Past 2^53 an `i64` no longer has an `f64`, so reading both
//! operands as doubles answers about a NEIGHBOURING number:
//!
//! ```text
//! L = 3**34 = 16_677_181_699_666_569      its f64 = …568
//! CPython:  L == float(L) -> False        L > float(L) -> True
//! ```
//!
//! fusevm hands such a pair to this host precisely because only the host can
//! keep the integer exact. These tests call the hook DIRECTLY, so they hold
//! whether or not the linked fusevm delegates the pair — a fusevm that answers
//! it natively simply never asks, and the answer the host gives when asked must
//! still be Python's.
//!
//! Every expected value here is the value CPython prints for the same
//! expression. Note that the answers are deliberately NOT the rounded ones: an
//! exact comparison is a *different* answer from the `f64` one, and that
//! difference is the whole point of the delegation.

use fusevm::{NumOp, Value};
use pythonrs::builtins::numeric_hook;
use pythonrs::host::{reset_host, with_host};

/// `3**34`: the smallest power of three past 2^53, so it fits an `i64` and still
/// has no `f64`. `f64(L)` is its neighbour, one less.
const L: i64 = 16_677_181_699_666_569;

/// `repr` of a hook result, so a wrong answer names itself in the failure.
fn r(v: Result<Value, String>) -> String {
    match v {
        Ok(x) => with_host(|h| h.repr_of(&x)),
        Err(e) => format!("ERR {e}"),
    }
}

/// Run `src`, then the `repr` of global `name` (mirrors `tests/lang.rs`).
fn g(src: &str, name: &str) -> String {
    pythonrs::eval_str(src).expect("program should run without error");
    with_host(|h| {
        let v = h
            .read_global(name)
            .unwrap_or_else(|| panic!("global {name} unbound"));
        h.repr_of(&v)
    })
}

/// The premise: `L` and its `f64` really are different numbers. Without this the
/// rest of the file would pass on a machine where the two happened to coincide.
#[test]
fn the_operands_are_genuinely_different_numbers() {
    assert_ne!(
        L as i128, L as f64 as i128,
        "L must not survive the trip through f64, or these tests prove nothing"
    );
}

/// All six comparisons, in BOTH operand orders, against a float that is not the
/// integer. Eight of these twelve answer the opposite way when the pair is
/// resolved as two `f64`s, so this is what the exact path buys.
#[test]
fn mixed_int_float_comparisons_are_exact_in_both_orders() {
    reset_host();
    let i = Value::Int(L);
    let f = Value::Float(L as f64); // …568, one BELOW the integer

    // (op, `L op f`, `f op L`) — CPython 3.14 answers.
    let cases = [
        (NumOp::Lt, "False", "True"),
        (NumOp::Gt, "True", "False"),
        (NumOp::Le, "False", "True"),
        (NumOp::Ge, "True", "False"),
        (NumOp::Eq, "False", "False"),
        (NumOp::Ne, "True", "True"),
    ];
    for (op, fwd, rev) in cases {
        assert_eq!(r(numeric_hook(op, &i, &f)), fwd, "int {op:?} float");
        assert_eq!(r(numeric_hook(op, &f, &i)), rev, "float {op:?} int");
    }
}

/// The same exactness when the float is one ABOVE the integer, so a fix that
/// merely hardcoded "the integer is the larger" cannot pass.
#[test]
fn exactness_holds_when_the_float_is_the_larger() {
    reset_host();
    // `L - 1` is even, so its f64 is exact; step one ULP up to clear it.
    let below = L - 1;
    let f = Value::Float((below as f64).next_up());
    let i = Value::Int(below);
    assert_eq!(r(numeric_hook(NumOp::Lt, &i, &f)), "True");
    assert_eq!(r(numeric_hook(NumOp::Gt, &i, &f)), "False");
    assert_eq!(r(numeric_hook(NumOp::Eq, &i, &f)), "False");
    assert_eq!(r(numeric_hook(NumOp::Gt, &f, &i)), "True");
    assert_eq!(r(numeric_hook(NumOp::Lt, &f, &i)), "False");
}

/// A float that IS exactly the integer still compares equal — the exact path
/// must not turn every mixed pair unequal.
#[test]
fn an_exact_float_still_equals_its_integer() {
    reset_host();
    let even = L - 1; // even, so it has an exact f64
    let i = Value::Int(even);
    let f = Value::Float(even as f64);
    assert_eq!(
        even as f64 as i128, even as i128,
        "premise: f64 is exact here"
    );
    assert_eq!(r(numeric_hook(NumOp::Eq, &i, &f)), "True");
    assert_eq!(r(numeric_hook(NumOp::Ne, &i, &f)), "False");
    assert_eq!(r(numeric_hook(NumOp::Le, &i, &f)), "True");
    assert_eq!(r(numeric_hook(NumOp::Ge, &f, &i)), "True");
    assert_eq!(r(numeric_hook(NumOp::Lt, &i, &f)), "False");
}

/// Infinities order by sign and NaN is unordered — the exact path handles the
/// non-finite floats it now intercepts, rather than falling into the integer
/// conversion (which has no answer for them).
#[test]
fn non_finite_floats_keep_python_semantics() {
    reset_host();
    let i = Value::Int(L);
    assert_eq!(
        r(numeric_hook(NumOp::Lt, &i, &Value::Float(f64::INFINITY))),
        "True"
    );
    assert_eq!(
        r(numeric_hook(NumOp::Gt, &i, &Value::Float(f64::INFINITY))),
        "False"
    );
    assert_eq!(
        r(numeric_hook(
            NumOp::Gt,
            &i,
            &Value::Float(f64::NEG_INFINITY)
        )),
        "True"
    );
    assert_eq!(
        r(numeric_hook(NumOp::Eq, &i, &Value::Float(f64::NAN))),
        "False"
    );
    assert_eq!(
        r(numeric_hook(NumOp::Ne, &i, &Value::Float(f64::NAN))),
        "True"
    );
    assert_eq!(
        r(numeric_hook(NumOp::Lt, &i, &Value::Float(f64::NAN))),
        "False"
    );
    assert_eq!(
        r(numeric_hook(NumOp::Gt, &i, &Value::Float(f64::NAN))),
        "False"
    );
}

/// Small operands must be answered EXACTLY as before: `f64` already holds every
/// integer below 2^53, so the fast path stays and no answer moves.
#[test]
fn small_mixed_pairs_are_unchanged() {
    reset_host();
    let cases = [
        (NumOp::Eq, Value::Int(1), Value::Float(1.0), "True"),
        (NumOp::Eq, Value::Bool(true), Value::Float(1.0), "True"),
        (NumOp::Lt, Value::Int(2), Value::Float(2.5), "True"),
        (NumOp::Ge, Value::Int(3), Value::Float(3.0), "True"),
        (NumOp::Eq, Value::Int(0), Value::Float(-0.0), "True"),
        // 2^53 itself is the last integer f64 holds exactly.
        (
            NumOp::Eq,
            Value::Int(1 << 53),
            Value::Float((1u64 << 53) as f64),
            "True",
        ),
    ];
    for (op, a, b, want) in cases {
        assert_eq!(r(numeric_hook(op, &a, &b)), want, "{a:?} {op:?} {b:?}");
    }
}

/// Arithmetic is the OPPOSITE of comparison: `int + float` in Python really does
/// convert the integer to a double, so the rounded result is the correct one and
/// the exact-comparison work must not leak into it.
#[test]
fn mixed_arithmetic_still_converts_to_float() {
    reset_host();
    let i = Value::Int(L);
    let one = Value::Float(1.0);
    // (op, `L op 1.0`, `1.0 op L`) — CPython 3.14 reprs.
    let cases = [
        (
            NumOp::Add,
            "1.6677181699666568e+16",
            "1.6677181699666568e+16",
        ),
        (
            NumOp::Sub,
            "1.6677181699666568e+16",
            "-1.6677181699666568e+16",
        ),
        (
            NumOp::Mul,
            "1.6677181699666568e+16",
            "1.6677181699666568e+16",
        ),
        (NumOp::Div, "1.6677181699666568e+16", "5.9962169748381e-17"),
        (NumOp::Mod, "0.0", "1.0"),
        (NumOp::Pow, "1.6677181699666568e+16", "1.0"),
    ];
    for (op, fwd, rev) in cases {
        assert_eq!(r(numeric_hook(op, &i, &one)), fwd, "int {op:?} float");
        assert_eq!(r(numeric_hook(op, &one, &i)), rev, "float {op:?} int");
    }
}

/// A bignum against a float ordered by rounding BOTH to `f64`, which called two
/// different numbers Equal: `3**40 > float(3**40)` answered False. Equality was
/// already exact here, so this is the site where the invariant was missing
/// rather than merely too narrow.
#[test]
fn bignum_against_float_orders_exactly() {
    assert_eq!(g("B = 3**40\nx = B > float(B)", "x"), "True");
    assert_eq!(g("B = 3**40\nx = B < float(B)", "x"), "False");
    assert_eq!(g("B = 3**40\nx = float(B) < B", "x"), "True");
    assert_eq!(g("B = 3**40\nx = float(B) > B", "x"), "False");
    assert_eq!(g("B = 3**40\nx = B == float(B)", "x"), "False");
    // A bignum that IS the float still compares equal, both orders.
    assert_eq!(g("B = 2**80\nx = B == float(B)", "x"), "True");
    assert_eq!(g("B = 2**80\nx = B < float(B)", "x"), "False");
    assert_eq!(g("B = 2**80\nx = float(B) > B", "x"), "False");
}

/// Containment reaches the host's equality directly (not fusevm's native
/// compare), so a past-2^53 integer must not be found in a list holding only its
/// rounded float — nor the reverse.
#[test]
fn containment_does_not_round_the_element() {
    assert_eq!(g("L = 3**34\nx = float(L) in [L]", "x"), "False");
    assert_eq!(g("L = 3**34\nx = L in [float(L)]", "x"), "False");
    assert_eq!(g("L = 3**34\nx = L in [L]", "x"), "True");
    assert_eq!(g("L = 3**34 - 1\nx = float(L) in [L]", "x"), "True");
}

/// `min`/`max` route through the hook's comparison, so they must pick by the
/// exact order. Asserted on the VALUE, not on a `==` against a candidate: `==`
/// is the very thing under test, so a comparison both sides satisfy would pass
/// even with the rounding bug intact.
#[test]
fn min_and_max_pick_by_the_exact_order() {
    assert_eq!(
        g("L = 3**34\nx = repr(max(float(L), L))", "x"),
        "'16677181699666569'"
    );
    assert_eq!(
        g("L = 3**34\nx = repr(min(float(L), L))", "x"),
        "'1.6677181699666568e+16'"
    );
}

/// A past-2^53 integer and its rounded float are DISTINCT dict/set keys, since
/// they are not equal. This site normalizes a float key to its exact integer key
/// and was already correct — locked in so it cannot regress into agreement with
/// a rounding comparison.
#[test]
fn a_rounded_float_is_a_different_key() {
    assert_eq!(g("L = 3**34\nx = len({L: 1, float(L): 2})", "x"), "2");
    assert_eq!(g("L = 3**34\nx = len({L, float(L)})", "x"), "2");
    // Below 2^53 the two ARE equal and must still share one key.
    assert_eq!(g("x = len({1: 'a', 1.0: 'b'})", "x"), "1");
    assert_eq!(g("x = len({1, 1.0, True})", "x"), "1");
}

/// Two plain `Value::Int`s reach the hook, so its documentation may not claim
/// otherwise: `x += y` lowers to `INPLACE` (outside a native for-range slot
/// loop) and `inplace_binary_fallback` routes `+`/`-`/`*` back through the hook.
/// Bignum promotion on overflow is the observable consequence.
#[test]
fn two_native_ints_reach_the_hook_through_augmented_assignment() {
    assert_eq!(
        r(numeric_hook(NumOp::Add, &Value::Int(1), &Value::Int(2))),
        "3"
    );
    // Overflow past i64 promotes rather than wrapping — only the hook does that.
    assert_eq!(
        g("x = 9223372036854775807\nx += 1", "x"),
        "9223372036854775808"
    );
}
