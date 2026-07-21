//! The `statistics` standard-library module: `mean`, `median`, `median_low`,
//! `median_high`, `mode`, `pvariance`, `variance`, `pstdev`, `stdev`.
//!
//! CPython computes these with exact rationals and then converts back to the
//! input type (so `mean([1, 2, 3])` is the int `2`, but `mean([1, 2])` is the
//! float `1.5`). This port reproduces that behaviour for integer inputs using
//! exact `i128` arithmetic: it returns an int when the exact result is integral
//! and a float otherwise. Any float (or big-int) in the data drops to an f64
//! computation returning a float, matching CPython's float result type there.
//!
//! Variance uses the exact identity
//!   `sum((x-mean)^2) * n == n*sum(x^2) - (sum x)^2`
//! so the population/sample variances are `(n*Σx² − (Σx)²) / n²` and
//! `… / (n·(n−1))` respectively — integer numerator and denominator, reduced.
//! Standard deviations are `sqrt(variance)`, always a float (as in CPython).
//!
//! Wiring (done by the parent): an `import_module` arm for `"statistics"` calling
//! [`entries`], and a `call_builtin_function` arm routing `statistics.*` to
//! [`call`].

use crate::host::{type_error, PyHost, PyObj};
use fusevm::{NumOp, Value};
use num_bigint::BigInt;
use num_traits::ToPrimitive;

pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    [
        "mean",
        "median",
        "median_low",
        "median_high",
        "mode",
        "pvariance",
        "variance",
        "pstdev",
        "stdev",
    ]
    .iter()
    .map(|name| {
        (
            (*name).to_string(),
            h.alloc(PyObj::Builtin(format!("statistics.{name}"))),
        )
    })
    .collect()
}

pub fn call(h: &mut PyHost, fname: &str, args: &[Value]) -> Option<Result<Value, String>> {
    match fname {
        "mean" => Some(mean(h, args)),
        "median" => Some(median(h, args)),
        "median_low" => Some(median_low(h, args)),
        "median_high" => Some(median_high(h, args)),
        "mode" => Some(mode(h, args)),
        "pvariance" => Some(variance_family(h, args, false)),
        "variance" => Some(variance_family(h, args, true)),
        "pstdev" => Some(stdev_family(h, args, false)),
        "stdev" => Some(stdev_family(h, args, true)),
        _ => None,
    }
}

// ── numeric helpers ──────────────────────────────────────────────────────────

/// Materialise the data argument into a list of values.
fn data_items(h: &mut PyHost, args: &[Value], fname: &str) -> Result<Vec<Value>, String> {
    let v = args
        .first()
        .ok_or_else(|| type_error(&format!("{fname}() missing required argument: 'data'")))?;
    h.iter_items(v)
}

/// `v` as f64 when it is any numeric type (int/float/bool/big-int).
fn as_f64(h: &PyHost, v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        Value::Bool(b) => Some(*b as i64 as f64),
        Value::Obj(_) => match h.get(v) {
            Some(PyObj::BigInt(b)) => b.to_f64(),
            _ => None,
        },
        _ => None,
    }
}

/// True when every value is a plain (i64-range) int or bool — the exact path.
fn all_small_int(h: &PyHost, items: &[Value]) -> bool {
    items.iter().all(|v| h.as_int(v).is_some())
}

fn require_numeric(h: &PyHost, items: &[Value]) -> Result<Vec<f64>, String> {
    items
        .iter()
        .map(|v| {
            as_f64(h, v).ok_or_else(|| {
                type_error(&format!(
                    "can't convert type '{}' to a number",
                    h.type_name(v)
                ))
            })
        })
        .collect()
}

fn stat_err(msg: &str) -> String {
    format!("StatisticsError: {msg}")
}

fn gcd(mut a: i128, mut b: i128) -> i128 {
    a = a.abs();
    b = b.abs();
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// An `i128` as the narrowest Python int (native int, else a big-int).
fn int_value(h: &mut PyHost, x: i128) -> Value {
    match i64::try_from(x) {
        Ok(n) => Value::Int(n),
        Err(_) => h.alloc(PyObj::BigInt(BigInt::from(x))),
    }
}

/// The exact rational `num/den` (den > 0) as an int when integral, else a float.
fn ratio_value(h: &mut PyHost, num: i128, den: i128) -> Value {
    let g = gcd(num, den).max(1);
    let (num, den) = (num / g, den / g);
    if den == 1 {
        int_value(h, num)
    } else {
        Value::Float(num as f64 / den as f64)
    }
}

/// Sort a copy of `items` ascending by the host ordering (stable).
fn sorted_copy(h: &mut PyHost, items: &[Value]) -> Result<Vec<Value>, String> {
    let mut out = items.to_vec();
    let mut err: Option<String> = None;
    out.sort_by(|a, b| {
        if err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match h.compare(NumOp::Lt, a, b) {
            Ok(v) => {
                if h.truthy(&v) {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            }
            Err(e) => {
                err = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

// ── mean ─────────────────────────────────────────────────────────────────────

fn mean(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let items = data_items(h, args, "mean")?;
    let n = items.len();
    if n == 0 {
        return Err(stat_err("mean requires at least one data point"));
    }
    if all_small_int(h, &items) {
        let sum: i128 = items.iter().map(|v| h.as_int(v).unwrap() as i128).sum();
        Ok(ratio_value(h, sum, n as i128))
    } else {
        let nums = require_numeric(h, &items)?;
        Ok(Value::Float(nums.iter().sum::<f64>() / n as f64))
    }
}

// ── median family ────────────────────────────────────────────────────────────

fn median(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let items = data_items(h, args, "median")?;
    if items.is_empty() {
        return Err(stat_err("no median for empty data"));
    }
    let sorted = sorted_copy(h, &items)?;
    let n = sorted.len();
    if n % 2 == 1 {
        // Odd count: the exact middle element, unchanged.
        Ok(sorted[n / 2].clone())
    } else {
        // Even count: (a + b) / 2 — true division, always a float.
        let a = as_f64(h, &sorted[n / 2 - 1])
            .ok_or_else(|| type_error("median requires numeric data"))?;
        let b =
            as_f64(h, &sorted[n / 2]).ok_or_else(|| type_error("median requires numeric data"))?;
        Ok(Value::Float((a + b) / 2.0))
    }
}

fn median_low(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let items = data_items(h, args, "median_low")?;
    if items.is_empty() {
        return Err(stat_err("no median for empty data"));
    }
    let sorted = sorted_copy(h, &items)?;
    let n = sorted.len();
    let idx = if n % 2 == 1 { n / 2 } else { n / 2 - 1 };
    Ok(sorted[idx].clone())
}

fn median_high(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let items = data_items(h, args, "median_high")?;
    if items.is_empty() {
        return Err(stat_err("no median for empty data"));
    }
    let sorted = sorted_copy(h, &items)?;
    let n = sorted.len();
    // High median is index n//2 for both parities.
    Ok(sorted[n / 2].clone())
}

// ── mode ─────────────────────────────────────────────────────────────────────

fn mode(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let items = data_items(h, args, "mode")?;
    if items.is_empty() {
        return Err(stat_err("no mode for empty data"));
    }
    // Count by structural equality, preserving first-seen order; ties resolve to
    // the first value reaching the maximum count (CPython's Counter ordering).
    let mut groups: Vec<(Value, usize)> = Vec::new();
    for x in &items {
        if let Some(slot) = groups.iter_mut().find(|(v, _)| h.equal(v, x)) {
            slot.1 += 1;
        } else {
            groups.push((x.clone(), 1));
        }
    }
    // `max_by_key` would return the LAST max on a tie; scan for the FIRST group
    // that reaches the maximum count instead.
    let max_count = groups.iter().map(|(_, c)| *c).max().unwrap();
    let best = groups
        .iter()
        .find(|(_, c)| *c == max_count)
        .map(|(v, _)| v.clone())
        .unwrap();
    Ok(best)
}

// ── variance / stdev ─────────────────────────────────────────────────────────

/// The exact integer numerator/denominator of the requested variance, or the
/// f64 fallback when the data is not all small ints. `sample` selects the
/// Bessel-corrected (n−1) denominator; otherwise the population (n) form.
enum Variance {
    Exact { num: i128, den: i128 },
    Float(f64),
}

fn compute_variance(h: &PyHost, items: &[Value], sample: bool) -> Result<Variance, String> {
    let n = items.len() as i128;
    if all_small_int(h, items) {
        let sx: i128 = items.iter().map(|v| h.as_int(v).unwrap() as i128).sum();
        let sxx: i128 = items
            .iter()
            .map(|v| {
                let x = h.as_int(v).unwrap() as i128;
                x * x
            })
            .sum();
        // sum((x-mean)^2) * n  ==  n*Σx² − (Σx)²
        let ss_times_n = n * sxx - sx * sx;
        let den = if sample { n * (n - 1) } else { n * n };
        Ok(Variance::Exact {
            num: ss_times_n,
            den,
        })
    } else {
        let nums = require_numeric(h, items)?;
        let mean = nums.iter().sum::<f64>() / nums.len() as f64;
        let ss: f64 = nums.iter().map(|x| (x - mean) * (x - mean)).sum();
        let div = if sample {
            (nums.len() - 1) as f64
        } else {
            nums.len() as f64
        };
        Ok(Variance::Float(ss / div))
    }
}

fn variance_family(h: &mut PyHost, args: &[Value], sample: bool) -> Result<Value, String> {
    let items = data_items(h, args, if sample { "variance" } else { "pvariance" })?;
    let n = items.len();
    if sample {
        if n < 2 {
            return Err(stat_err("variance requires at least two data points"));
        }
    } else if n < 1 {
        return Err(stat_err("pvariance requires at least one data point"));
    }
    match compute_variance(h, &items, sample)? {
        Variance::Exact { num, den } => Ok(ratio_value(h, num, den)),
        Variance::Float(f) => Ok(Value::Float(f)),
    }
}

fn stdev_family(h: &mut PyHost, args: &[Value], sample: bool) -> Result<Value, String> {
    let items = data_items(h, args, if sample { "stdev" } else { "pstdev" })?;
    let n = items.len();
    if sample {
        if n < 2 {
            return Err(stat_err("stdev requires at least two data points"));
        }
    } else if n < 1 {
        return Err(stat_err("pstdev requires at least one data point"));
    }
    // Standard deviation is sqrt(variance); CPython always returns a float here.
    let var = match compute_variance(h, &items, sample)? {
        Variance::Exact { num, den } => num as f64 / den as f64,
        Variance::Float(f) => f,
    };
    Ok(Value::Float(var.sqrt()))
}
