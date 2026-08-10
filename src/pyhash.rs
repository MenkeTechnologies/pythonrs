//! CPython's `hash()` algorithms, ported from the CPython 3.14.6 C sources.
//!
//! `hash()` is not an implementation detail CPython leaves open. The numeric
//! hash is *specified*: `hash(n)` is `n` reduced modulo `2**61 - 1`, and the
//! same modular scheme is extended to `float` and `complex` so that any two
//! numerically equal values hash equally regardless of type. That rule is what
//! makes `{1, 1.0, True, Decimal(1)}` a one-element set — a container property,
//! not a cosmetic number. An interpreter that invents its own hash silently
//! breaks the numeric tower across a type boundary.
//!
//! Ported faithfully from these upstream functions (the C is the spec):
//!
//! | this module | CPython 3.14.6 |
//! | --- | --- |
//! | [`int_i64`] / [`int_big`] | `Objects/longobject.c` `long_hash` |
//! | [`double`] | `Python/pyhash.c` `_Py_HashDouble` |
//! | [`complex`] | `Objects/complexobject.c` `complex_hash` |
//! | [`buffer`] | `Python/pyhash.c` `Py_HashBuffer` + `siphash13` |
//! | [`tuple`] | `Objects/tupleobject.c` `tuple_hash` |
//! | [`frozenset`] | `Objects/setobject.c` `frozenset_hash` |
//!
//! Every arithmetic step runs in wrapping `u64` to match CPython's
//! `Py_uhash_t`, and the `-1` result is remapped exactly where CPython remaps
//! it (`-1` is its `tp_hash` error sentinel, so no object may return it).
//!
//! **What is deliberately NOT here.** `hash(float('nan'))`, `hash(...)`,
//! `hash(NotImplemented)` and the default identity hash of an instance are
//! `PyObject_GenericHash` — derived from the object's ADDRESS. Measured across
//! runs they differ every time even under `PYTHONHASHSEED=0`, so no
//! implementation can reproduce them and none is attempted.
//!
//! **`str`/`bytes` are reproducible only at `PYTHONHASHSEED=0`.** They use
//! SipHash-1-3 keyed by `_Py_HashSecret`, which CPython randomizes per process
//! unless the seed is pinned. [`buffer`] implements the zero-key case, which is
//! what a pinned `PYTHONHASHSEED=0` produces.

/// `_PyHASH_BITS` — the numeric hash is taken modulo `2**BITS - 1`.
const BITS: u32 = 61;
/// `_PyHASH_MODULUS` — the Mersenne prime `2**61 - 1`.
const MODULUS: u64 = (1u64 << BITS) - 1;
/// `_PyHASH_INF`.
const INF: i64 = 314159;
/// `_PyHASH_IMAG` / `PyHASH_MULTIPLIER` — the imaginary part's weight.
const IMAG: u64 = 1000003;
/// `hash(None)`. A fixed constant since CPython 3.12 (`object.c`).
pub const NONE: i64 = 0xFCA8_6420;

/// CPython reserves `-1` as `tp_hash`'s error sentinel, so a real hash of `-1`
/// is reported as `-2`.
fn avoid_minus_one(x: u64) -> i64 {
    if x == u64::MAX {
        -2
    } else {
        x as i64
    }
}

/// `long_hash` for a value that fits in `i64`.
///
/// CPython has two branches — a "compact" int returns its value verbatim, a
/// larger one reduces modulo `MODULUS` — but they agree, because a compact int
/// is always far below the modulus. One uniform rule reproduces both: reduce
/// the MAGNITUDE modulo `MODULUS`, then reapply the sign (CPython hashes the
/// magnitude digits and multiplies by `sign` at the end).
pub fn int_i64(n: i64) -> i64 {
    // `unsigned_abs` so `i64::MIN` does not overflow on negation.
    let magnitude = n.unsigned_abs() % MODULUS;
    let signed = if n < 0 {
        (magnitude as i64).wrapping_neg() as u64
    } else {
        magnitude
    };
    avoid_minus_one(signed)
}

/// `long_hash` for an arbitrary-precision integer.
///
/// CPython walks the magnitude digits and applies the sign LAST, so the
/// reduction is of `|n|`, never of a negative remainder. Taking a signed
/// remainder and correcting it into `[0, MODULUS)` instead yields
/// `MODULUS - (|n| mod MODULUS)` for negatives — a value that is wrong for
/// every negative bignum except the exact multiples of the modulus.
pub fn int_big(b: &num_bigint::BigInt) -> i64 {
    use num_traits::ToPrimitive;
    let modulus = num_bigint::BigInt::from(MODULUS);
    // `magnitude()` is the absolute value, matching CPython's digit walk.
    let magnitude = num_bigint::BigInt::from(b.magnitude().clone()) % &modulus;
    let magnitude = magnitude.to_u64().unwrap_or(0);
    let signed = if b.sign() == num_bigint::Sign::Minus {
        (magnitude as i64).wrapping_neg() as u64
    } else {
        magnitude
    };
    avoid_minus_one(signed)
}

/// `_Py_HashDouble`.
///
/// Reduces the mantissa 28 bits at a time modulo `MODULUS`, then rotates by the
/// exponent — the rotation is valid because `2**61 ≡ 1 (mod 2**61 - 1)`. The
/// point of the scheme is that an integral float lands on the same number as
/// the equal `int`, which is what keeps `hash(2.0) == hash(2)`.
///
/// Returns `None` for NaN, whose CPython hash is address-derived and therefore
/// unreproducible; the caller decides what to do with it.
pub fn double(v: f64) -> Option<i64> {
    if v.is_nan() {
        return None;
    }
    if v.is_infinite() {
        return Some(if v > 0.0 { INF } else { -INF });
    }
    // `frexp`: v == m * 2**e with 0.5 <= |m| < 1.
    let (mut m, mut e) = frexp(v);
    let sign: i64 = if m < 0.0 {
        m = -m;
        -1
    } else {
        1
    };
    let mut x: u64 = 0;
    while m != 0.0 {
        x = ((x << 28) & MODULUS) | (x >> (BITS - 28));
        m *= 268435456.0; // 2**28
        e -= 28;
        let y = m as u64; // pull out the integer part
        m -= y as f64;
        x += y;
        if x >= MODULUS {
            x -= MODULUS;
        }
    }
    // Reduce the exponent modulo BITS, matching C's asymmetric expression for
    // negative exponents (C's `%` truncates toward zero).
    let e = if e >= 0 {
        (e % BITS as i32) as u32
    } else {
        BITS - 1 - ((-1 - e) % BITS as i32) as u32
    };
    x = ((x << e) & MODULUS) | (x >> (BITS - e));
    let x = (x as i64).wrapping_mul(sign) as u64;
    Some(avoid_minus_one(x))
}

/// Split `v` into a normalized mantissa and exponent (C's `frexp`).
///
/// Rust has no `frexp` in `std`, so this derives it from the IEEE-754 bits
/// rather than from a log, which would be inexact.
fn frexp(v: f64) -> (f64, i32) {
    if v == 0.0 || !v.is_finite() {
        return (v, 0);
    }
    let bits = v.to_bits();
    let raw_exp = ((bits >> 52) & 0x7ff) as i32;
    if raw_exp == 0 {
        // Subnormal: scale into the normal range, then correct the exponent.
        let (m, e) = frexp(v * 9007199254740992.0); // 2**53
        return (m, e - 53);
    }
    // Force the stored exponent to the value representing [0.5, 1).
    let mantissa = f64::from_bits((bits & !(0x7ffu64 << 52)) | (1022u64 << 52));
    (mantissa, raw_exp - 1022)
}

/// `complex_hash`: combine the two component hashes with the imaginary weight.
///
/// Real and imaginary parts are hashed as floats, so a complex with a zero
/// imaginary part hashes exactly like the equal real number.
pub fn complex(re: f64, im: f64) -> Option<i64> {
    let hre = double(re)?;
    let him = double(im)?;
    let combined = (hre as u64).wrapping_add((him as u64).wrapping_mul(IMAG));
    Some(avoid_minus_one(combined))
}

/// `Py_HashBuffer`: SipHash-1-3 over raw bytes, with CPython's zero-length
/// special case.
///
/// The empty buffer hashes to `0` (CPython does this deliberately, to avoid
/// leaking `prefix ^ suffix` of the hash secret). Everything else is SipHash-1-3
/// under the key from `_Py_HashSecret` — reproduced here for the all-zero key,
/// which is what `PYTHONHASHSEED=0` installs.
pub fn buffer(data: &[u8]) -> i64 {
    if data.is_empty() {
        return 0;
    }
    let x = siphash13(0, 0, data);
    if x == u64::MAX {
        return -2;
    }
    x as i64
}

/// SipHash-1-3 — one compression round per 8-byte block, three finalization
/// rounds — as CPython builds it in `Python/pyhash.c`.
///
/// This is NOT stock SipHash-1-3: CPython's finalizer is marked `/* modified */`
/// upstream and returns `(v0 ^ v1) ^ (v2 ^ v3)` directly. Reproducing the
/// standard construction instead would disagree on every non-empty string.
fn siphash13(k0: u64, k1: u64, src: &[u8]) -> u64 {
    /// `HALF_ROUND(a, b, c, d, s, t)` from the C macro.
    fn half_round(a: &mut u64, b: &mut u64, c: &mut u64, d: &mut u64, s: u32, t: u32) {
        *a = a.wrapping_add(*b);
        *c = c.wrapping_add(*d);
        *b = b.rotate_left(s) ^ *a;
        *d = d.rotate_left(t) ^ *c;
        *a = a.rotate_left(32);
    }
    /// `SINGLE_ROUND(v0, v1, v2, v3)` — note the operand REORDER in the second
    /// half round (`v2, v1, v0, v3`), which is easy to drop when transcribing.
    fn single_round(v0: &mut u64, v1: &mut u64, v2: &mut u64, v3: &mut u64) {
        half_round(v0, v1, v2, v3, 13, 16);
        half_round(v2, v1, v0, v3, 17, 21);
    }

    let mut b: u64 = (src.len() as u64) << 56;
    let mut v0 = k0 ^ 0x736f6d6570736575;
    let mut v1 = k1 ^ 0x646f72616e646f6d;
    let mut v2 = k0 ^ 0x6c7967656e657261;
    let mut v3 = k1 ^ 0x7465646279746573;

    let mut chunks = src.chunks_exact(8);
    for chunk in &mut chunks {
        let mi = u64::from_le_bytes(chunk.try_into().unwrap());
        v3 ^= mi;
        single_round(&mut v0, &mut v1, &mut v2, &mut v3);
        v0 ^= mi;
    }

    // The trailing 0..7 bytes are packed little-endian into the low bytes of
    // `t`, leaving the length in the top byte of `b`.
    let mut t = [0u8; 8];
    let rest = chunks.remainder();
    t[..rest.len()].copy_from_slice(rest);
    b |= u64::from_le_bytes(t);

    v3 ^= b;
    single_round(&mut v0, &mut v1, &mut v2, &mut v3);
    v0 ^= b;
    v2 ^= 0xff;
    single_round(&mut v0, &mut v1, &mut v2, &mut v3);
    single_round(&mut v0, &mut v1, &mut v2, &mut v3);
    single_round(&mut v0, &mut v1, &mut v2, &mut v3);

    (v0 ^ v1) ^ (v2 ^ v3)
}

/// `hash(str)`: SipHash over CPython's INTERNAL string buffer.
///
/// CPython does not hash UTF-8. A `str` is stored in the narrowest of three
/// fixed-width forms — latin-1, UCS-2 or UCS-4, chosen by the largest codepoint
/// — and `unicode_hash` hashes those raw code units. Hashing UTF-8 instead
/// would agree only for pure-ASCII strings, where the two encodings coincide,
/// and silently diverge for everything else.
pub fn string(s: &str) -> i64 {
    let max = s.chars().map(|c| c as u32).max().unwrap_or(0);
    let bytes: Vec<u8> = if max < 0x100 {
        s.chars().map(|c| c as u8).collect()
    } else if max < 0x10000 {
        s.chars().flat_map(|c| (c as u16).to_le_bytes()).collect()
    } else {
        s.chars().flat_map(|c| (c as u32).to_le_bytes()).collect()
    };
    buffer(&bytes)
}

/// `tuple_hash`: the xxHash-derived accumulator over the ELEMENT hashes.
///
/// Takes the elements' hashes rather than the elements so the caller stays in
/// charge of how each element is hashed (an element may be a user instance
/// whose `__hash__` has to run outside the host borrow).
pub fn tuple(element_hashes: &[i64]) -> i64 {
    const XXPRIME_1: u64 = 11400714785074694791;
    const XXPRIME_2: u64 = 14029467366897019727;
    const XXPRIME_5: u64 = 2870177450012600261;

    let mut acc: u64 = XXPRIME_5;
    for &lane in element_hashes {
        acc = acc.wrapping_add((lane as u64).wrapping_mul(XXPRIME_2));
        acc = acc.rotate_left(31);
        acc = acc.wrapping_mul(XXPRIME_1);
    }
    // The length is mangled to preserve the historical value of `hash(())`.
    acc = acc.wrapping_add((element_hashes.len() as u64) ^ (XXPRIME_5 ^ 3527539));
    if acc == u64::MAX {
        return 1546275796;
    }
    acc as i64
}

/// `frozenset_hash`: an order-independent XOR of shuffled element hashes.
///
/// CPython walks the whole hash TABLE, including empty slots, then cancels
/// their contribution afterwards — a vectorization trick whose net effect is
/// exactly an XOR over the live entries. Iterating the live elements directly
/// reproduces the result without modelling the table.
pub fn frozenset(element_hashes: &[i64]) -> i64 {
    fn shuffle_bits(h: u64) -> u64 {
        ((h ^ 89869747) ^ (h << 16)).wrapping_mul(3644798167)
    }
    let mut hash: u64 = 0;
    for &h in element_hashes {
        hash ^= shuffle_bits(h as u64);
    }
    hash ^= ((element_hashes.len() as u64).wrapping_add(1)).wrapping_mul(1927868237);
    // Disperse patterns arising in nested frozensets.
    hash ^= (hash >> 11) ^ (hash >> 25);
    hash = hash.wrapping_mul(69069).wrapping_add(907133923);
    if hash == u64::MAX {
        return 590923713;
    }
    hash as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values transcribed from CPython 3.14.6 run under `PYTHONHASHSEED=0`.
    /// These are the reference numbers, not this implementation's output — a
    /// test that asserted against our own result would pass with any bug.
    #[test]
    fn matches_cpython_integers() {
        // `hash(n) == n` below the modulus.
        assert_eq!(int_i64(0), 0);
        assert_eq!(int_i64(1), 1);
        assert_eq!(int_i64(7), 7);
        assert_eq!(int_i64(-7), -7);
        // `-1` is the error sentinel and reports as `-2`; `-2` also gives `-2`.
        assert_eq!(int_i64(-1), -2);
        assert_eq!(int_i64(-2), -2);
        // At and past the modulus the reduction becomes visible.
        assert_eq!(int_i64(2305843009213693951), 0); // 2**61-1
        assert_eq!(int_i64(2305843009213693952), 1); // 2**61
        assert_eq!(int_i64(4611686018427387904), 2); // 2**62
        assert_eq!(int_i64(-2305843009213693951), 0);
        assert_eq!(int_i64(-2305843009213693952), -2); // -1 -> -2
        assert_eq!(int_i64(-4611686018427387904), -2);
        // `i64::MIN` must not overflow while negating.
        assert_eq!(int_i64(i64::MIN), -4);
    }

    #[test]
    fn matches_cpython_bignums() {
        let p = |s: &str| -> num_bigint::BigInt { s.parse().unwrap() };
        assert_eq!(int_big(&p("18446744073709551616")), 8); // 2**64
                                                            // The sign bug this port replaces returned -(MODULUS - 8) here.
        assert_eq!(int_big(&p("-18446744073709551616")), -8);
        assert_eq!(int_big(&p("1180591620717411315769")), 12857); // 2**70+12345
        assert_eq!(int_big(&p("-1180591620717411315769")), -12857);
        assert_eq!(
            int_big(&p("1000000000000000000000000000000")),
            465258685558744706
        );
    }

    #[test]
    fn matches_cpython_floats() {
        assert_eq!(double(0.0), Some(0));
        assert_eq!(double(-0.0), Some(0));
        assert_eq!(double(1.0), Some(1));
        assert_eq!(double(-1.0), Some(-2)); // -1 -> -2
        assert_eq!(double(2.5), Some(1152921504606846978));
        assert_eq!(double(0.5), Some(1152921504606846976));
        assert_eq!(double(-0.5), Some(-1152921504606846976));
        assert_eq!(double(3.15), Some(345876451382053891));
        assert_eq!(double(0.1), Some(230584300921369408));
        assert_eq!(double(1e300), Some(1224995262755759164));
        assert_eq!(double(-1e300), Some(-1224995262755759164));
        assert_eq!(double(f64::INFINITY), Some(314159));
        assert_eq!(double(f64::NEG_INFINITY), Some(-314159));
        // NaN is address-derived in CPython, so it is explicitly unreproducible.
        assert_eq!(double(f64::NAN), None);
        // A subnormal exercises the frexp scaling path.
        assert_eq!(double(5e-324), Some(16777216));
    }

    /// The numeric tower: equal values hash equally ACROSS types. This is the
    /// property the container behaviour depends on.
    #[test]
    fn numeric_tower_agrees_across_types() {
        assert_eq!(double(1.0).unwrap(), int_i64(1));
        assert_eq!(double(2.0).unwrap(), int_i64(2));
        assert_eq!(double(-3.0).unwrap(), int_i64(-3));
        assert_eq!(double(0.0).unwrap(), int_i64(0));
        assert_eq!(complex(1.0, 0.0).unwrap(), int_i64(1));
        assert_eq!(
            double(9007199254740992.0).unwrap(),
            int_i64(9007199254740992)
        );
    }

    #[test]
    fn matches_cpython_complex() {
        assert_eq!(complex(1.0, 2.0), Some(2000007));
        assert_eq!(complex(0.0, 1.0), Some(1000003));
        assert_eq!(complex(3.0, -4.0), Some(-4000009));
        assert_eq!(complex(2.5, -0.5), Some(-2305843009213693950));
    }

    #[test]
    fn matches_cpython_str_and_bytes() {
        assert_eq!(string(""), 0);
        assert_eq!(buffer(b""), 0);
        assert_eq!(string("a"), 4644417185603328019);
        assert_eq!(buffer(b"a"), 4644417185603328019);
        assert_eq!(string("abc"), -4594863902769663758);
        assert_eq!(buffer(b"abc"), -4594863902769663758);
        assert_eq!(string("hello world"), -5642461784034726774);
        // Non-ASCII: CPython hashes latin-1 / UCS-2 code units, NOT UTF-8. If
        // this hashed UTF-8 these two would both be wrong while every ASCII
        // case above still passed.
        assert_eq!(string("\u{e9}"), 6047309291227476195);
        assert_eq!(string("\u{65e5}"), -2037161330753641417);
        assert_eq!(string("\u{1f600}"), -3536540696076613844);
    }

    #[test]
    fn matches_cpython_tuple() {
        assert_eq!(tuple(&[]), 5740354900026072187);
        assert_eq!(tuple(&[int_i64(1)]), -6644214454873602895);
        assert_eq!(tuple(&[int_i64(1), int_i64(2)]), -3550055125485641917);
        assert_eq!(
            tuple(&[int_i64(1), int_i64(2), int_i64(3)]),
            529344067295497451
        );
        assert_eq!(
            tuple(&[int_i64(0), int_i64(0), int_i64(0)]),
            3010437511937009226
        );
        assert_eq!(tuple(&[string("a")]), 7319529274390396360);
        // Nested: hash((1, (2, 3))).
        assert_eq!(
            tuple(&[int_i64(1), tuple(&[int_i64(2), int_i64(3)])]),
            7267574591690527098
        );
    }

    #[test]
    fn matches_cpython_frozenset() {
        assert_eq!(frozenset(&[]), 133146708735736);
        assert_eq!(frozenset(&[int_i64(1)]), -558064481276695278);
        assert_eq!(frozenset(&[int_i64(1), int_i64(2)]), -1826646154956904602);
        assert_eq!(
            frozenset(&[int_i64(1), int_i64(2), int_i64(3)]),
            -272375401224217160
        );
        assert_eq!(frozenset(&[string("a")]), -7857795138424989601);
    }

    /// A frozenset hash must not depend on element order — the property that
    /// makes two equal frozensets share a dict slot.
    #[test]
    fn frozenset_hash_is_order_independent() {
        let a = frozenset(&[int_i64(1), int_i64(2), int_i64(3)]);
        let b = frozenset(&[int_i64(3), int_i64(1), int_i64(2)]);
        let c = frozenset(&[int_i64(2), int_i64(3), int_i64(1)]);
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    /// No hash may be `-1`: CPython reserves it as `tp_hash`'s error sentinel.
    #[test]
    fn never_returns_minus_one() {
        assert_ne!(int_i64(-1), -1);
        assert_ne!(double(-1.0).unwrap(), -1);
        assert_ne!(int_big(&"-1".parse::<num_bigint::BigInt>().unwrap()), -1);
        for n in -600i64..600 {
            assert_ne!(int_i64(n), -1, "int_i64({n})");
        }
    }
}
