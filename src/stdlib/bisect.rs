//! The `bisect` standard-library module: binary search / ordered insertion into
//! a sorted list, ported from CPython's `Lib/bisect.py` pure-Python fallback.
//!
//! `bisect_left` / `bisect_right` (alias `bisect`) return an insertion index;
//! `insort_left` / `insort_right` (alias `insort`) insert into the list in place,
//! preserving its identity. Element comparison uses the host's value ordering
//! (`PyHost::compare`), matching how `sorted()` orders the same values.
//!
//! Wiring (done by the parent): an `import_module` arm for `"bisect"` calling
//! [`entries`], and a `call_builtin_function` arm routing `bisect.*` to [`call`].

use crate::host::{type_error, PyHost, PyObj};
use fusevm::{NumOp, Value};

pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    [
        "bisect_left",
        "bisect_right",
        "bisect",
        "insort_left",
        "insort_right",
        "insort",
    ]
    .iter()
    .map(|name| {
        (
            (*name).to_string(),
            h.alloc(PyObj::Builtin(format!("bisect.{name}"))),
        )
    })
    .collect()
}

pub fn call(h: &mut PyHost, fname: &str, args: &[Value]) -> Option<Result<Value, String>> {
    match fname {
        "bisect_left" => Some(bisect_left(h, args)),
        // `bisect` is a documented alias for `bisect_right`.
        "bisect_right" | "bisect" => Some(bisect_right(h, args)),
        "insort_left" => Some(insort_left(h, args)),
        // `insort` is a documented alias for `insort_right`.
        "insort_right" | "insort" => Some(insort_right(h, args)),
        _ => None,
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// `a < b` via the host ordering.
fn lt(h: &mut PyHost, a: &Value, b: &Value) -> Result<bool, String> {
    let v = h.compare(NumOp::Lt, a, b)?;
    Ok(h.truthy(&v))
}

/// Snapshot the list at `args[0]`, or a TypeError if it is not a list.
fn list_snapshot(h: &PyHost, args: &[Value], fname: &str) -> Result<Vec<Value>, String> {
    let v = args
        .first()
        .ok_or_else(|| type_error(&format!("{fname} expected a list argument")))?;
    match h.get(v) {
        Some(PyObj::List(l)) => Ok(l.clone()),
        _ => Err(type_error(&format!(
            "{fname}() argument must be a list, not '{}'",
            h.type_name(v)
        ))),
    }
}

/// Resolve the optional `lo`/`hi` bounds (`args[2]`, `args[3]`) against a list of
/// length `len`, defaulting to `0` and `len`. Rejects a negative `lo`.
fn bounds(h: &PyHost, args: &[Value], len: usize) -> Result<(usize, usize), String> {
    let lo = match args.get(2).and_then(|v| h.as_int(v)) {
        Some(n) if n < 0 => return Err("ValueError: lo must be non-negative".into()),
        Some(n) => n as usize,
        None => 0,
    };
    let hi = match args.get(3).and_then(|v| h.as_int(v)) {
        Some(n) => (n.max(0) as usize).min(len),
        None => len,
    };
    Ok((lo.min(len), hi))
}

// ── searches ─────────────────────────────────────────────────────────────────

fn bisect_left(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let a = list_snapshot(h, args, "bisect_left")?;
    let x = args
        .get(1)
        .cloned()
        .ok_or_else(|| type_error("bisect_left expected 2 arguments"))?;
    let (mut lo, mut hi) = bounds(h, args, a.len())?;
    while lo < hi {
        let mid = (lo + hi) / 2;
        if lt(h, &a[mid], &x)? {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(Value::Int(lo as i64))
}

fn bisect_right(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let a = list_snapshot(h, args, "bisect_right")?;
    let x = args
        .get(1)
        .cloned()
        .ok_or_else(|| type_error("bisect_right expected 2 arguments"))?;
    let (mut lo, mut hi) = bounds(h, args, a.len())?;
    while lo < hi {
        let mid = (lo + hi) / 2;
        // Right variant: step past elements that are <= x (i.e. not x < a[mid]).
        if lt(h, &x, &a[mid])? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(Value::Int(lo as i64))
}

// ── insertions (in place) ────────────────────────────────────────────────────

fn insort_left(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let idx = match bisect_left(h, args)? {
        Value::Int(i) => i as usize,
        _ => unreachable!(),
    };
    insert_at(h, args, idx)
}

fn insort_right(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let idx = match bisect_right(h, args)? {
        Value::Int(i) => i as usize,
        _ => unreachable!(),
    };
    insert_at(h, args, idx)
}

/// Insert `args[1]` into the list `args[0]` at `idx`, rewriting the same object.
fn insert_at(h: &mut PyHost, args: &[Value], idx: usize) -> Result<Value, String> {
    let target = args
        .first()
        .cloned()
        .ok_or_else(|| type_error("insort expected a list argument"))?;
    let x = args
        .get(1)
        .cloned()
        .ok_or_else(|| type_error("insort expected 2 arguments"))?;
    let mut a = list_snapshot(h, args, "insort")?;
    let idx = idx.min(a.len());
    a.insert(idx, x);
    if let Some(PyObj::List(l)) = h.get_mut(&target) {
        *l = a;
    }
    Ok(Value::Undef)
}
