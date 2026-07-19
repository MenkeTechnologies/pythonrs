//! Python stdlib `functools`, implemented over the pythonrs host object model.
//!
//! Provided: `reduce`.
//!
//! Skipped, with reasons:
//!   - `partial` — a `partial` object is a *new kind of callable* (a closure over
//!     a function plus pre-bound positional/keyword args). The host's `PyObj`
//!     callable variants are `Builtin(name)`, `Func(FuncVal)` (a compiled
//!     `def`/`lambda` template), `BoundMethod{recv,func}` (binds exactly one
//!     leading `self`), and `Class(name)`. None can represent an arbitrary
//!     args/kwargs pre-binding over an opaque callable, and `host::invoke` only
//!     accepts those variants, so there is no faithful representation without a
//!     new `PyObj` variant (a host change this file is not allowed to make).
//!   - `lru_cache` — out of scope per task.
//!   - `cmp_to_key` — out of scope per task.
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
    vec![(
        "reduce".to_string(),
        h.alloc(PyObj::Builtin("functools.reduce".into())),
    )]
}

/// Dispatch `functools.<fname>(args)`. Returns `None` when `fname` is not ours.
pub fn call(fname: &str, args: &[Value]) -> Option<Result<Value, String>> {
    match fname {
        "reduce" => Some(reduce(args)),
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
