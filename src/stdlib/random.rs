//! The `random` standard-library module.
//!
//! The generator is a self-contained SplitMix64 PRNG kept in a thread-local, auto
//! seeded from the wall clock on first use. It is deterministic after `seed(n)`,
//! but the sequence is pythonrs's own PRNG — it is NOT bit-compatible with
//! CPython's Mersenne Twister. Only the API surface and value distributions match
//! CPython (`random`, `seed`, `randint`, `randrange`, `uniform`, `choice`,
//! `shuffle`, `sample`).
//!
//! Wiring (done by the parent): an `import_module` arm for `"random"` calling
//! [`entries`], and a `call_builtin_function` arm routing `random.*` to [`call`].

use crate::host::{type_error, PyHost, PyObj};
use fusevm::Value;
use std::cell::RefCell;

// ── the PRNG ─────────────────────────────────────────────────────────────────

/// SplitMix64 — a fast, well-distributed 64-bit generator. Small enough to keep
/// inline with no external crate.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn from_time() -> SplitMix64 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x853c49e6748fea9b);
        SplitMix64 {
            state: nanos ^ 0x9E3779B97F4A7C15,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// A float in `[0, 1)` with 53 bits of precision, like CPython's `random()`.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A uniform integer in `[0, n)` (n > 0).
    fn below(&mut self, n: u64) -> u64 {
        // Rejection sampling to avoid modulo bias.
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let r = self.next_u64();
            if r < zone {
                return r % n;
            }
        }
    }
}

thread_local! {
    static RNG: RefCell<SplitMix64> = RefCell::new(SplitMix64::from_time());
}

fn rng_f64() -> f64 {
    RNG.with(|r| r.borrow_mut().next_f64())
}

fn rng_below(n: u64) -> u64 {
    RNG.with(|r| r.borrow_mut().below(n))
}

fn rng_seed(seed: u64) {
    RNG.with(|r| {
        // Mix the seed once so small seeds still spread across the state.
        r.borrow_mut().state = seed ^ 0x9E3779B97F4A7C15;
    });
}

// ── module surface ───────────────────────────────────────────────────────────

pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for name in [
        "seed",
        "random",
        "randint",
        "randrange",
        "uniform",
        "choice",
        "shuffle",
        "sample",
    ] {
        out.push((
            name.to_string(),
            h.alloc(PyObj::Builtin(format!("random.{name}"))),
        ));
    }
    out
}

pub fn call(h: &mut PyHost, fname: &str, args: &[Value]) -> Option<Result<Value, String>> {
    // `fname` arrives already stripped of the `random.` prefix by the caller.
    let rest = fname;
    let r = match rest {
        "seed" => r_seed(h, args),
        "random" => Ok(Value::Float(rng_f64())),
        "randint" => r_randint(h, args),
        "randrange" => r_randrange(h, args),
        "uniform" => r_uniform(h, args),
        "choice" => r_choice(h, args),
        "shuffle" => r_shuffle(h, args),
        "sample" => r_sample(h, args),
        _ => return None,
    };
    Some(r)
}

// ── functions ────────────────────────────────────────────────────────────────

fn r_seed(h: &PyHost, args: &[Value]) -> Result<Value, String> {
    match args.first() {
        None | Some(Value::Undef) => {
            RNG.with(|r| *r.borrow_mut() = SplitMix64::from_time());
        }
        Some(v) => {
            let n = h
                .as_int(v)
                .ok_or_else(|| type_error("seed() argument must be an int or None"))?;
            rng_seed(n as u64);
        }
    }
    Ok(Value::Undef)
}

fn r_randint(h: &PyHost, args: &[Value]) -> Result<Value, String> {
    let a = arg_int(h, args, 0)?;
    let b = arg_int(h, args, 1)?;
    if b < a {
        return Err("ValueError: empty range for randrange()".into());
    }
    let span = (b - a) as u64 + 1;
    Ok(Value::Int(a + rng_below(span) as i64))
}

/// randrange(stop) | randrange(start, stop[, step]).
fn r_randrange(h: &PyHost, args: &[Value]) -> Result<Value, String> {
    let (start, stop, step) = match args.len() {
        1 => (0, arg_int(h, args, 0)?, 1),
        2 => (arg_int(h, args, 0)?, arg_int(h, args, 1)?, 1),
        _ => (
            arg_int(h, args, 0)?,
            arg_int(h, args, 1)?,
            arg_int(h, args, 2)?,
        ),
    };
    if step == 0 {
        return Err("ValueError: zero step for randrange()".into());
    }
    let count = if step > 0 {
        if stop > start {
            (stop - start + step - 1) / step
        } else {
            0
        }
    } else if start > stop {
        (start - stop + (-step) - 1) / (-step)
    } else {
        0
    };
    if count <= 0 {
        return Err("ValueError: empty range for randrange()".into());
    }
    let i = rng_below(count as u64) as i64;
    Ok(Value::Int(start + i * step))
}

fn r_uniform(h: &PyHost, args: &[Value]) -> Result<Value, String> {
    let a = arg_float(h, args, 0)?;
    let b = arg_float(h, args, 1)?;
    Ok(Value::Float(a + (b - a) * rng_f64()))
}

fn r_choice(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let items = seq_items(h, args.first())?;
    if items.is_empty() {
        return Err("IndexError: Cannot choose from an empty sequence".into());
    }
    let idx = rng_below(items.len() as u64) as usize;
    Ok(items[idx].clone())
}

/// Fisher-Yates in place on a `list`; returns None.
fn r_shuffle(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let target = match args.first() {
        Some(v) => v.clone(),
        None => return Err(type_error("shuffle() missing required argument: 'x'")),
    };
    // Snapshot, shuffle, write back into the same heap object to keep identity.
    let mut items = match h.get(&target) {
        Some(PyObj::List(l)) => l.clone(),
        _ => {
            return Err(type_error(&format!(
                "'{}' object does not support item assignment",
                h.type_name(&target)
            )))
        }
    };
    let n = items.len();
    for i in (1..n).rev() {
        let j = rng_below((i + 1) as u64) as usize;
        items.swap(i, j);
    }
    if let Some(PyObj::List(l)) = h.get_mut(&target) {
        *l = items;
    }
    Ok(Value::Undef)
}

/// sample(seq, k): k distinct elements as a new list.
fn r_sample(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let items = seq_items(h, args.first())?;
    let k = arg_int(h, args, 1)?;
    if k < 0 {
        return Err("ValueError: Sample larger than population or is negative".into());
    }
    let k = k as usize;
    if k > items.len() {
        return Err("ValueError: Sample larger than population or is negative".into());
    }
    // Partial Fisher-Yates over an index pool.
    let mut pool: Vec<usize> = (0..items.len()).collect();
    let mut out = Vec::with_capacity(k);
    for i in 0..k {
        let j = i + rng_below((items.len() - i) as u64) as usize;
        pool.swap(i, j);
        out.push(items[pool[i]].clone());
    }
    Ok(h.new_list(out))
}

// ── arg helpers ──────────────────────────────────────────────────────────────

fn arg_int(h: &PyHost, args: &[Value], i: usize) -> Result<i64, String> {
    args.get(i)
        .and_then(|v| h.as_int(v))
        .ok_or_else(|| type_error("expected an int argument"))
}

fn arg_float(_h: &PyHost, args: &[Value], i: usize) -> Result<f64, String> {
    match args.get(i) {
        Some(Value::Int(n)) => Ok(*n as f64),
        Some(Value::Float(f)) => Ok(*f),
        Some(Value::Bool(b)) => Ok(*b as i64 as f64),
        _ => Err(type_error("expected a number argument")),
    }
}

/// Materialize a sequence argument (list/tuple/str) into a vector of elements.
fn seq_items(h: &mut PyHost, v: Option<&Value>) -> Result<Vec<Value>, String> {
    let v = match v {
        Some(v) => v.clone(),
        None => return Err(type_error("expected a sequence argument")),
    };
    // For str, materialize single-character strings (CPython choice('abc')).
    let chars: Option<Vec<char>> = match h.get(&v) {
        Some(PyObj::Str(s)) => Some(s.chars().collect()),
        _ => None,
    };
    if let Some(chars) = chars {
        return Ok(chars
            .into_iter()
            .map(|c| h.new_str(c.to_string()))
            .collect());
    }
    match h.get(&v) {
        Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => Ok(l.clone()),
        _ => Err(type_error(&format!(
            "'{}' object is not a valid sequence",
            h.type_name(&v)
        ))),
    }
}
