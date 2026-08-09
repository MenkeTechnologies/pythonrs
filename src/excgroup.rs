//! PEP 654 exception groups — `ExceptionGroup`/`BaseExceptionGroup`, their
//! `split`/`subgroup`/`derive` protocol, and the reconstruction `except*` does
//! when its handlers finish.
//!
//! Ported from CPython's `Objects/exceptions.c`: `BaseExceptionGroup_new`,
//! `exceptiongroup_split_recursive`, `exceptiongroup_subset`,
//! `exception_group_projection` and `_PyExc_PrepReraiseStar`. The reconstruction
//! rules are subtle (a handler that re-raises its slice must be merged back into
//! the ORIGINAL group's nesting, while a freshly raised exception becomes a
//! sibling in a new group), so the C control flow is followed step for step
//! rather than re-derived.
//!
//! A group is stored as an ordinary `PyObj::Exception` whose `args` are
//! `[message, exceptions]` — exactly the constructor arguments, which is what
//! `BaseException.__repr__` renders. `.message`/`.exceptions` read them back.

use crate::builtins::{exception_isa, is_exception_class};
use crate::host::{self, with_host, PyHost, PyObj};
use fusevm::Value;
use std::collections::HashSet;

/// The builtin group classes. `BaseExceptionGroup` may hold any `BaseException`;
/// `ExceptionGroup` is restricted to `Exception` subclasses.
pub const BASE_GROUP: &str = "BaseExceptionGroup";
pub const GROUP: &str = "ExceptionGroup";

const MATCHER_TYPE_ERROR: &str = "TypeError: expected an exception type, a tuple of exception \
                                  types, or a callable (other than a class)";

// ── the object model ─────────────────────────────────────────────────────────

/// Whether `class` is a group type: the two builtins, or a user class deriving
/// from either.
pub fn class_is_group(h: &PyHost, class: &str) -> bool {
    class == GROUP || class == BASE_GROUP || h.mro_of(class).iter().any(|c| c == BASE_GROUP)
}

/// Whether `v` is an exception-group instance.
pub fn is_group(h: &PyHost, v: &Value) -> bool {
    match h.get(v) {
        Some(PyObj::Exception { class, .. }) => class_is_group(h, class),
        Some(PyObj::Instance(i)) => class_is_group(h, &i.class),
        _ => false,
    }
}

/// A group's `(message, exceptions)` — its two constructor arguments. `None`
/// when `v` is not a group (or a group whose `args` were tampered with).
pub fn group_parts(h: &PyHost, v: &Value) -> Option<(Value, Vec<Value>)> {
    if !is_group(h, v) {
        return None;
    }
    let args = match h.get(v) {
        Some(PyObj::Exception { args, .. }) => args.clone(),
        Some(PyObj::Instance(i)) => h.exc_instance_args(&i.dict),
        _ => return None,
    };
    let (msg, excs) = (args.first()?.clone(), args.get(1)?);
    let items = match h.get(excs) {
        Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => l.clone(),
        _ => return None,
    };
    Some((msg, items))
}

/// A group's direct sub-exceptions (empty for a non-group).
fn group_excs(h: &PyHost, v: &Value) -> Vec<Value> {
    group_parts(h, v).map(|(_, e)| e).unwrap_or_default()
}

/// The class name of any exception value.
fn class_of(h: &PyHost, v: &Value) -> String {
    match h.get(v) {
        Some(PyObj::Exception { class, .. }) => class.clone(),
        Some(PyObj::Instance(i)) => i.class.clone(),
        _ => h.type_name(v),
    }
}

/// Whether `v` is an exception instance at all — the check
/// `BaseExceptionGroup.__new__` runs over every item of its second argument.
fn is_exception_instance(h: &PyHost, v: &Value) -> bool {
    match h.get(v) {
        Some(PyObj::Exception { .. }) => true,
        Some(PyObj::Instance(i)) => h.class_is_exception(&i.class),
        _ => false,
    }
}

/// Whether `v` derives from `Exception` (rather than only `BaseException`) —
/// what decides whether a group may be an `ExceptionGroup`.
fn is_nonbase_exception(h: &PyHost, v: &Value) -> bool {
    exception_isa(&class_of(h, v), "Exception", h)
}

/// `BaseExceptionGroup(message, exceptions)` / `ExceptionGroup(...)` with
/// CPython's argument validation, including the narrowing that makes
/// `BaseExceptionGroup` return an `ExceptionGroup` when nothing inside it is a
/// bare `BaseException`.
pub fn construct(h: &mut PyHost, class: &str, args: &[Value]) -> Result<Value, String> {
    if args.len() != 2 {
        return Err(format!(
            "TypeError: BaseExceptionGroup.__new__() takes exactly 2 arguments ({} given)",
            args.len()
        ));
    }
    let message = &args[0];
    if !matches!(h.get(message), Some(PyObj::Str(_))) && !matches!(message, Value::Str(_)) {
        return Err(format!(
            "TypeError: BaseExceptionGroup.__new__() argument 1 must be str, not {}",
            h.type_name(message)
        ));
    }
    let items = match h.get(&args[1]) {
        Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => l.clone(),
        _ => return Err("TypeError: second argument (exceptions) must be a sequence".to_string()),
    };
    if items.is_empty() {
        return Err(
            "ValueError: second argument (exceptions) must be a non-empty sequence".to_string(),
        );
    }
    let mut nested_base = false;
    for (i, e) in items.iter().enumerate() {
        if !is_exception_instance(h, e) {
            return Err(format!(
                "ValueError: Item {i} of second argument (exceptions) is not an exception"
            ));
        }
        if !is_nonbase_exception(h, e) {
            nested_base = true;
        }
    }
    // The class actually built: `ExceptionGroup` refuses bare `BaseException`s,
    // and `BaseExceptionGroup` narrows to `ExceptionGroup` when it holds none.
    let cls = match class {
        GROUP if nested_base => {
            return Err("TypeError: Cannot nest BaseExceptions in an ExceptionGroup".to_string())
        }
        BASE_GROUP if !nested_base => GROUP,
        other => other,
    };
    Ok(h.alloc(PyObj::Exception {
        class: cls.to_string(),
        args: args.to_vec(),
    }))
}

/// `_PyExc_CreateExceptionGroup` — build a group from a Rust list, letting the
/// constructor pick `ExceptionGroup` vs `BaseExceptionGroup`.
pub fn create(h: &mut PyHost, message: &str, excs: Vec<Value>) -> Value {
    let msg = h.new_str(message);
    let list = h.new_list(excs);
    // The inputs are exception objects by construction here, so the validating
    // constructor cannot fail; fall back to a bare group if it somehow does.
    construct(h, BASE_GROUP, &[msg.clone(), list.clone()]).unwrap_or_else(|_| {
        h.alloc(PyObj::Exception {
            class: BASE_GROUP.to_string(),
            args: vec![msg, list],
        })
    })
}

// ── split / subgroup ─────────────────────────────────────────────────────────

/// How `split`/`subgroup` decide whether a leaf exception matches.
enum Matcher {
    /// An exception class or a tuple of them (`eg.split(ValueError)`).
    Type(Value),
    /// Any other callable (`eg.split(lambda e: ...)`).
    Predicate(Value),
    /// Heap ids of the leaves to keep — the internal matcher the re-raise
    /// projection uses to carve the original group down to what was re-raised.
    Ids(HashSet<u32>),
}

/// CPython's `get_matcher_type`.
fn matcher_of(v: &Value) -> Result<Matcher, String> {
    let ok = with_host(|h| {
        if is_exception_class_value(h, v) {
            return Some(true);
        }
        if let Some(PyObj::Tuple(ts)) = h.get(v) {
            let all = ts.iter().all(|t| is_exception_class_value(h, t));
            return if all { Some(true) } else { None };
        }
        if is_callable(h, v) {
            return Some(false);
        }
        None
    });
    match ok {
        Some(true) => Ok(Matcher::Type(v.clone())),
        Some(false) => Ok(Matcher::Predicate(v.clone())),
        None => Err(MATCHER_TYPE_ERROR.to_string()),
    }
}

fn is_exception_class_value(h: &PyHost, v: &Value) -> bool {
    match h.get(v) {
        Some(PyObj::Builtin(n)) => is_exception_class(n),
        Some(PyObj::Class(n)) => h.class_is_exception(n),
        _ => false,
    }
}

fn is_callable(h: &PyHost, v: &Value) -> bool {
    match h.get(v) {
        Some(PyObj::Func(_))
        | Some(PyObj::Builtin(_))
        | Some(PyObj::Class(_))
        | Some(PyObj::Partial { .. })
        | Some(PyObj::StaticMethod(_))
        | Some(PyObj::ClassMethod(_))
        | Some(PyObj::BoundMethod { .. }) => true,
        Some(PyObj::Instance(i)) => h.class_lookup(&i.class, "__call__").is_some(),
        _ => false,
    }
}

/// CPython's `exceptiongroup_split_check_match`. A predicate runs real Python,
/// so the host borrow is released around the call.
fn check_match(exc: &Value, m: &Matcher) -> Result<bool, String> {
    match m {
        Matcher::Type(t) => Ok(with_host(|h| crate::builtins::exc_matches(h, exc, t))),
        Matcher::Predicate(f) => {
            let r = host::invoke(f, vec![exc.clone()], vec![])?;
            Ok(with_host(|h| h.truthy(&r)))
        }
        // A group never matches by leaf identity — only the leaves inside it do,
        // which is what makes the projection rebuild the original nesting.
        Matcher::Ids(ids) => Ok(with_host(|h| {
            !is_group(h, exc) && matches!(exc, Value::Obj(id) if ids.contains(id))
        })),
    }
}

/// CPython's `exceptiongroup_subset`: wrap `excs` in a group derived from
/// `orig`, carrying `orig`'s chaining metadata. `None` when `excs` is empty.
fn subset(orig: &Value, excs: Vec<Value>) -> Option<Value> {
    if excs.is_empty() {
        return None;
    }
    Some(with_host(|h| derive_from(h, orig, excs)))
}

/// `orig.derive(excs)` plus the metadata copy `split` performs: the derived
/// group carries `orig`'s traceback, `__context__`, `__cause__` and `__notes__`,
/// and is linked back to `orig`'s split root.
fn derive_from(h: &mut PyHost, orig: &Value, excs: Vec<Value>) -> Value {
    let msg = group_parts(h, orig)
        .map(|(m, _)| m)
        .unwrap_or_else(|| h.new_str(""));
    let list = h.new_list(excs);
    let eg = construct(h, BASE_GROUP, &[msg.clone(), list.clone()]).unwrap_or_else(|_| {
        h.alloc(PyObj::Exception {
            class: BASE_GROUP.to_string(),
            args: vec![msg, list],
        })
    });
    if let (Value::Obj(src), Value::Obj(dst)) = (orig, &eg) {
        if let Some(tb) = h.exc_tb.get(src).cloned() {
            h.exc_tb.insert(*dst, tb);
        }
        if let Some(notes) = h
            .func_attrs
            .get(src)
            .and_then(|m| m.get("__notes__"))
            .cloned()
        {
            h.func_attrs
                .entry(*dst)
                .or_default()
                .insert("__notes__".to_string(), notes);
        }
        if h.suppress_context.contains(src) {
            h.suppress_context.insert(*dst);
        }
        let root = h.eg_split_root.get(src).copied().unwrap_or(*src);
        h.eg_split_root.insert(*dst, root);
    }
    let (cause, context) = h.exc_link(orig);
    h.set_exc_link(&eg, cause, context);
    eg
}

/// CPython's `exceptiongroup_split_recursive`, returning `(match, rest)`.
fn split_recursive(
    exc: &Value,
    m: &Matcher,
    construct_rest: bool,
) -> Result<(Option<Value>, Option<Value>), String> {
    if check_match(exc, m)? {
        return Ok((Some(exc.clone()), None));
    }
    if !with_host(|h| is_group(h, exc)) {
        // A leaf that did not match is entirely `rest`.
        return Ok((None, construct_rest.then(|| exc.clone())));
    }
    let mut matched = Vec::new();
    let mut rest = Vec::new();
    for e in with_host(|h| group_excs(h, exc)) {
        let (mm, rr) = split_recursive(&e, m, construct_rest)?;
        matched.extend(mm);
        rest.extend(rr);
    }
    Ok((
        subset(exc, matched),
        if construct_rest {
            subset(exc, rest)
        } else {
            None
        },
    ))
}

/// `eg.split(matcher)` — `(matching subgroup or None, remainder or None)`.
pub fn split(exc: &Value, matcher: &Value) -> Result<(Option<Value>, Option<Value>), String> {
    let m = matcher_of(matcher)?;
    split_recursive(exc, &m, true)
}

/// `eg.subgroup(matcher)` — the matching subgroup, or `None`.
pub fn subgroup(exc: &Value, matcher: &Value) -> Result<Option<Value>, String> {
    let m = matcher_of(matcher)?;
    Ok(split_recursive(exc, &m, false)?.0)
}

/// `eg.derive(excs)` — a plain group with this group's message and `excs`. As in
/// CPython, the default `derive` builds a `BaseExceptionGroup` (narrowed to
/// `ExceptionGroup`), NOT the receiver's own subclass.
pub fn derive(exc: &Value, excs: &Value) -> Result<Value, String> {
    let items = with_host(|h| match h.get(excs) {
        Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => Some(l.clone()),
        _ => None,
    })
    .ok_or_else(|| "TypeError: second argument (exceptions) must be a sequence".to_string())?;
    with_host(|h| {
        let msg = group_parts(h, exc)
            .map(|(m, _)| m)
            .unwrap_or_else(|| h.new_str(""));
        let list = h.new_list(items);
        construct(h, BASE_GROUP, &[msg, list])
    })
}

// ── `except*` reconstruction ─────────────────────────────────────────────────

/// `_PyEval_ExceptionGroupMatch` — match `exc` against one `except*` clause's
/// type, returning `(matched part, remainder)`. A *naked* exception that matches
/// is wrapped in a one-element group, which is what the handler is bound to.
pub fn eg_match(exc: &Value, typ: &Value) -> Result<(Option<Value>, Option<Value>), String> {
    if with_host(|h| crate::builtins::exc_matches(h, exc, typ)) {
        let matched = with_host(|h| {
            if is_group(h, exc) {
                return exc.clone();
            }
            let msg = h.new_str("");
            let tup = h.new_tuple(vec![exc.clone()]);
            construct(h, BASE_GROUP, &[msg.clone(), tup.clone()]).unwrap_or_else(|_| {
                h.alloc(PyObj::Exception {
                    class: BASE_GROUP.to_string(),
                    args: vec![msg, tup],
                })
            })
        });
        return Ok((Some(matched), None));
    }
    if with_host(|h| is_group(h, exc)) {
        return split(exc, typ);
    }
    Ok((None, None))
}

/// Every leaf exception id reachable under `exc` (CPython's
/// `collect_exception_group_leaf_ids`).
fn collect_leaf_ids(h: &PyHost, exc: &Value, out: &mut HashSet<u32>) {
    if !is_group(h, exc) {
        if let Value::Obj(id) = exc {
            out.insert(*id);
        }
        return;
    }
    for e in group_excs(h, exc) {
        collect_leaf_ids(h, &e, out);
    }
}

/// CPython's `exception_group_projection`: the sub-group of `eg` holding every
/// leaf that appears anywhere in `keep`.
fn projection(eg: &Value, keep: &[Value]) -> Result<Option<Value>, String> {
    let ids = with_host(|h| {
        let mut ids = HashSet::new();
        for e in keep {
            collect_leaf_ids(h, e, &mut ids);
        }
        ids
    });
    Ok(split_recursive(eg, &Matcher::Ids(ids), false)?.0)
}

/// The split root an exception belongs to: its own id, unless it was carved out
/// of a larger group, in which case that group's root.
fn split_root(h: &PyHost, v: &Value) -> Option<u32> {
    let Value::Obj(id) = v else { return None };
    Some(h.eg_split_root.get(id).copied().unwrap_or(*id))
}

/// Whether `v` is `orig` itself or a group carved out of it — i.e. whether it
/// carries `orig`'s traceback and chaining rather than being newly built.
pub fn is_piece_of(h: &PyHost, v: &Value, orig: &Value) -> bool {
    split_root(h, v) == split_root(h, orig)
}

/// CPython's `_PyExc_PrepReraiseStar`: given the exception a `try` caught and
/// the list of exceptions its `except*` clauses left behind (each handler's own
/// escape, then the unhandled remainder), work out what to raise. Re-raised
/// pieces of `orig` are merged back into `orig`'s nesting; freshly raised ones
/// become siblings in a new unnamed group.
pub fn prep_reraise_star(orig: &Value, excs: &[Value]) -> Result<Option<Value>, String> {
    if excs.is_empty() {
        return Ok(None);
    }
    if !with_host(|h| is_group(h, orig)) {
        // A naked exception was caught and wrapped, so at most one clause could
        // have run and there is at most one exception to re-raise.
        return Ok(Some(excs[0].clone()));
    }
    let (mut raised, reraised): (Vec<Value>, Vec<Value>) = with_host(|h| {
        let root = split_root(h, orig);
        excs.iter()
            .cloned()
            .partition(|e| split_root(h, e) != root || !is_group(h, e))
    });
    let reraised_eg = projection(orig, &reraised)?;
    if raised.is_empty() {
        return Ok(reraised_eg);
    }
    raised.extend(reraised_eg);
    if raised.len() > 1 {
        Ok(Some(with_host(|h| create(h, "", raised))))
    } else {
        Ok(raised.into_iter().next())
    }
}
