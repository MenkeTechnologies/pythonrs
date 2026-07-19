//! Python stdlib `functools`, implemented over the pythonrs host object model.
//!
//! Provided: `reduce`, `partial`, `lru_cache` / `cache`.
//!
//! `partial` and `lru_cache` are backed by the host's `PyObj::Partial` /
//! `PyObj::LruCache` callable variants (added alongside this file); `host::invoke`
//! handles them. `lru_cache` supports both the bare `@lru_cache` form and the
//! parameterized `@lru_cache(maxsize=N)` form — the latter returns a
//! `functools.partial` over an internal `__lru_wrap` builtin so the eventual
//! decorator application reaches back here with the captured `maxsize`.
//!
//! Iteration uses `host::iter_vec` (generator-safe); the callback is dispatched
//! through `host::invoke` (the VM's `CALL_VALUE` path). Both re-enter
//! `with_host`, so this dispatch layer is a free function and holds no host
//! borrow across a call — mirroring `builtins::call_math`.

use crate::host::{self, PyHost, PyObj};
use fusevm::Value;

/// Entries for `import functools` — `(attr_name, Value)`. Takes `&mut PyHost` so
/// it splices into the `with_host(|h| …)` block of `host::import_module`.
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    vec![
        (
            "reduce".to_string(),
            h.alloc(PyObj::Builtin("functools.reduce".into())),
        ),
        (
            "partial".to_string(),
            h.alloc(PyObj::Builtin("functools.partial".into())),
        ),
        (
            "lru_cache".to_string(),
            h.alloc(PyObj::Builtin("functools.lru_cache".into())),
        ),
        (
            "cache".to_string(),
            h.alloc(PyObj::Builtin("functools.cache".into())),
        ),
    ]
}

/// Dispatch `functools.<fname>(args, kwargs)`. Returns `None` when `fname` is not
/// ours.
pub fn call(
    fname: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Option<Result<Value, String>> {
    match fname {
        "reduce" => Some(reduce(args)),
        "partial" => Some(partial(args, kwargs)),
        "lru_cache" => Some(lru_cache(args, kwargs)),
        // `functools.cache` == `lru_cache(maxsize=None)`.
        "cache" => Some(lru_cache_wrap(args, &[])),
        // Internal: the decorator produced by `lru_cache(maxsize=…)` applies here.
        "__lru_wrap" => Some(lru_cache_wrap(args, kwargs)),
        _ => None,
    }
}

/// `functools.partial(func, *args, **kwargs)`.
fn partial(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let func = args
        .first()
        .cloned()
        .ok_or_else(|| host::type_error("partial() takes at least 1 argument (0 given)"))?;
    Ok(host::make_partial(
        func,
        args[1..].to_vec(),
        kwargs.to_vec(),
    ))
}

/// `functools.lru_cache` — either the bare form `lru_cache(func)` or the
/// parameterized `lru_cache(maxsize=N)` / `lru_cache(None)` returning a decorator.
fn lru_cache(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    // Bare `@lru_cache` — the first positional is the function to wrap. A first
    // positional that is an int is the old `lru_cache(maxsize)` positional form.
    if let Some(first) = args.first() {
        let is_callable = host::with_host(|h| {
            matches!(
                h.get(first),
                Some(PyObj::Func(_))
                    | Some(PyObj::Builtin(_))
                    | Some(PyObj::BoundMethod { .. })
                    | Some(PyObj::Class(_))
                    | Some(PyObj::Partial { .. })
                    | Some(PyObj::LruCache { .. })
            )
        });
        if is_callable {
            return Ok(host::make_lru_cache(first.clone(), Some(128)));
        }
    }
    // Parameterized: build a decorator (a partial over `__lru_wrap`) carrying the
    // resolved `maxsize`.
    let maxsize = resolve_maxsize(args, kwargs);
    let wrapper = host::with_host(|h| h.alloc(PyObj::Builtin("functools.__lru_wrap".into())));
    let ms_kw = match maxsize {
        Some(n) => ("maxsize".to_string(), Value::Int(n as i64)),
        None => ("maxsize".to_string(), Value::Undef),
    };
    Ok(host::make_partial(wrapper, vec![], vec![ms_kw]))
}

/// Apply the lru wrapper to a function, reading `maxsize` from the `maxsize`
/// kwarg the decorator partial folded in. Absent / `None` → unbounded.
fn lru_cache_wrap(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let func = args
        .first()
        .cloned()
        .ok_or_else(|| host::type_error("lru_cache decorator expects a callable"))?;
    let maxsize = match kwargs.iter().find(|(k, _)| k == "maxsize").map(|(_, v)| v) {
        Some(Value::Int(n)) if *n >= 0 => Some(*n as usize),
        _ => None,
    };
    Ok(host::make_lru_cache(func, maxsize))
}

/// Resolve `maxsize` from `lru_cache(maxsize=N)` (kwarg) or `lru_cache(N)`
/// (positional). `None` (Python `None`) means unbounded → `None` here.
fn resolve_maxsize(args: &[Value], kwargs: &[(String, Value)]) -> Option<usize> {
    let v = kwargs
        .iter()
        .find(|(k, _)| k == "maxsize")
        .map(|(_, v)| v.clone())
        .or_else(|| args.first().cloned());
    match v {
        Some(Value::Int(n)) if n >= 0 => Some(n as usize),
        _ => None,
    }
}

/// `reduce(function, iterable[, initializer])` — left fold.
///
/// With no `initializer` the first element seeds the accumulator; an empty
/// iterable then raises `TypeError`, exactly as CPython's `functools.reduce`.
fn reduce(args: &[Value]) -> Result<Value, String> {
    let func = args
        .first()
        .cloned()
        .ok_or_else(|| host::type_error("reduce expected at least 2 arguments, got 0"))?;
    let iterable = args
        .get(1)
        .ok_or_else(|| host::type_error("reduce expected at least 2 arguments, got 1"))?;
    let items = host::iter_vec(iterable)?;

    let mut iter = items.into_iter();
    let mut acc = match args.get(2) {
        Some(init) => init.clone(),
        None => iter
            .next()
            .ok_or_else(|| host::type_error("reduce() of empty iterable with no initial value"))?,
    };
    for it in iter {
        acc = host::invoke(&func, vec![acc, it], vec![])?;
    }
    Ok(acc)
}
