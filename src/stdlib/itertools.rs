//! Python stdlib `itertools`, implemented over the pythonrs host object model.
//!
//! **Eager approximation.** CPython's `itertools` returns lazy iterators; this
//! module returns a concrete `list` of the fully-materialized results. For any
//! FINITE input, `list(itertools.f(...))` and `for x in itertools.f(...)` observe
//! exactly the same elements in the same order, so the eager form is
//! behaviorally identical for the bounded uses that dominate real code. The only
//! semantic loss is laziness itself: truly unbounded producers
//! (`count()`, `cycle()`, `repeat(x)` with no count) cannot be materialized, so
//! they are rejected up front with a clear error rather than hanging.
//!
//! Iteration of every input goes through `host::iter_vec`, which is the
//! generator-safe materializer (it holds no host borrow across a generator
//! resume). Every callback (`accumulate` func, `dropwhile`/`takewhile`
//! predicate, `groupby` key, `starmap` function) is dispatched through
//! `host::invoke`, the same call path the VM's `CALL_VALUE` handler uses. Because
//! `host::invoke`/`host::iter_vec` re-enter `with_host`, this dispatch layer is a
//! free function (like `builtins::call_math`) and must never hold a `&mut PyHost`
//! across an invoke/iterate call.

use crate::host::{self, with_host, PyHost, PyObj};
use fusevm::{NumOp, Value};

/// Entries for `import itertools` — `(attr_name, Value)`. Every function name is
/// a `PyObj::Builtin("itertools.<fn>")` that routes back into [`call`].
///
/// Takes `&mut PyHost` so it can be spliced directly into the `with_host(|h| …)`
/// block of `host::import_module` alongside the existing `math`/`sys` arms.
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    let names = [
        // bounded, materializable
        "chain",
        "repeat",
        "product",
        "permutations",
        "combinations",
        "combinations_with_replacement",
        "accumulate",
        "islice",
        "zip_longest",
        "groupby",
        "compress",
        "dropwhile",
        "takewhile",
        "starmap",
        "pairwise",
        // unbounded forms — resolve to a Builtin, then error cleanly on call
        "count",
        "cycle",
    ];
    names
        .iter()
        .map(|n| {
            let v = h.alloc(PyObj::Builtin(format!("itertools.{n}")));
            (n.to_string(), v)
        })
        .collect()
}

/// Dispatch `itertools.<fname>(args, kwargs)`. Returns `None` when `fname` is not
/// an itertools function (so the caller can fall through to the next module).
///
/// `kwargs` is threaded through because several itertools functions take
/// keyword-only parameters (`product(repeat=)`, `zip_longest(fillvalue=)`,
/// `accumulate(initial=)`, `groupby(key=)`); it mirrors the
/// `(name, args, kwargs)` shape `builtins::call_builtin_function` already has in
/// scope at the routing site.
pub fn call(
    fname: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Option<Result<Value, String>> {
    let r = match fname {
        "count" | "cycle" => Err(unbounded(fname)),
        "repeat" => repeat(args, kwargs),
        "chain" => chain(args),
        "chain.from_iterable" => chain_from_iterable(args),
        "product" => product(args, kwargs),
        "permutations" => permutations(args, kwargs),
        "combinations" => combinations(args, kwargs, false),
        "combinations_with_replacement" => combinations(args, kwargs, true),
        "accumulate" => accumulate(args, kwargs),
        "islice" => islice(args),
        "zip_longest" => zip_longest(args, kwargs),
        "groupby" => groupby(args, kwargs),
        "compress" => compress(args),
        "dropwhile" => dropwhile(args),
        "takewhile" => takewhile(args),
        "starmap" => starmap(args),
        "pairwise" => pairwise(args),
        _ => return None,
    };
    Some(r)
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn unbounded(name: &str) -> String {
    format!("itertools.{name}: unbounded form unsupported (use a bounded variant)")
}

fn type_err(fname: &str, msg: &str) -> String {
    host::type_error(&format!("itertools.{fname}: {msg}"))
}

/// A positional int argument that may be Python `None` (→ `None` here).
fn opt_int(v: &Value) -> Option<i64> {
    if matches!(v, Value::Undef) {
        None
    } else {
        with_host(|h| h.as_int(v))
    }
}

fn kw<'a>(kwargs: &'a [(String, Value)], name: &str) -> Option<&'a Value> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn new_list(items: Vec<Value>) -> Value {
    with_host(|h| h.new_list(items))
}

fn new_tuple(items: Vec<Value>) -> Value {
    with_host(|h| h.new_tuple(items))
}

// ── functions ────────────────────────────────────────────────────────────────

/// `chain(*iterables)` — concatenate every iterable.
fn chain(args: &[Value]) -> Result<Value, String> {
    let mut out = Vec::new();
    for a in args {
        out.extend(host::iter_vec(a)?);
    }
    Ok(new_list(out))
}

/// `chain.from_iterable(iterable)` — flatten one level.
fn chain_from_iterable(args: &[Value]) -> Result<Value, String> {
    let outer = args
        .first()
        .ok_or_else(|| type_err("chain.from_iterable", "missing iterable"))?;
    let mut out = Vec::new();
    for sub in host::iter_vec(outer)? {
        out.extend(host::iter_vec(&sub)?);
    }
    Ok(new_list(out))
}

/// `repeat(object[, times])` — `times` copies (keyword `times=` accepted). With
/// no count it is the unbounded form and is rejected.
fn repeat(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let obj = args
        .first()
        .cloned()
        .ok_or_else(|| type_err("repeat", "missing object argument"))?;
    let times = args
        .get(1)
        .or_else(|| kw(kwargs, "times"))
        .map(opt_int)
        .unwrap_or(None);
    match times {
        None => Err(unbounded("repeat")),
        Some(n) => {
            let n = n.max(0) as usize;
            Ok(new_list(vec![obj; n]))
        }
    }
}

/// `product(*iterables, repeat=1)` — cartesian product as a list of tuples.
fn product(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let repeat = match kw(kwargs, "repeat").map(opt_int) {
        Some(Some(r)) if r < 0 => return Err(type_err("product", "repeat must be non-negative")),
        Some(Some(r)) => r as usize,
        _ => 1,
    };
    let mut pools: Vec<Vec<Value>> = Vec::with_capacity(args.len());
    for a in args {
        pools.push(host::iter_vec(a)?);
    }
    // `product(a, b, repeat=n)` == `product(a, b, a, b, …)` (n copies).
    let base = pools.clone();
    for _ in 1..repeat {
        pools.extend(base.iter().cloned());
    }
    if repeat == 0 {
        pools.clear();
    }
    // Build the cross product; starts as one empty tuple (`product()` == `[()]`).
    let mut acc: Vec<Vec<Value>> = vec![Vec::new()];
    for pool in &pools {
        let mut next = Vec::with_capacity(acc.len() * pool.len());
        for prefix in &acc {
            for item in pool {
                let mut row = prefix.clone();
                row.push(item.clone());
                next.push(row);
            }
        }
        acc = next;
    }
    Ok(new_list(acc.into_iter().map(new_tuple).collect()))
}

/// `permutations(iterable, r=None)` — r-length ordered arrangements (lexicographic
/// by input index, matching CPython).
fn permutations(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let pool = host::iter_vec(
        args.first()
            .ok_or_else(|| type_err("permutations", "missing iterable"))?,
    )?;
    let n = pool.len();
    let r = match args.get(1).or_else(|| kw(kwargs, "r")).map(opt_int) {
        Some(Some(r)) if r < 0 => return Err(host::type_error("r must be non-negative")),
        Some(Some(r)) => r as usize,
        _ => n,
    };
    let mut rows: Vec<Vec<usize>> = Vec::new();
    if r <= n {
        let mut used = vec![false; n];
        let mut cur = Vec::with_capacity(r);
        perm_rec(n, r, &mut used, &mut cur, &mut rows);
    }
    Ok(materialize(rows, &pool))
}

fn perm_rec(
    n: usize,
    r: usize,
    used: &mut [bool],
    cur: &mut Vec<usize>,
    out: &mut Vec<Vec<usize>>,
) {
    if cur.len() == r {
        out.push(cur.clone());
        return;
    }
    for i in 0..n {
        if used[i] {
            continue;
        }
        used[i] = true;
        cur.push(i);
        perm_rec(n, r, used, cur, out);
        cur.pop();
        used[i] = false;
    }
}

/// `combinations(iterable, r)` / `combinations_with_replacement(iterable, r)`.
fn combinations(
    args: &[Value],
    kwargs: &[(String, Value)],
    with_replacement: bool,
) -> Result<Value, String> {
    let fname = if with_replacement {
        "combinations_with_replacement"
    } else {
        "combinations"
    };
    let pool = host::iter_vec(
        args.first()
            .ok_or_else(|| type_err(fname, "missing iterable"))?,
    )?;
    let n = pool.len();
    let r = match args.get(1).or_else(|| kw(kwargs, "r")).map(opt_int) {
        Some(Some(r)) if r < 0 => return Err(host::type_error("r must be non-negative")),
        Some(Some(r)) => r as usize,
        _ => return Err(type_err(fname, "missing r argument")),
    };
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut cur = Vec::with_capacity(r);
    comb_rec(n, r, 0, with_replacement, &mut cur, &mut rows);
    Ok(materialize(rows, &pool))
}

fn comb_rec(
    n: usize,
    r: usize,
    start: usize,
    with_replacement: bool,
    cur: &mut Vec<usize>,
    out: &mut Vec<Vec<usize>>,
) {
    if cur.len() == r {
        out.push(cur.clone());
        return;
    }
    for i in start..n {
        cur.push(i);
        let next_start = if with_replacement { i } else { i + 1 };
        comb_rec(n, r, next_start, with_replacement, cur, out);
        cur.pop();
    }
}

/// Turn index rows into a list of value tuples.
fn materialize(rows: Vec<Vec<usize>>, pool: &[Value]) -> Value {
    let tuples = rows
        .into_iter()
        .map(|row| new_tuple(row.into_iter().map(|i| pool[i].clone()).collect()))
        .collect();
    new_list(tuples)
}

/// `accumulate(iterable[, func][, *, initial=None])` — running reduction.
fn accumulate(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let items = host::iter_vec(
        args.first()
            .ok_or_else(|| type_err("accumulate", "missing iterable"))?,
    )?;
    // 2nd positional is the binary func; `None` means the default (addition).
    let func = match args.get(1) {
        Some(v) if !matches!(v, Value::Undef) => Some(v.clone()),
        _ => None,
    };
    let initial = kw(kwargs, "initial")
        .filter(|v| !matches!(v, Value::Undef))
        .cloned();

    let mut out = Vec::new();
    let mut acc;
    let rest: &[Value];
    match &initial {
        Some(init) => {
            acc = init.clone();
            out.push(acc.clone());
            rest = &items;
        }
        None => {
            if items.is_empty() {
                return Ok(new_list(out));
            }
            acc = items[0].clone();
            out.push(acc.clone());
            rest = &items[1..];
        }
    }
    for it in rest {
        acc = match &func {
            Some(f) => host::invoke(f, vec![acc.clone(), it.clone()], vec![])?,
            None => with_host(|h| h.arith(NumOp::Add, &acc, it))?,
        };
        out.push(acc.clone());
    }
    Ok(new_list(out))
}

/// `islice(iterable, stop)` / `islice(iterable, start, stop[, step])`.
fn islice(args: &[Value]) -> Result<Value, String> {
    let items = host::iter_vec(
        args.first()
            .ok_or_else(|| type_err("islice", "missing iterable"))?,
    )?;
    let (start, stop, step) = match args.len() {
        0 | 1 => return Err(type_err("islice", "missing stop argument")),
        2 => (0i64, opt_int(&args[1]), 1i64),
        _ => {
            let start = opt_int(&args[1]).unwrap_or(0);
            let stop = opt_int(&args[2]);
            let step = args.get(3).map(opt_int).unwrap_or(None).unwrap_or(1);
            (start, stop, step)
        }
    };
    if start < 0 || step < 1 || matches!(stop, Some(s) if s < 0) {
        return Err(host::type_error(
            "Indices for islice() must be None or an integer: 0 <= x <= sys.maxsize.",
        ));
    }
    let stop = stop.unwrap_or(items.len() as i64);
    let mut out = Vec::new();
    let mut i = start;
    while i < stop && (i as usize) < items.len() {
        out.push(items[i as usize].clone());
        i += step;
    }
    Ok(new_list(out))
}

/// `zip_longest(*iterables, fillvalue=None)`.
fn zip_longest(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let fill = kw(kwargs, "fillvalue").cloned().unwrap_or(Value::Undef);
    let mut pools: Vec<Vec<Value>> = Vec::with_capacity(args.len());
    for a in args {
        pools.push(host::iter_vec(a)?);
    }
    let maxlen = pools.iter().map(|p| p.len()).max().unwrap_or(0);
    let mut out = Vec::with_capacity(maxlen);
    for i in 0..maxlen {
        let row: Vec<Value> = pools
            .iter()
            .map(|p| p.get(i).cloned().unwrap_or_else(|| fill.clone()))
            .collect();
        out.push(new_tuple(row));
    }
    Ok(new_list(out))
}

/// `groupby(iterable, key=None)` — consecutive runs of equal key. Eager form
/// returns a list of `(key, [group…])` tuples (CPython yields a sub-iterator).
fn groupby(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let items = host::iter_vec(
        args.first()
            .ok_or_else(|| type_err("groupby", "missing iterable"))?,
    )?;
    let key_func = match args.get(1).or_else(|| kw(kwargs, "key")) {
        Some(v) if !matches!(v, Value::Undef) => Some(v.clone()),
        _ => None,
    };
    // Compute each element's key once (CPython evaluates key per element once).
    let mut keys = Vec::with_capacity(items.len());
    for it in &items {
        let k = match &key_func {
            Some(f) => host::invoke(f, vec![it.clone()], vec![])?,
            None => it.clone(),
        };
        keys.push(k);
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let group_key = keys[i].clone();
        let mut group = vec![items[i].clone()];
        let mut j = i + 1;
        while j < items.len() && with_host(|h| h.equal(&keys[j], &group_key)) {
            group.push(items[j].clone());
            j += 1;
        }
        let group_list = new_list(group);
        out.push(new_tuple(vec![group_key, group_list]));
        i = j;
    }
    Ok(new_list(out))
}

/// `compress(data, selectors)` — keep `data[i]` where `selectors[i]` is truthy.
fn compress(args: &[Value]) -> Result<Value, String> {
    if args.len() < 2 {
        return Err(type_err("compress", "expected (data, selectors)"));
    }
    let data = host::iter_vec(&args[0])?;
    let selectors = host::iter_vec(&args[1])?;
    let out: Vec<Value> = data
        .into_iter()
        .zip(selectors)
        .filter(|(_, s)| with_host(|h| h.truthy(s)))
        .map(|(d, _)| d)
        .collect();
    Ok(new_list(out))
}

/// `dropwhile(predicate, iterable)` — drop the leading run where `predicate` is
/// true, then keep everything after (untested).
fn dropwhile(args: &[Value]) -> Result<Value, String> {
    if args.len() < 2 {
        return Err(type_err("dropwhile", "expected (predicate, iterable)"));
    }
    let pred = args[0].clone();
    let items = host::iter_vec(&args[1])?;
    let mut out = Vec::new();
    let mut dropping = true;
    for it in items {
        if dropping {
            let keep = host::invoke(&pred, vec![it.clone()], vec![])?;
            if with_host(|h| h.truthy(&keep)) {
                continue;
            }
            dropping = false;
        }
        out.push(it);
    }
    Ok(new_list(out))
}

/// `takewhile(predicate, iterable)` — take the leading run where `predicate` is
/// true, then stop.
fn takewhile(args: &[Value]) -> Result<Value, String> {
    if args.len() < 2 {
        return Err(type_err("takewhile", "expected (predicate, iterable)"));
    }
    let pred = args[0].clone();
    let items = host::iter_vec(&args[1])?;
    let mut out = Vec::new();
    for it in items {
        let keep = host::invoke(&pred, vec![it.clone()], vec![])?;
        if with_host(|h| h.truthy(&keep)) {
            out.push(it);
        } else {
            break;
        }
    }
    Ok(new_list(out))
}

/// `starmap(function, iterable)` — `function(*item)` for each item.
fn starmap(args: &[Value]) -> Result<Value, String> {
    if args.len() < 2 {
        return Err(type_err("starmap", "expected (function, iterable)"));
    }
    let func = args[0].clone();
    let items = host::iter_vec(&args[1])?;
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        let call_args = host::iter_vec(&it)?;
        out.push(host::invoke(&func, call_args, vec![])?);
    }
    Ok(new_list(out))
}

/// `pairwise(iterable)` — successive overlapping pairs.
fn pairwise(args: &[Value]) -> Result<Value, String> {
    let items = host::iter_vec(
        args.first()
            .ok_or_else(|| type_err("pairwise", "missing iterable"))?,
    )?;
    let mut out = Vec::new();
    for w in items.windows(2) {
        out.push(new_tuple(vec![w[0].clone(), w[1].clone()]));
    }
    Ok(new_list(out))
}
