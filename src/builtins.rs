//! The pythonrs builtin layer: the fusevm `CallBuiltin` handler table
//! (`install`), the strict numeric hook, the Kernel builtin functions
//! (`print`, `len`, `range`, …) and the per-type method dispatch
//! (`str`/`list`/`dict`/… methods). Handlers pop their arguments off the VM
//! stack, call into the `host` object model, and push the result back.

use crate::host::{self, ops, with_host, Instance, IterState, PKey, PyObj};
use fusevm::{NumOp, Value, VM};
use indexmap::IndexMap;

/// Register every pythonrs builtin id on a VM.
pub fn install(vm: &mut VM) {
    vm.register_builtin(ops::GETLOCAL, b_getlocal);
    vm.register_builtin(ops::SETLOCAL, b_setlocal);
    vm.register_builtin(ops::SETGLOBAL, b_setglobal);
    vm.register_builtin(ops::DECLARE_GLOBAL, b_declare_global);
    vm.register_builtin(ops::DELNAME, b_delname);
    vm.register_builtin(ops::GETATTR, b_getattr);
    vm.register_builtin(ops::SETATTR, b_setattr);
    vm.register_builtin(ops::DELATTR, b_delattr);
    vm.register_builtin(ops::GETITEM, b_getitem);
    vm.register_builtin(ops::SETITEM, b_setitem);
    vm.register_builtin(ops::DELITEM, b_delitem);
    vm.register_builtin(ops::MKSTR, b_mkstr);
    vm.register_builtin(ops::MKLIST, b_mklist);
    vm.register_builtin(ops::MKTUPLE, b_mktuple);
    vm.register_builtin(ops::MKSET, b_mkset);
    vm.register_builtin(ops::MKDICT, b_mkdict);
    vm.register_builtin(ops::MKSLICE, b_mkslice);
    vm.register_builtin(ops::CALL, b_call);
    vm.register_builtin(ops::CALL_KW, b_call_kw);
    vm.register_builtin(ops::CALL_METHOD, b_call_method);
    vm.register_builtin(ops::CALL_METHOD_KW, b_call_method_kw);
    vm.register_builtin(ops::CALL_VALUE, b_call_value);
    vm.register_builtin(ops::CALL_VALUE_KW, b_call_value_kw);
    vm.register_builtin(ops::TRUTHY, b_truthy);
    vm.register_builtin(ops::TOSTR, b_tostr);
    vm.register_builtin(ops::FORMAT, b_format);
    vm.register_builtin(ops::MKFUNC, b_mkfunc);
    vm.register_builtin(ops::MKLAMBDA, b_mkfunc);
    vm.register_builtin(ops::BUILD_CLASS, b_build_class);
    vm.register_builtin(ops::GETITER, b_getiter);
    vm.register_builtin(ops::FORITER, b_foriter);
    vm.register_builtin(ops::CONTAINS, b_contains);
    vm.register_builtin(ops::IS, b_is);
    vm.register_builtin(ops::RAISE, b_raise);
    vm.register_builtin(ops::RERAISE, b_reraise);
    vm.register_builtin(ops::SIG_RETURN, b_sig_return);
    vm.register_builtin(ops::SIG_BREAK, b_noop);
    vm.register_builtin(ops::SIG_CONTINUE, b_noop);
    vm.register_builtin(ops::IMPORT, b_import);
    vm.register_builtin(ops::IMPORT_FROM, b_import_from);
    vm.register_builtin(ops::UNPACK, b_unpack);
    vm.register_builtin(ops::BINOP, b_binop);
    vm.register_builtin(ops::UNARY, b_unary);
    vm.register_builtin(ops::GETGLOBAL, b_getglobal);
    vm.register_builtin(ops::GETSELF, b_getself);
    vm.register_builtin(ops::ASSERT_FAIL, b_assert_fail);
    vm.register_builtin(ops::TRY, b_try);
    vm.register_builtin(ops::DBG_LINE, b_dbg_line);
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn pop_n(vm: &mut VM, n: usize) -> Vec<Value> {
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(vm.pop());
    }
    v.reverse();
    v
}

/// Read a compiler-internal name string (native `Value::Str` or heap `str`).
fn sval(v: &Value) -> String {
    if let Value::Str(s) = v {
        return (**s).clone();
    }
    with_host(|h| h.as_str(v)).unwrap_or_default()
}

fn abort(vm: &mut VM, e: String) -> Value {
    with_host(|h| h.error = Some(e));
    vm.ip = vm.chunk.ops.len();
    Value::Undef
}

/// Halt the chunk if a call left an error or non-local signal pending.
fn finish(vm: &mut VM, r: Result<Value, String>) -> Value {
    match r {
        Ok(v) => {
            if with_host(|h| h.error.is_some() || h.signal.is_some()) {
                vm.ip = vm.chunk.ops.len();
            }
            v
        }
        Err(e) => abort(vm, e),
    }
}

/// Extract `(name, value)` keyword pairs from a kwargs dict value.
fn kw_pairs(d: &Value) -> Vec<(String, Value)> {
    with_host(|h| match h.get(d) {
        Some(PyObj::Dict(m)) => m
            .values()
            .filter_map(|(k, v)| h.as_str(k).map(|s| (s, v.clone())))
            .collect(),
        _ => Vec::new(),
    })
}

// ── name / attribute / item handlers ─────────────────────────────────────────

fn b_getlocal(vm: &mut VM, _: u8) -> Value {
    let name = sval(&vm.pop());
    if let Some(v) = with_host(|h| h.read_name(&name)) {
        return v;
    }
    if is_known_builtin(&name) {
        return with_host(|h| h.alloc(PyObj::Builtin(name.clone())));
    }
    abort(vm, host::name_error(&name))
}

fn b_getglobal(vm: &mut VM, _: u8) -> Value {
    let name = sval(&vm.pop());
    if let Some(v) = with_host(|h| h.read_global(&name)) {
        return v;
    }
    if is_known_builtin(&name) {
        return with_host(|h| h.alloc(PyObj::Builtin(name.clone())));
    }
    abort(vm, host::name_error(&name))
}

fn b_setlocal(vm: &mut VM, _: u8) -> Value {
    let val = vm.pop();
    let name = sval(&vm.pop());
    with_host(|h| h.set_name(&name, val.clone()));
    val
}

fn b_setglobal(vm: &mut VM, _: u8) -> Value {
    let val = vm.pop();
    let name = sval(&vm.pop());
    with_host(|h| h.set_global(&name, val.clone()));
    val
}

fn b_declare_global(vm: &mut VM, _: u8) -> Value {
    let name = sval(&vm.pop());
    with_host(|h| h.declare_global(&name));
    Value::Undef
}

fn b_delname(vm: &mut VM, _: u8) -> Value {
    let name = sval(&vm.pop());
    match with_host(|h| h.del_name(&name)) {
        Ok(()) => Value::Undef,
        Err(e) => abort(vm, e),
    }
}

fn b_getself(vm: &mut VM, _: u8) -> Value {
    let _ = vm;
    with_host(|h| h.current_self().unwrap_or(Value::Undef))
}

fn b_getattr(vm: &mut VM, _: u8) -> Value {
    let name = sval(&vm.pop());
    let recv = vm.pop();
    let r = with_host(|h| h.get_attr(&recv, &name));
    finish(vm, r)
}

fn b_setattr(vm: &mut VM, _: u8) -> Value {
    let val = vm.pop();
    let name = sval(&vm.pop());
    let recv = vm.pop();
    match with_host(|h| h.set_attr(&recv, &name, val)) {
        Ok(()) => Value::Undef,
        Err(e) => abort(vm, e),
    }
}

fn b_delattr(vm: &mut VM, _: u8) -> Value {
    let name = sval(&vm.pop());
    let recv = vm.pop();
    match with_host(|h| h.del_attr(&recv, &name)) {
        Ok(()) => Value::Undef,
        Err(e) => abort(vm, e),
    }
}

fn b_getitem(vm: &mut VM, _: u8) -> Value {
    let idx = vm.pop();
    let recv = vm.pop();
    // __getitem__ on instances.
    if with_host(|h| matches!(h.get(&recv), Some(PyObj::Instance(_)))) {
        let r = host::call_method(&recv, "__getitem__", vec![idx], vec![]);
        return finish(vm, r);
    }
    let r = with_host(|h| h.get_item(&recv, &idx));
    finish(vm, r)
}

fn b_setitem(vm: &mut VM, _: u8) -> Value {
    let val = vm.pop();
    let idx = vm.pop();
    let recv = vm.pop();
    if with_host(|h| matches!(h.get(&recv), Some(PyObj::Instance(_)))) {
        let r = host::call_method(&recv, "__setitem__", vec![idx, val.clone()], vec![]);
        return finish(vm, r);
    }
    match with_host(|h| h.set_item(&recv, &idx, val.clone())) {
        Ok(()) => val,
        Err(e) => abort(vm, e),
    }
}

fn b_delitem(vm: &mut VM, _: u8) -> Value {
    let idx = vm.pop();
    let recv = vm.pop();
    match with_host(|h| h.del_item(&recv, &idx)) {
        Ok(()) => Value::Undef,
        Err(e) => abort(vm, e),
    }
}

// ── constructors ─────────────────────────────────────────────────────────────

fn b_mkstr(vm: &mut VM, argc: u8) -> Value {
    let parts = pop_n(vm, argc as usize);
    let mut s = String::new();
    with_host(|h| {
        for p in &parts {
            s.push_str(&h.str_of(p));
        }
    });
    with_host(|h| h.new_str(s))
}

fn b_mklist(vm: &mut VM, argc: u8) -> Value {
    let items = pop_n(vm, argc as usize);
    with_host(|h| h.new_list(items))
}

fn b_mktuple(vm: &mut VM, argc: u8) -> Value {
    let items = pop_n(vm, argc as usize);
    with_host(|h| h.new_tuple(items))
}

fn b_mkset(vm: &mut VM, argc: u8) -> Value {
    let items = pop_n(vm, argc as usize);
    let mut set: IndexMap<PKey, Value> = IndexMap::new();
    for it in items {
        match with_host(|h| h.to_key(&it)) {
            Ok(k) => {
                set.insert(k, it);
            }
            Err(e) => return abort(vm, e),
        }
    }
    with_host(|h| h.new_set(set))
}

fn b_mkdict(vm: &mut VM, argc: u8) -> Value {
    let flat = pop_n(vm, argc as usize);
    let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
    let mut i = 0;
    while i + 1 < flat.len() {
        let k = flat[i].clone();
        let v = flat[i + 1].clone();
        match with_host(|h| h.to_key(&k)) {
            Ok(key) => {
                d.insert(key, (k, v));
            }
            Err(e) => return abort(vm, e),
        }
        i += 2;
    }
    with_host(|h| h.new_dict(d))
}

fn b_mkslice(vm: &mut VM, _: u8) -> Value {
    let step = vm.pop();
    let hi = vm.pop();
    let lo = vm.pop();
    with_host(|h| h.alloc(PyObj::Slice { lo, hi, step }))
}

// ── calls ────────────────────────────────────────────────────────────────────

fn b_call(vm: &mut VM, argc: u8) -> Value {
    let mut args = pop_n(vm, argc as usize);
    let name = sval(&args.remove(0));
    let r = host::call_named(&name, args, vec![]);
    finish(vm, r)
}

fn b_call_kw(vm: &mut VM, argc: u8) -> Value {
    let mut args = pop_n(vm, argc as usize);
    let kwd = args.pop().unwrap();
    let name = sval(&args.remove(0));
    let kwargs = kw_pairs(&kwd);
    let r = host::call_named(&name, args, kwargs);
    finish(vm, r)
}

fn b_call_method(vm: &mut VM, argc: u8) -> Value {
    let mut args = pop_n(vm, argc as usize);
    let recv = args.remove(0);
    let name = sval(&args.remove(0));
    let r = host::call_method(&recv, &name, args, vec![]);
    finish(vm, r)
}

fn b_call_method_kw(vm: &mut VM, argc: u8) -> Value {
    let mut args = pop_n(vm, argc as usize);
    let kwd = args.pop().unwrap();
    let recv = args.remove(0);
    let name = sval(&args.remove(0));
    let kwargs = kw_pairs(&kwd);
    let r = host::call_method(&recv, &name, args, kwargs);
    finish(vm, r)
}

fn b_call_value(vm: &mut VM, argc: u8) -> Value {
    let mut args = pop_n(vm, argc as usize);
    let callable = args.remove(0);
    let r = host::invoke(&callable, args, vec![]);
    finish(vm, r)
}

fn b_call_value_kw(vm: &mut VM, argc: u8) -> Value {
    let mut args = pop_n(vm, argc as usize);
    let kwd = args.pop().unwrap();
    let callable = args.remove(0);
    let kwargs = kw_pairs(&kwd);
    let r = host::invoke(&callable, args, kwargs);
    finish(vm, r)
}

// ── truthiness / str / format ────────────────────────────────────────────────

fn b_truthy(vm: &mut VM, _: u8) -> Value {
    let v = vm.pop();
    if with_host(
        |h| matches!(h.get(&v), Some(PyObj::Instance(i)) if instance_has(h, i, "__bool__") || instance_has(h, i, "__len__")),
    ) {
        // __bool__ preferred, else __len__.
        let has_bool = with_host(|h| match h.get(&v) {
            Some(PyObj::Instance(i)) => instance_has(h, i, "__bool__"),
            _ => false,
        });
        let r = if has_bool {
            host::call_method(&v, "__bool__", vec![], vec![])
        } else {
            host::call_method(&v, "__len__", vec![], vec![])
        };
        return match r {
            Ok(x) => Value::Bool(with_host(|h| h.truthy(&x))),
            Err(e) => abort(vm, e),
        };
    }
    Value::Bool(with_host(|h| h.truthy(&v)))
}

fn instance_has(h: &host::PyHost, i: &Instance, name: &str) -> bool {
    h.class_lookup(&i.class, name).is_some()
}

fn b_tostr(vm: &mut VM, _: u8) -> Value {
    let v = vm.pop();
    stringify(vm, &v, false)
}

/// str()/repr() with dunder dispatch for instances.
fn stringify(vm: &mut VM, v: &Value, repr: bool) -> Value {
    let is_inst = with_host(|h| matches!(h.get(v), Some(PyObj::Instance(_))));
    if is_inst {
        let method = if repr { "__repr__" } else { "__str__" };
        let has = with_host(|h| match h.get(v) {
            Some(PyObj::Instance(i)) => instance_has(h, i, method),
            _ => false,
        });
        let has_fallback = !repr
            && with_host(|h| match h.get(v) {
                Some(PyObj::Instance(i)) => instance_has(h, i, "__repr__"),
                _ => false,
            });
        if has {
            let r = host::call_method(v, method, vec![], vec![]);
            return finish(vm, r);
        } else if has_fallback {
            let r = host::call_method(v, "__repr__", vec![], vec![]);
            return finish(vm, r);
        }
    }
    let s = with_host(|h| if repr { h.repr_of(v) } else { h.str_of(v) });
    with_host(|h| h.new_str(s))
}

fn b_format(vm: &mut VM, _: u8) -> Value {
    let spec = sval(&vm.pop());
    let conv = match vm.pop() {
        Value::Int(n) => n,
        _ => 0,
    };
    let v = vm.pop();
    // Apply conversion.
    let s = match conv {
        2 => with_host(|h| h.repr_of(&v)), // !r
        _ => {
            // !s / !a / none -> str(), with instance dunder dispatch.
            let sv = stringify(vm, &v, false);
            with_host(|h| h.str_of(&sv))
        }
    };
    let out = crate::builtins::apply_format_spec(&s, &v, &spec);
    with_host(|h| h.new_str(out))
}

// ── functions / classes ──────────────────────────────────────────────────────

fn b_mkfunc(vm: &mut VM, argc: u8) -> Value {
    let mut args = pop_n(vm, argc as usize);
    let def_id = match args.pop() {
        Some(Value::Int(n)) => n as usize,
        _ => return abort(vm, "internal: MKFUNC without func id".into()),
    };
    let defaults = args; // remaining are positional defaults, in order
    let env = with_host(|h| h.current_env_capture());
    with_host(|h| {
        h.alloc(PyObj::Func(host::FuncVal {
            def_id,
            env: Some(env),
            defaults,
            bound: None,
            owner: None,
        }))
    })
}

fn b_build_class(vm: &mut VM, _: u8) -> Value {
    let body_func = vm.pop();
    let name = sval(&vm.pop());
    let bases_val = vm.pop();
    let bases: Vec<String> = with_host(|h| match h.get(&bases_val) {
        Some(PyObj::List(l)) => l.iter().filter_map(|b| callable_name(h, b)).collect(),
        _ => Vec::new(),
    });
    let r = host::build_class(&name, bases, &body_func);
    finish(vm, r)
}

// ── iteration ────────────────────────────────────────────────────────────────

fn b_getiter(vm: &mut VM, _: u8) -> Value {
    let v = vm.pop();
    // __iter__ on instances -> materialize via repeated __next__.
    if with_host(|h| matches!(h.get(&v), Some(PyObj::Instance(_)))) {
        let r = iter_instance(&v);
        return finish(vm, r);
    }
    let r = with_host(|h| h.make_iter(&v));
    finish(vm, r)
}

/// Drive a user iterable's `__iter__`/`__next__` into a concrete seq iterator.
fn iter_instance(v: &Value) -> Result<Value, String> {
    let it = host::call_method(v, "__iter__", vec![], vec![])?;
    // If __iter__ returned a builtin iterator, use it directly.
    if with_host(|h| matches!(h.get(&it), Some(PyObj::Iter(_)))) {
        return Ok(it);
    }
    // Otherwise materialize by calling __next__ until StopIteration.
    let mut items = Vec::new();
    loop {
        match host::call_method(&it, "__next__", vec![], vec![]) {
            Ok(x) => items.push(x),
            Err(e) if e.contains("StopIteration") => break,
            Err(e) => return Err(e),
        }
        if items.len() > 10_000_000 {
            break;
        }
    }
    Ok(with_host(|h| {
        h.alloc(PyObj::Iter(IterState::Seq { items, idx: 0 }))
    }))
}

fn b_foriter(vm: &mut VM, _: u8) -> Value {
    let it = match vm.stack.last() {
        Some(v) => v.clone(),
        None => return abort(vm, "internal: FORITER with empty stack".into()),
    };
    match with_host(|h| h.iter_next(&it)) {
        Ok(Some(v)) => {
            vm.push(v);
            Value::Bool(true)
        }
        Ok(None) => Value::Bool(false),
        Err(e) => abort(vm, e),
    }
}

// ── membership / identity ────────────────────────────────────────────────────

fn b_contains(vm: &mut VM, _: u8) -> Value {
    let container = vm.pop();
    let item = vm.pop();
    let r = with_host(|h| h.contains(&item, &container));
    match r {
        Ok(b) => Value::Bool(b),
        Err(e) => abort(vm, e),
    }
}

fn b_is(vm: &mut VM, _: u8) -> Value {
    let b = vm.pop();
    let a = vm.pop();
    let same = match (&a, &b) {
        (Value::Obj(x), Value::Obj(y)) => x == y,
        (Value::Undef, Value::Undef) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        _ => false,
    };
    Value::Bool(same)
}

// ── control ──────────────────────────────────────────────────────────────────

fn b_sig_return(vm: &mut VM, _: u8) -> Value {
    let v = vm.pop();
    with_host(|h| h.signal = Some(host::Signal::Return(v.clone())));
    vm.ip = vm.chunk.ops.len();
    v
}

fn b_noop(_vm: &mut VM, _: u8) -> Value {
    Value::Undef
}

fn b_raise(vm: &mut VM, _: u8) -> Value {
    let exc = vm.pop();
    match host::raise_value(&exc) {
        Ok(msg) => abort(vm, msg),
        Err(e) => abort(vm, e),
    }
}

fn b_reraise(vm: &mut VM, _: u8) -> Value {
    let msg = with_host(|h| {
        let e = h.exc.clone();
        match e {
            Some(v) => Some(h.str_of(&v)),
            None => h.error.clone(),
        }
    });
    match msg {
        Some(m) => abort(vm, exc_to_error(&m)),
        None => abort(vm, "RuntimeError: No active exception to re-raise".into()),
    }
}

fn exc_to_error(m: &str) -> String {
    m.to_string()
}

fn b_assert_fail(vm: &mut VM, _: u8) -> Value {
    let msg = vm.pop();
    let s = with_host(|h| {
        if matches!(msg, Value::Undef) {
            "AssertionError".to_string()
        } else {
            format!("AssertionError: {}", h.str_of(&msg))
        }
    });
    // Record the exception object too, for except-binding.
    with_host(|h| {
        let m = if let Value::Undef = msg {
            Value::Undef
        } else {
            msg.clone()
        };
        let args = if matches!(m, Value::Undef) {
            vec![]
        } else {
            vec![m]
        };
        let e = h.alloc(PyObj::Exception {
            class: "AssertionError".into(),
            args,
        });
        h.exc = Some(e);
    });
    abort(vm, s)
}

fn b_dbg_line(vm: &mut VM, _: u8) -> Value {
    let _line = vm.pop();
    Value::Undef
}

// ── binary / unary operators ─────────────────────────────────────────────────

/// Whether `v` is a user instance whose class defines method `name` — the guard
/// for operator-overloading dunder dispatch.
fn is_instance_with(h: &host::PyHost, v: &Value, name: &str) -> bool {
    matches!(h.get(v), Some(PyObj::Instance(i)) if instance_has(h, i, name))
}

/// Python operator overloading: if `a` defines the forward dunder `lname`
/// (`__add__`), call `a.lname(b)`; else if `b` defines the reflected dunder
/// `rname` (`__radd__`), call `b.rname(a)`. Returns `None` to fall through to the
/// host's native handling when neither operand is an overloading instance.
fn try_binop_dunder(
    a: &Value,
    b: &Value,
    lname: &str,
    rname: &str,
) -> Option<Result<Value, String>> {
    if with_host(|h| is_instance_with(h, a, lname)) {
        return Some(host::call_method(a, lname, vec![b.clone()], vec![]));
    }
    if with_host(|h| is_instance_with(h, b, rname)) {
        return Some(host::call_method(b, rname, vec![a.clone()], vec![]));
    }
    None
}

/// (forward, reflected) dunder names for a non-native binop tag (`host::binop`).
fn binop_tag_dunders(tag: i64) -> Option<(&'static str, &'static str)> {
    use host::binop::*;
    Some(match tag {
        DIV => ("__truediv__", "__rtruediv__"),
        FLOORDIV => ("__floordiv__", "__rfloordiv__"),
        MOD => ("__mod__", "__rmod__"),
        POW => ("__pow__", "__rpow__"),
        MATMUL => ("__matmul__", "__rmatmul__"),
        BITAND => ("__and__", "__rand__"),
        BITOR => ("__or__", "__ror__"),
        BITXOR => ("__xor__", "__rxor__"),
        SHL => ("__lshift__", "__rlshift__"),
        SHR => ("__rshift__", "__rrshift__"),
        _ => return None,
    })
}

/// (forward, reflected) dunder names for a native `NumOp` that fell to the hook.
fn numop_dunders(op: NumOp) -> Option<(&'static str, &'static str)> {
    use NumOp::*;
    Some(match op {
        Add => ("__add__", "__radd__"),
        Sub => ("__sub__", "__rsub__"),
        Mul => ("__mul__", "__rmul__"),
        Div => ("__truediv__", "__rtruediv__"),
        Mod => ("__mod__", "__rmod__"),
        Pow => ("__pow__", "__rpow__"),
        Eq => ("__eq__", "__eq__"),
        Ne => ("__ne__", "__ne__"),
        Lt => ("__lt__", "__gt__"),
        Le => ("__le__", "__ge__"),
        Gt => ("__gt__", "__lt__"),
        Ge => ("__ge__", "__le__"),
        Neg => return None,
    })
}

fn b_binop(vm: &mut VM, _: u8) -> Value {
    let b = vm.pop();
    let a = vm.pop();
    let tag = match vm.pop() {
        Value::Int(n) => n,
        _ => return abort(vm, "internal: BINOP tag".into()),
    };
    // Instance operator overloading (`a / b`, `a % b`, `a & b`, …) via dunders,
    // then core types handled by the host.
    if let Some((l, r)) = binop_tag_dunders(tag) {
        if let Some(res) = try_binop_dunder(&a, &b, l, r) {
            return finish(vm, res);
        }
    }
    let r = with_host(|h| h.binop(tag, &a, &b));
    finish(vm, r)
}

fn b_unary(vm: &mut VM, _: u8) -> Value {
    let v = vm.pop();
    let tag = match vm.pop() {
        Value::Int(n) => n,
        _ => return abort(vm, "internal: UNARY tag".into()),
    };
    let r = with_host(|h| h.unary(tag, &v));
    finish(vm, r)
}

// ── import ───────────────────────────────────────────────────────────────────

fn b_import(vm: &mut VM, _: u8) -> Value {
    let name = sval(&vm.pop());
    let r = host::import_module(&name);
    finish(vm, r)
}

fn b_import_from(vm: &mut VM, _: u8) -> Value {
    let name = sval(&vm.pop());
    let module = vm.pop();
    let r = with_host(|h| h.get_attr(&module, &name));
    finish(vm, r)
}

// ── unpack ───────────────────────────────────────────────────────────────────

fn b_unpack(vm: &mut VM, _: u8) -> Value {
    let star_idx = match vm.pop() {
        Value::Int(n) => n,
        _ => -1,
    };
    let count = match vm.pop() {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let iterable = vm.pop();
    let items = match with_host(|h| h.iter_items(&iterable)) {
        Ok(v) => v,
        Err(e) => return abort(vm, e),
    };
    // Build the `count` target values in target order, then push them so that
    // target[0] ends on top: manually push target[1..] reversed and RETURN
    // target[0] (the VM pushes the return value last, i.e. on top).
    let ordered: Vec<Value> = if star_idx < 0 {
        if items.len() != count {
            let msg = if items.len() < count {
                format!(
                    "ValueError: not enough values to unpack (expected {count}, got {})",
                    items.len()
                )
            } else {
                format!("ValueError: too many values to unpack (expected {count})")
            };
            return abort(vm, msg);
        }
        items
    } else {
        let si = star_idx as usize;
        let before = si;
        let after = count - si - 1;
        if items.len() < before + after {
            return abort(
                vm,
                format!(
                    "ValueError: not enough values to unpack (expected at least {})",
                    before + after
                ),
            );
        }
        let mid = &items[before..items.len() - after];
        let mid_list = with_host(|h| h.new_list(mid.to_vec()));
        let mut ordered: Vec<Value> = Vec::with_capacity(count);
        ordered.extend_from_slice(&items[..before]);
        ordered.push(mid_list);
        ordered.extend_from_slice(&items[items.len() - after..]);
        ordered
    };
    if ordered.is_empty() {
        return Value::Undef;
    }
    // Push target[count-1..=1] so target[1] is on top of the manual pushes;
    // return target[0] so it lands above them.
    for it in ordered[1..].iter().rev().cloned() {
        vm.push(it);
    }
    ordered[0].clone()
}

// ── try/except/finally ───────────────────────────────────────────────────────

fn b_try(vm: &mut VM, _: u8) -> Value {
    let id = match vm.pop() {
        Value::Int(n) => n as usize,
        _ => return abort(vm, "internal: TRY id".into()),
    };
    let td = match with_host(|h| h.try_def(id)) {
        Some(t) => t,
        None => return abort(vm, "internal: unknown try id".into()),
    };
    let mut pending: Option<String> = None;

    let body_res = host::run_chunk_on(td.body.clone());
    let signal_after_body = with_host(|h| h.signal.is_some());
    match body_res {
        Ok(_) => {
            if !signal_after_body {
                if let Some(els) = &td.orelse {
                    if let Err(e) = host::run_chunk_on(els.clone()) {
                        pending = Some(e);
                    }
                }
            }
        }
        Err(e) => {
            let exc = with_host(|h| h.exc.clone().unwrap_or_else(|| synth_exc(h, &e)));
            let mut handled = false;
            for (type_chunk, bind, hbody) in &td.handlers {
                let matches = match type_chunk {
                    None => true,
                    Some(tc) => {
                        let tv = host::run_chunk_on(tc.clone()).unwrap_or(Value::Undef);
                        with_host(|h| exc_matches(h, &exc, &tv))
                    }
                };
                if matches {
                    if let Some(name) = bind {
                        with_host(|h| h.set_name(name, exc.clone()));
                    }
                    with_host(|h| {
                        h.error = None;
                        h.exc = None;
                    });
                    if let Err(e2) = host::run_chunk_on(hbody.clone()) {
                        pending = Some(e2);
                    }
                    if let Some(name) = bind {
                        with_host(|h| {
                            let _ = h.del_name(name);
                        });
                    }
                    handled = true;
                    break;
                }
            }
            if !handled {
                pending = Some(e);
            }
        }
    }

    // finally always runs; a finally error/return supersedes.
    if let Some(fin) = &td.finalbody {
        let sig_before = with_host(|h| h.signal.take());
        match host::run_chunk_on(fin.clone()) {
            Ok(_) => {
                if with_host(|h| h.signal.is_none()) {
                    with_host(|h| h.signal = sig_before);
                }
            }
            Err(e) => {
                pending = Some(e);
            }
        }
    }

    if let Some(e) = pending {
        return abort(vm, e);
    }
    // Propagate a pending return signal to the enclosing chunk.
    if with_host(|h| h.signal.is_some()) {
        vm.ip = vm.chunk.ops.len();
    }
    Value::Undef
}

fn synth_exc(h: &mut host::PyHost, err: &str) -> Value {
    let (class, msg) = match err.split_once(": ") {
        Some((c, m)) => (c.to_string(), m.to_string()),
        None => (err.to_string(), String::new()),
    };
    let args = if msg.is_empty() {
        vec![]
    } else {
        let s = h.new_str(msg);
        vec![s]
    };
    h.alloc(PyObj::Exception { class, args })
}

/// Whether the raised exception matches the handler type value (a class,
/// exception-class name, or tuple of them).
fn exc_matches(h: &host::PyHost, exc: &Value, typ: &Value) -> bool {
    let exc_class = match h.get(exc) {
        Some(PyObj::Exception { class, .. }) => class.clone(),
        Some(PyObj::Instance(i)) => i.class.clone(),
        _ => h.type_name(exc),
    };
    // Tuple of types.
    if let Some(PyObj::Tuple(ts)) = h.get(typ) {
        return ts.iter().any(|t| exc_matches(h, exc, t));
    }
    let want = match callable_name(h, typ) {
        Some(n) => n,
        None => return false,
    };
    exception_isa(&exc_class, &want, h)
}

/// The name of a callable value (builtin or class).
fn callable_name(h: &host::PyHost, v: &Value) -> Option<String> {
    match h.get(v) {
        Some(PyObj::Builtin(n)) => Some(n.clone()),
        Some(PyObj::Class(n)) => Some(n.clone()),
        _ => None,
    }
}

// ── the strict numeric hook ──────────────────────────────────────────────────

/// Python arithmetic/comparison for operands the VM can't handle natively. User
/// instances defining an operator dunder (`__add__`, `__eq__`, `__lt__`, …) are
/// dispatched first; everything else falls to the host's native numeric logic.
pub fn numeric_hook(op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
    if let Some((l, r)) = numop_dunders(op) {
        if let Some(res) = try_binop_dunder(a, b, l, r) {
            return res;
        }
    }
    with_host(|h| h.arith(op, a, b))
}

// ── builtin predicates ───────────────────────────────────────────────────────

pub fn is_builtin_function(name: &str) -> bool {
    BUILTIN_FUNCS.contains(&name) || name.starts_with("math.")
}

fn is_builtin_type(name: &str) -> bool {
    matches!(
        name,
        "int"
            | "float"
            | "str"
            | "bool"
            | "list"
            | "tuple"
            | "dict"
            | "set"
            | "frozenset"
            | "bytes"
            | "complex"
            | "object"
            | "type"
            | "range"
    )
}

pub fn is_exception_class(name: &str) -> bool {
    EXC_PARENTS.iter().any(|(c, _)| *c == name) || name == "BaseException"
}

pub fn is_known_builtin(name: &str) -> bool {
    is_builtin_function(name) || is_builtin_type(name) || is_exception_class(name)
}

const BUILTIN_FUNCS: &[&str] = &[
    "print",
    "len",
    "range",
    "abs",
    "min",
    "max",
    "sum",
    "sorted",
    "reversed",
    "enumerate",
    "zip",
    "map",
    "filter",
    "any",
    "all",
    "round",
    "divmod",
    "pow",
    "type",
    "isinstance",
    "issubclass",
    "hasattr",
    "getattr",
    "setattr",
    "delattr",
    "id",
    "hash",
    "ord",
    "chr",
    "hex",
    "oct",
    "bin",
    "repr",
    "ascii",
    "iter",
    "next",
    "input",
    "format",
    "vars",
    "dir",
    "callable",
];

// ── builtin functions ────────────────────────────────────────────────────────

/// str()/repr() with instance dunder dispatch (free-function form).
fn py_str(v: &Value) -> Result<String, String> {
    if with_host(|h| matches!(h.get(v), Some(PyObj::Instance(_)))) {
        let (has_str, has_repr) = with_host(|h| match h.get(v) {
            Some(PyObj::Instance(i)) => (
                h.class_lookup(&i.class, "__str__").is_some(),
                h.class_lookup(&i.class, "__repr__").is_some(),
            ),
            _ => (false, false),
        });
        if has_str {
            let r = host::call_method(v, "__str__", vec![], vec![])?;
            return Ok(with_host(|h| h.str_of(&r)));
        }
        if has_repr {
            let r = host::call_method(v, "__repr__", vec![], vec![])?;
            return Ok(with_host(|h| h.str_of(&r)));
        }
    }
    // `str(container)` == `repr(container)`; route through py_repr so instance
    // elements/keys/values dispatch their `__repr__`.
    if with_host(|h| {
        matches!(
            h.get(v),
            Some(PyObj::List(_))
                | Some(PyObj::Tuple(_))
                | Some(PyObj::Dict(_))
                | Some(PyObj::Set(_))
        )
    }) {
        return py_repr(v);
    }
    Ok(with_host(|h| h.str_of(v)))
}

fn py_repr(v: &Value) -> Result<String, String> {
    if with_host(|h| matches!(h.get(v), Some(PyObj::Instance(_)))) {
        let has_repr = with_host(|h| match h.get(v) {
            Some(PyObj::Instance(i)) => h.class_lookup(&i.class, "__repr__").is_some(),
            _ => false,
        });
        if has_repr {
            let r = host::call_method(v, "__repr__", vec![], vec![])?;
            return Ok(with_host(|h| h.str_of(&r)));
        }
    }
    // Containers recurse through this layer so instance elements/keys/values
    // dispatch their own `__repr__` (the host's `repr_of` is `&self` and can't
    // call back into a method).
    enum Cont {
        List(Vec<Value>),
        Tuple(Vec<Value>),
        Set(Vec<Value>),
        Dict(Vec<(Value, Value)>),
    }
    let cont = with_host(|h| match h.get(v) {
        Some(PyObj::List(l)) => Some(Cont::List(l.clone())),
        Some(PyObj::Tuple(l)) => Some(Cont::Tuple(l.clone())),
        Some(PyObj::Set(s)) => Some(Cont::Set(s.values().cloned().collect())),
        Some(PyObj::Dict(d)) => Some(Cont::Dict(d.values().cloned().collect())),
        _ => None,
    });
    let reprs =
        |elems: &[Value]| -> Result<Vec<String>, String> { elems.iter().map(py_repr).collect() };
    if let Some(cont) = cont {
        return Ok(match cont {
            Cont::List(e) => format!("[{}]", reprs(&e)?.join(", ")),
            Cont::Tuple(e) => {
                let p = reprs(&e)?;
                if p.len() == 1 {
                    format!("({},)", p[0])
                } else {
                    format!("({})", p.join(", "))
                }
            }
            Cont::Set(e) if e.is_empty() => "set()".into(),
            Cont::Set(e) => format!("{{{}}}", reprs(&e)?.join(", ")),
            Cont::Dict(pairs) => {
                let mut p = Vec::with_capacity(pairs.len());
                for (k, val) in &pairs {
                    p.push(format!("{}: {}", py_repr(k)?, py_repr(val)?));
                }
                format!("{{{}}}", p.join(", "))
            }
        });
    }
    Ok(with_host(|h| h.repr_of(v)))
}

fn kw_get(kwargs: &[(String, Value)], name: &str) -> Option<Value> {
    kwargs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

/// Dispatch a Kernel builtin function by name.
pub fn call_builtin_function(
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    // math.* module functions.
    if let Some(m) = name.strip_prefix("math.") {
        return call_math(m, &args);
    }
    // Exception constructors.
    if is_exception_class(name) {
        return Ok(with_host(|h| {
            h.alloc(PyObj::Exception {
                class: name.to_string(),
                args,
            })
        }));
    }
    match name {
        "print" => {
            let sep = kw_get(&kwargs, "sep")
                .map(|v| with_host(|h| h.str_of(&v)))
                .unwrap_or_else(|| " ".into());
            let end = kw_get(&kwargs, "end")
                .map(|v| with_host(|h| h.str_of(&v)))
                .unwrap_or_else(|| "\n".into());
            let mut parts = Vec::new();
            for a in &args {
                parts.push(py_str(a)?);
            }
            use std::io::Write;
            let out = format!("{}{}", parts.join(&sep), end);
            let _ = std::io::stdout().write_all(out.as_bytes());
            let _ = std::io::stdout().flush();
            Ok(Value::Undef)
        }
        "len" => {
            let v = arg0(&args)?;
            let n = py_len(&v)?;
            Ok(Value::Int(n as i64))
        }
        "range" => make_range(&args),
        "abs" => {
            let v = arg0(&args)?;
            with_host(|h| match &v {
                Value::Int(n) => Ok(Value::Int(n.abs())),
                Value::Float(f) => Ok(Value::Float(f.abs())),
                Value::Bool(b) => Ok(Value::Int(*b as i64)),
                _ => Err(host::type_error(&format!(
                    "bad operand type for abs(): '{}'",
                    h.type_name(&v)
                ))),
            })
        }
        "min" => reduce_minmax(&args, &kwargs, false),
        "max" => reduce_minmax(&args, &kwargs, true),
        "sum" => {
            let items = with_host(|h| h.iter_items(&arg0(&args)?))?;
            let mut acc = args.get(1).cloned().unwrap_or(Value::Int(0));
            for it in items {
                acc = with_host(|h| h.arith(NumOp::Add, &acc, &it))?;
            }
            Ok(acc)
        }
        "sorted" => py_sorted(&args, &kwargs),
        "reversed" => {
            let mut items = with_host(|h| h.iter_items(&arg0(&args)?))?;
            items.reverse();
            Ok(with_host(|h| h.new_list(items)))
        }
        "enumerate" => {
            let items = with_host(|h| h.iter_items(&arg0(&args)?))?;
            let start = args
                .get(1)
                .and_then(|v| with_host(|h| h.as_int(v)))
                .unwrap_or(0);
            let out: Vec<Value> = with_host(|h| {
                items
                    .into_iter()
                    .enumerate()
                    .map(|(i, x)| h.new_tuple(vec![Value::Int(start + i as i64), x]))
                    .collect()
            });
            Ok(with_host(|h| h.new_list(out)))
        }
        "zip" => {
            let mut seqs = Vec::new();
            for a in &args {
                seqs.push(with_host(|h| h.iter_items(a))?);
            }
            let n = seqs.iter().map(|s| s.len()).min().unwrap_or(0);
            let mut out = Vec::new();
            for i in 0..n {
                let tup: Vec<Value> = seqs.iter().map(|s| s[i].clone()).collect();
                out.push(with_host(|h| h.new_tuple(tup)));
            }
            Ok(with_host(|h| h.new_list(out)))
        }
        "map" => {
            let f = arg0(&args)?;
            let mut seqs = Vec::new();
            for a in &args[1..] {
                seqs.push(with_host(|h| h.iter_items(a))?);
            }
            let n = seqs.iter().map(|s| s.len()).min().unwrap_or(0);
            let mut out = Vec::new();
            for i in 0..n {
                let call_args: Vec<Value> = seqs.iter().map(|s| s[i].clone()).collect();
                out.push(host::invoke(&f, call_args, vec![])?);
            }
            Ok(with_host(|h| h.new_list(out)))
        }
        "filter" => {
            let f = arg0(&args)?;
            let items = with_host(|h| h.iter_items(&args[1]))?;
            let mut out = Vec::new();
            for it in items {
                let keep = if matches!(f, Value::Undef) {
                    with_host(|h| h.truthy(&it))
                } else {
                    let r = host::invoke(&f, vec![it.clone()], vec![])?;
                    with_host(|h| h.truthy(&r))
                };
                if keep {
                    out.push(it);
                }
            }
            Ok(with_host(|h| h.new_list(out)))
        }
        "any" => {
            let items = with_host(|h| h.iter_items(&arg0(&args)?))?;
            Ok(Value::Bool(
                items.iter().any(|x| with_host(|h| h.truthy(x))),
            ))
        }
        "all" => {
            let items = with_host(|h| h.iter_items(&arg0(&args)?))?;
            Ok(Value::Bool(
                items.iter().all(|x| with_host(|h| h.truthy(x))),
            ))
        }
        "round" => {
            let v = arg0(&args)?;
            let nd = args.get(1).and_then(|v| with_host(|h| h.as_int(v)));
            with_host(|_h| match &v {
                Value::Int(n) => Ok(Value::Int(*n)),
                Value::Float(f) => match nd {
                    Some(d) => {
                        let m = 10f64.powi(d as i32);
                        Ok(Value::Float((f * m).round() / m))
                    }
                    None => Ok(Value::Int(f.round() as i64)),
                },
                _ => Err(host::type_error("round() argument must be a number")),
            })
        }
        "divmod" => {
            let a = arg0(&args)?;
            let b = args.get(1).cloned().unwrap_or(Value::Int(0));
            let q = with_host(|h| h.binop(host::binop::FLOORDIV, &a, &b))?;
            let r = with_host(|h| h.binop(host::binop::MOD, &a, &b))?;
            Ok(with_host(|h| h.new_tuple(vec![q, r])))
        }
        "pow" => {
            let a = arg0(&args)?;
            let b = args.get(1).cloned().unwrap_or(Value::Int(1));
            with_host(|h| h.binop(host::binop::POW, &a, &b))
        }
        "type" => {
            let v = arg0(&args)?;
            let tn = with_host(|h| h.type_name(&v));
            Ok(with_host(|h| h.alloc(PyObj::Builtin(tn))))
        }
        "isinstance" => {
            let v = arg0(&args)?;
            let cls = args.get(1).cloned().unwrap_or(Value::Undef);
            Ok(Value::Bool(with_host(|h| isinstance(h, &v, &cls))))
        }
        "issubclass" => {
            let a0 = arg0(&args)?;
            let a1 = args.get(1).cloned().unwrap_or(Value::Undef);
            let a = with_host(|h| callable_name(h, &a0)).unwrap_or_default();
            let b = with_host(|h| callable_name(h, &a1)).unwrap_or_default();
            Ok(Value::Bool(with_host(|h| type_isa(h, &a, &b))))
        }
        "hasattr" => {
            let v = arg0(&args)?;
            let n = with_host(|h| h.str_of(&args.get(1).cloned().unwrap_or(Value::Undef)));
            Ok(Value::Bool(with_host(|h| h.get_attr(&v, &n)).is_ok()))
        }
        "getattr" => {
            let v = arg0(&args)?;
            let n = with_host(|h| h.str_of(&args.get(1).cloned().unwrap_or(Value::Undef)));
            match with_host(|h| h.get_attr(&v, &n)) {
                Ok(x) => Ok(x),
                Err(e) => match args.get(2) {
                    Some(d) => Ok(d.clone()),
                    None => Err(e),
                },
            }
        }
        "setattr" => {
            let v = arg0(&args)?;
            let n = with_host(|h| h.str_of(&args.get(1).cloned().unwrap_or(Value::Undef)));
            let val = args.get(2).cloned().unwrap_or(Value::Undef);
            with_host(|h| h.set_attr(&v, &n, val))?;
            Ok(Value::Undef)
        }
        "id" => {
            let v = arg0(&args)?;
            Ok(Value::Int(match v {
                Value::Obj(i) => i as i64,
                Value::Int(n) => n,
                _ => 0,
            }))
        }
        "hash" => {
            let v = arg0(&args)?;
            let k = with_host(|h| h.to_key(&v))?;
            Ok(Value::Int(hash_key(&k)))
        }
        "ord" => {
            let a0 = arg0(&args)?;
            let s = with_host(|h| h.as_str(&a0)).unwrap_or_default();
            match s.chars().next() {
                Some(c) => Ok(Value::Int(c as i64)),
                None => Err(host::type_error("ord() expected a character")),
            }
        }
        "chr" => {
            let a0 = arg0(&args)?;
            let n = with_host(|h| h.as_int(&a0)).unwrap_or(0);
            match char::from_u32(n as u32) {
                Some(c) => Ok(with_host(|h| h.new_str(c.to_string()))),
                None => Err("ValueError: chr() arg not in range".to_string()),
            }
        }
        "hex" => int_radix(&args, 16, "0x"),
        "oct" => int_radix(&args, 8, "0o"),
        "bin" => int_radix(&args, 2, "0b"),
        "repr" | "ascii" => {
            let v = arg0(&args)?;
            let s = py_repr(&v)?;
            Ok(with_host(|h| h.new_str(s)))
        }
        "format" => {
            let v = arg0(&args)?;
            let spec =
                with_host(|h| h.str_of(&args.get(1).cloned().unwrap_or_else(|| Value::str(""))));
            let s = py_str(&v)?;
            Ok(with_host(|h| {
                let out = apply_format_spec(&s, &v, &spec);
                h.new_str(out)
            }))
        }
        "iter" => {
            let v = arg0(&args)?;
            with_host(|h| h.make_iter(&v))
        }
        "next" => {
            let it = arg0(&args)?;
            match with_host(|h| h.iter_next(&it))? {
                Some(v) => Ok(v),
                None => match args.get(1) {
                    Some(d) => Ok(d.clone()),
                    None => Err("StopIteration".into()),
                },
            }
        }
        "input" => {
            if let Some(p) = args.first() {
                use std::io::Write;
                let s = py_str(p)?;
                let _ = std::io::stdout().write_all(s.as_bytes());
                let _ = std::io::stdout().flush();
            }
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            let line = line
                .trim_end_matches('\n')
                .trim_end_matches('\r')
                .to_string();
            Ok(with_host(|h| h.new_str(line)))
        }
        "callable" => {
            let v = arg0(&args)?;
            Ok(Value::Bool(with_host(|h| {
                matches!(
                    h.get(&v),
                    Some(PyObj::Func(_))
                        | Some(PyObj::Builtin(_))
                        | Some(PyObj::Class(_))
                        | Some(PyObj::BoundMethod { .. })
                )
            })))
        }
        "vars" | "dir" => Ok(with_host(|h| h.new_list(vec![]))),
        // Type constructors.
        "int" => construct_int(&args),
        "float" => construct_float(&args),
        "str" => {
            let v = args.first().cloned().unwrap_or_else(|| Value::str(""));
            let s = py_str(&v)?;
            Ok(with_host(|h| h.new_str(s)))
        }
        "bool" => {
            let v = args.first().cloned().unwrap_or(Value::Bool(false));
            Ok(Value::Bool(with_host(|h| h.truthy(&v))))
        }
        "list" => {
            let items = match args.first() {
                Some(v) => with_host(|h| h.iter_items(v))?,
                None => vec![],
            };
            Ok(with_host(|h| h.new_list(items)))
        }
        "tuple" => {
            let items = match args.first() {
                Some(v) => with_host(|h| h.iter_items(v))?,
                None => vec![],
            };
            Ok(with_host(|h| h.new_tuple(items)))
        }
        "set" | "frozenset" => {
            let items = match args.first() {
                Some(v) => with_host(|h| h.iter_items(v))?,
                None => vec![],
            };
            let mut s: IndexMap<PKey, Value> = IndexMap::new();
            for it in items {
                let k = with_host(|h| h.to_key(&it))?;
                s.insert(k, it);
            }
            Ok(with_host(|h| h.new_set(s)))
        }
        "dict" => construct_dict(&args, &kwargs),
        "complex" => {
            let r = args
                .first()
                .and_then(|v| with_host(|h| h.as_int(v)).map(|n| n as f64).or(as_f(v)))
                .unwrap_or(0.0);
            let i = args.get(1).and_then(as_f).unwrap_or(0.0);
            Ok(with_host(|h| h.alloc(PyObj::Complex(r, i))))
        }
        "bytes" => Ok(with_host(|h| h.alloc(PyObj::Bytes(vec![])))),
        "object" => Ok(with_host(|h| {
            h.alloc(PyObj::Instance(Instance {
                class: "object".into(),
                attrs: IndexMap::new(),
            }))
        })),
        _ => Err(host::name_error(name)),
    }
}

fn as_f(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        Value::Bool(b) => Some(*b as i64 as f64),
        _ => None,
    }
}

fn arg0(args: &[Value]) -> Result<Value, String> {
    args.first()
        .cloned()
        .ok_or_else(|| host::type_error("missing required argument"))
}

fn hash_key(k: &PKey) -> i64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    k.hash(&mut h);
    h.finish() as i64
}

fn py_len(v: &Value) -> Result<usize, String> {
    with_host(|h| match h.get(v) {
        Some(PyObj::Str(s)) => Ok(s.chars().count()),
        Some(PyObj::Bytes(b)) => Ok(b.len()),
        Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => Ok(l.len()),
        Some(PyObj::Dict(d)) => Ok(d.len()),
        Some(PyObj::Set(s)) => Ok(s.len()),
        Some(PyObj::Range { start, stop, step }) => {
            Ok(host::range_len(*start, *stop, *step).max(0) as usize)
        }
        _ => Err(host::type_error(&format!(
            "object of type '{}' has no len()",
            h.type_name(v)
        ))),
    })
    .or_else(|e| {
        // __len__ on instances.
        if with_host(|h| matches!(h.get(v), Some(PyObj::Instance(_)))) {
            let r = host::call_method(v, "__len__", vec![], vec![])?;
            Ok(with_host(|h| h.as_int(&r)).unwrap_or(0) as usize)
        } else {
            Err(e)
        }
    })
}

fn make_range(args: &[Value]) -> Result<Value, String> {
    let ints: Vec<i64> = args
        .iter()
        .map(|v| {
            with_host(|h| h.as_int(v))
                .ok_or_else(|| host::type_error("'range' requires integer arguments"))
        })
        .collect::<Result<_, _>>()?;
    let (start, stop, step) = match ints.len() {
        1 => (0, ints[0], 1),
        2 => (ints[0], ints[1], 1),
        3 => (ints[0], ints[1], ints[2]),
        _ => return Err(host::type_error("range expected 1 to 3 arguments")),
    };
    if step == 0 {
        return Err("ValueError: range() arg 3 must not be zero".into());
    }
    Ok(with_host(|h| h.alloc(PyObj::Range { start, stop, step })))
}

fn reduce_minmax(
    args: &[Value],
    kwargs: &[(String, Value)],
    want_max: bool,
) -> Result<Value, String> {
    let items = if args.len() == 1 {
        with_host(|h| h.iter_items(&args[0]))?
    } else {
        args.to_vec()
    };
    if items.is_empty() {
        if let Some(d) = kw_get(kwargs, "default") {
            return Ok(d);
        }
        return Err(format!(
            "ValueError: {}() arg is an empty sequence",
            if want_max { "max" } else { "min" }
        ));
    }
    let key = kw_get(kwargs, "key");
    let mut best = items[0].clone();
    let mut best_k = eval_key(&key, &best)?;
    for it in &items[1..] {
        let k = eval_key(&key, it)?;
        let gt = numeric_hook(NumOp::Gt, &k, &best_k)?;
        let take = with_host(|h| h.truthy(&gt)) == want_max;
        if take {
            best = it.clone();
            best_k = k;
        }
    }
    Ok(best)
}

fn eval_key(key: &Option<Value>, v: &Value) -> Result<Value, String> {
    match key {
        Some(f) if !matches!(f, Value::Undef) => host::invoke(f, vec![v.clone()], vec![]),
        _ => Ok(v.clone()),
    }
}

fn py_sorted(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let mut items = with_host(|h| h.iter_items(&arg0(args)?))?;
    let key = kw_get(kwargs, "key");
    let reverse = kw_get(kwargs, "reverse")
        .map(|v| with_host(|h| h.truthy(&v)))
        .unwrap_or(false);
    // Precompute keys.
    let mut keyed: Vec<(Value, Value)> = Vec::with_capacity(items.len());
    for it in items.drain(..) {
        let k = eval_key(&key, &it)?;
        keyed.push((k, it));
    }
    // Insertion sort using host ordering (stable, tolerant of errors).
    let mut err: Option<String> = None;
    keyed.sort_by(|a, b| {
        if err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match numeric_hook(NumOp::Lt, &a.0, &b.0) {
            Ok(v) => {
                if with_host(|h| h.truthy(&v)) {
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
    if let Some(e) = err {
        return Err(e);
    }
    let mut out: Vec<Value> = keyed.into_iter().map(|(_, v)| v).collect();
    if reverse {
        out.reverse();
    }
    Ok(with_host(|h| h.new_list(out)))
}

fn int_radix(args: &[Value], radix: u32, prefix: &str) -> Result<Value, String> {
    let a0 = arg0(args)?;
    let n = with_host(|h| h.as_int(&a0)).ok_or_else(|| host::type_error("requires an integer"))?;
    let body = match radix {
        16 => format!("{:x}", n.unsigned_abs()),
        8 => format!("{:o}", n.unsigned_abs()),
        2 => format!("{:b}", n.unsigned_abs()),
        _ => unreachable!(),
    };
    let sign = if n < 0 { "-" } else { "" };
    Ok(with_host(|h| h.new_str(format!("{sign}{prefix}{body}"))))
}

fn construct_int(args: &[Value]) -> Result<Value, String> {
    let v = match args.first() {
        Some(v) => v.clone(),
        None => return Ok(Value::Int(0)),
    };
    let base = args
        .get(1)
        .and_then(|b| with_host(|h| h.as_int(b)))
        .unwrap_or(10);
    with_host(|h| match &v {
        Value::Int(n) => Ok(Value::Int(*n)),
        Value::Bool(b) => Ok(Value::Int(*b as i64)),
        Value::Float(f) => Ok(Value::Int(*f as i64)),
        _ => {
            let s = h
                .as_str(&v)
                .ok_or_else(|| host::type_error("int() argument must be a string or a number"))?;
            let s = s.trim();
            let (neg, digits) = if let Some(r) = s.strip_prefix('-') {
                (true, r)
            } else if let Some(r) = s.strip_prefix('+') {
                (false, r)
            } else {
                (false, s)
            };
            let digits = digits.replace('_', "");
            match i64::from_str_radix(&digits, base as u32) {
                Ok(n) => Ok(Value::Int(if neg { -n } else { n })),
                Err(_) => {
                    // Try bignum.
                    match digits.parse::<num_bigint::BigInt>() {
                        Ok(b) => {
                            let b = if neg { -b } else { b };
                            Ok(h.alloc(PyObj::BigInt(b)))
                        }
                        Err(_) => Err(format!(
                            "ValueError: invalid literal for int() with base {base}: '{s}'"
                        )),
                    }
                }
            }
        }
    })
}

fn construct_float(args: &[Value]) -> Result<Value, String> {
    let v = match args.first() {
        Some(v) => v.clone(),
        None => return Ok(Value::Float(0.0)),
    };
    with_host(|h| match &v {
        Value::Int(n) => Ok(Value::Float(*n as f64)),
        Value::Float(f) => Ok(Value::Float(*f)),
        Value::Bool(b) => Ok(Value::Float(*b as i64 as f64)),
        _ => {
            let s = h
                .as_str(&v)
                .ok_or_else(|| host::type_error("float() argument must be a string or a number"))?;
            match s.trim() {
                "inf" | "infinity" | "Infinity" => Ok(Value::Float(f64::INFINITY)),
                "-inf" => Ok(Value::Float(f64::NEG_INFINITY)),
                "nan" => Ok(Value::Float(f64::NAN)),
                t => t
                    .parse::<f64>()
                    .map(Value::Float)
                    .map_err(|_| format!("ValueError: could not convert string to float: '{s}'")),
            }
        }
    })
}

fn construct_dict(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
    if let Some(v) = args.first() {
        // dict(pairs) or dict(mapping)
        let is_dict = with_host(|h| matches!(h.get(v), Some(PyObj::Dict(_))));
        if is_dict {
            with_host(|h| {
                if let Some(PyObj::Dict(m)) = h.get(v) {
                    d = m.clone();
                }
            });
        } else {
            let pairs = with_host(|h| h.iter_items(v))?;
            for p in pairs {
                let kv = with_host(|h| h.iter_items(&p))?;
                if kv.len() == 2 {
                    let key = with_host(|h| h.to_key(&kv[0]))?;
                    d.insert(key, (kv[0].clone(), kv[1].clone()));
                }
            }
        }
    }
    for (k, v) in kwargs {
        let kv = with_host(|h| h.new_str(k.clone()));
        d.insert(PKey::Str(k.clone()), (kv, v.clone()));
    }
    Ok(with_host(|h| h.new_dict(d)))
}

fn call_math(name: &str, args: &[Value]) -> Result<Value, String> {
    let f0 = args.first().and_then(as_f).unwrap_or(0.0);
    match name {
        "sqrt" => Ok(Value::Float(f0.sqrt())),
        "floor" => Ok(Value::Int(f0.floor() as i64)),
        "ceil" => Ok(Value::Int(f0.ceil() as i64)),
        "fabs" => Ok(Value::Float(f0.abs())),
        "sin" => Ok(Value::Float(f0.sin())),
        "cos" => Ok(Value::Float(f0.cos())),
        "log" => {
            let base = args.get(1).and_then(as_f);
            Ok(Value::Float(match base {
                Some(b) => f0.log(b),
                None => f0.ln(),
            }))
        }
        "pow" => {
            let e = args.get(1).and_then(as_f).unwrap_or(0.0);
            Ok(Value::Float(f0.powf(e)))
        }
        "gcd" => {
            let a = with_host(|h| h.as_int(&args[0])).unwrap_or(0).abs();
            let b = args
                .get(1)
                .and_then(|v| with_host(|h| h.as_int(v)))
                .unwrap_or(0)
                .abs();
            Ok(Value::Int(gcd(a, b)))
        }
        "factorial" => {
            let n = with_host(|h| h.as_int(&args[0])).unwrap_or(0);
            if n < 0 {
                return Err("ValueError: factorial() not defined for negative values".into());
            }
            let mut acc = num_bigint::BigInt::from(1);
            for i in 2..=n {
                acc *= i;
            }
            Ok(with_host(|h| {
                use num_traits::ToPrimitive;
                match acc.to_i64() {
                    Some(v) => Value::Int(v),
                    None => h.alloc(PyObj::BigInt(acc)),
                }
            }))
        }
        _ => Err(host::name_error(&format!("math.{name}"))),
    }
}

fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

// ── exception hierarchy ──────────────────────────────────────────────────────

const EXC_PARENTS: &[(&str, &str)] = &[
    ("Exception", "BaseException"),
    ("ArithmeticError", "Exception"),
    ("ZeroDivisionError", "ArithmeticError"),
    ("OverflowError", "ArithmeticError"),
    ("FloatingPointError", "ArithmeticError"),
    ("LookupError", "Exception"),
    ("IndexError", "LookupError"),
    ("KeyError", "LookupError"),
    ("ValueError", "Exception"),
    ("UnicodeError", "ValueError"),
    ("TypeError", "Exception"),
    ("NameError", "Exception"),
    ("UnboundLocalError", "NameError"),
    ("AttributeError", "Exception"),
    ("RuntimeError", "Exception"),
    ("RecursionError", "RuntimeError"),
    ("NotImplementedError", "RuntimeError"),
    ("StopIteration", "Exception"),
    ("StopAsyncIteration", "Exception"),
    ("AssertionError", "Exception"),
    ("ImportError", "Exception"),
    ("ModuleNotFoundError", "ImportError"),
    ("OSError", "Exception"),
    ("IOError", "OSError"),
    ("FileNotFoundError", "OSError"),
    ("PermissionError", "OSError"),
    ("KeyboardInterrupt", "BaseException"),
    ("SystemExit", "BaseException"),
    ("GeneratorExit", "BaseException"),
    ("MemoryError", "Exception"),
    ("EOFError", "Exception"),
    ("NotADirectoryError", "OSError"),
    ("IsADirectoryError", "OSError"),
    ("IndentationError", "Exception"),
    ("SyntaxError", "Exception"),
    ("UnicodeDecodeError", "UnicodeError"),
];

fn exc_parent(name: &str) -> Option<&'static str> {
    EXC_PARENTS
        .iter()
        .find(|(c, _)| *c == name)
        .map(|(_, p)| *p)
}

/// Whether `exc_class` is-a `want` in the exception hierarchy (builtin chain +
/// user class MRO).
fn exception_isa(exc_class: &str, want: &str, h: &host::PyHost) -> bool {
    if exc_class == want || want == "BaseException" {
        return true;
    }
    // Builtin chain.
    let mut cur = exc_class;
    while let Some(p) = exc_parent(cur) {
        if p == want {
            return true;
        }
        cur = p;
    }
    // User class MRO.
    if h.classes.contains_key(exc_class) {
        for c in h.mro_of(exc_class) {
            if c == want {
                return true;
            }
            if let Some(p) = exc_parent(&c) {
                if exception_isa(p, want, h) {
                    return true;
                }
            }
        }
    }
    false
}

fn isinstance(h: &host::PyHost, v: &Value, cls: &Value) -> bool {
    if let Some(PyObj::Tuple(ts)) = h.get(cls) {
        return ts.iter().any(|t| isinstance(h, v, t));
    }
    let want = match callable_name(h, cls) {
        Some(n) => n,
        None => return false,
    };
    let vt = h.type_name(v);
    type_isa(h, &vt, &want)
}

fn type_isa(h: &host::PyHost, a: &str, b: &str) -> bool {
    if a == b || b == "object" {
        return true;
    }
    // Numeric duck: bool is-a int in Python.
    if a == "bool" && b == "int" {
        return true;
    }
    if exception_isa(a, b, h) {
        return true;
    }
    if h.classes.contains_key(a) {
        return h.mro_of(a).iter().any(|c| c == b);
    }
    false
}

// ── type method dispatch ─────────────────────────────────────────────────────

/// Whether `typename` responds to method `name` (used by `getattr`/bound
/// methods to distinguish a method from an `AttributeError`).
pub fn type_has_method(typename: &str, name: &str) -> bool {
    let list: &[&str] = match typename {
        "str" => STR_METHODS,
        "list" => LIST_METHODS,
        "dict" => DICT_METHODS,
        "set" | "frozenset" => SET_METHODS,
        "tuple" => TUPLE_METHODS,
        "int" | "float" | "bool" => NUM_METHODS,
        _ => &[],
    };
    list.contains(&name)
}

const STR_METHODS: &[&str] = &[
    "upper",
    "lower",
    "strip",
    "lstrip",
    "rstrip",
    "split",
    "rsplit",
    "splitlines",
    "join",
    "replace",
    "startswith",
    "endswith",
    "find",
    "rfind",
    "index",
    "count",
    "capitalize",
    "title",
    "format",
    "isdigit",
    "isalpha",
    "isalnum",
    "isspace",
    "isupper",
    "islower",
    "zfill",
    "center",
    "ljust",
    "rjust",
    "encode",
    "removeprefix",
    "removesuffix",
    "swapcase",
    "casefold",
];
const LIST_METHODS: &[&str] = &[
    "append", "extend", "insert", "remove", "pop", "clear", "index", "count", "sort", "reverse",
    "copy",
];
const DICT_METHODS: &[&str] = &[
    "keys",
    "values",
    "items",
    "get",
    "pop",
    "popitem",
    "setdefault",
    "update",
    "clear",
    "copy",
];
const SET_METHODS: &[&str] = &[
    "add",
    "remove",
    "discard",
    "pop",
    "clear",
    "union",
    "intersection",
    "difference",
    "issubset",
    "issuperset",
    "update",
    "copy",
    "symmetric_difference",
];
const TUPLE_METHODS: &[&str] = &["count", "index"];
const NUM_METHODS: &[&str] = &["bit_length", "is_integer", "conjugate"];

/// Dispatch a method call on a builtin-typed receiver.
pub fn call_type_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    let tn = with_host(|h| h.type_name(recv));
    match tn.as_str() {
        "str" => str_method(recv, name, &args),
        "list" => list_method(recv, name, &args, &kwargs),
        "dict" => dict_method(recv, name, &args),
        "set" | "frozenset" => set_method(recv, name, &args),
        "tuple" => tuple_method(recv, name, &args),
        "int" | "float" | "bool" => num_method(recv, name, &args),
        other => Err(format!(
            "AttributeError: '{other}' object has no attribute '{name}'"
        )),
    }
}

fn str_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let s = with_host(|h| h.as_str(recv)).unwrap_or_default();
    let sarg = |i: usize| with_host(|h| args.get(i).and_then(|v| h.as_str(v))).unwrap_or_default();
    match name {
        "upper" => Ok(new_str(s.to_uppercase())),
        "lower" | "casefold" => Ok(new_str(s.to_lowercase())),
        "strip" => Ok(new_str(strip_str(&s, args, 3))),
        "lstrip" => Ok(new_str(strip_str(&s, args, 1))),
        "rstrip" => Ok(new_str(strip_str(&s, args, 2))),
        "swapcase" => Ok(new_str(
            s.chars()
                .map(|c| {
                    if c.is_uppercase() {
                        c.to_ascii_lowercase()
                    } else {
                        c.to_ascii_uppercase()
                    }
                })
                .collect::<String>(),
        )),
        "capitalize" => {
            let mut c = s.chars();
            let out = match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            };
            Ok(new_str(out))
        }
        "title" => {
            let mut out = String::new();
            let mut prev_alpha = false;
            for ch in s.chars() {
                if ch.is_alphabetic() {
                    if prev_alpha {
                        out.extend(ch.to_lowercase());
                    } else {
                        out.extend(ch.to_uppercase());
                    }
                    prev_alpha = true;
                } else {
                    out.push(ch);
                    prev_alpha = false;
                }
            }
            Ok(new_str(out))
        }
        "split" => {
            let parts: Vec<Value> = if args.is_empty() || matches!(args.first(), Some(Value::Undef))
            {
                s.split_whitespace()
                    .map(|w| new_str(w.to_string()))
                    .collect()
            } else {
                let sep = sarg(0);
                s.split(&sep).map(|p| new_str(p.to_string())).collect()
            };
            Ok(with_host(|h| h.new_list(parts)))
        }
        "rsplit" => {
            let sep = sarg(0);
            let parts: Vec<Value> = if sep.is_empty() {
                s.split_whitespace()
                    .map(|w| new_str(w.to_string()))
                    .collect()
            } else {
                s.split(&sep).map(|p| new_str(p.to_string())).collect()
            };
            Ok(with_host(|h| h.new_list(parts)))
        }
        "splitlines" => {
            let parts: Vec<Value> = s.lines().map(|l| new_str(l.to_string())).collect();
            Ok(with_host(|h| h.new_list(parts)))
        }
        "join" => {
            let items = with_host(|h| h.iter_items(&args[0]))?;
            let mut strs = Vec::new();
            for it in items {
                strs.push(
                    with_host(|h| h.as_str(&it))
                        .ok_or_else(|| host::type_error("sequence item: expected str instance"))?,
                );
            }
            Ok(new_str(strs.join(&s)))
        }
        "replace" => {
            let from = sarg(0);
            let to = sarg(1);
            let count = args.get(2).and_then(|v| with_host(|h| h.as_int(v)));
            let out = match count {
                Some(n) if n >= 0 => s.replacen(&from, &to, n as usize),
                _ => s.replace(&from, &to),
            };
            Ok(new_str(out))
        }
        "startswith" => Ok(Value::Bool(s.starts_with(&sarg(0)))),
        "endswith" => Ok(Value::Bool(s.ends_with(&sarg(0)))),
        "find" => Ok(Value::Int(
            s.find(&sarg(0))
                .map(|b| s[..b].chars().count() as i64)
                .unwrap_or(-1),
        )),
        "rfind" => Ok(Value::Int(
            s.rfind(&sarg(0))
                .map(|b| s[..b].chars().count() as i64)
                .unwrap_or(-1),
        )),
        "index" => match s.find(&sarg(0)) {
            Some(b) => Ok(Value::Int(s[..b].chars().count() as i64)),
            None => Err("ValueError: substring not found".into()),
        },
        "count" => {
            let sub = sarg(0);
            if sub.is_empty() {
                Ok(Value::Int(s.chars().count() as i64 + 1))
            } else {
                Ok(Value::Int(s.matches(&sub).count() as i64))
            }
        }
        "isdigit" => Ok(Value::Bool(
            !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()),
        )),
        "isalpha" => Ok(Value::Bool(
            !s.is_empty() && s.chars().all(|c| c.is_alphabetic()),
        )),
        "isalnum" => Ok(Value::Bool(
            !s.is_empty() && s.chars().all(|c| c.is_alphanumeric()),
        )),
        "isspace" => Ok(Value::Bool(
            !s.is_empty() && s.chars().all(|c| c.is_whitespace()),
        )),
        "isupper" => Ok(Value::Bool(
            s.chars().any(|c| c.is_alphabetic())
                && s.chars()
                    .filter(|c| c.is_alphabetic())
                    .all(|c| c.is_uppercase()),
        )),
        "islower" => Ok(Value::Bool(
            s.chars().any(|c| c.is_alphabetic())
                && s.chars()
                    .filter(|c| c.is_alphabetic())
                    .all(|c| c.is_lowercase()),
        )),
        "zfill" => {
            let w = with_host(|h| h.as_int(&args[0])).unwrap_or(0) as usize;
            let out = if s.len() < w {
                let pad = "0".repeat(w - s.chars().count());
                if let Some(rest) = s.strip_prefix('-') {
                    format!("-{pad}{rest}")
                } else {
                    format!("{pad}{s}")
                }
            } else {
                s.clone()
            };
            Ok(new_str(out))
        }
        "center" => Ok(new_str(pad_str(&s, args, 'c'))),
        "ljust" => Ok(new_str(pad_str(&s, args, 'l'))),
        "rjust" => Ok(new_str(pad_str(&s, args, 'r'))),
        "removeprefix" => {
            let p = sarg(0);
            Ok(new_str(
                s.strip_prefix(&p).map(|r| r.to_string()).unwrap_or(s),
            ))
        }
        "removesuffix" => {
            let p = sarg(0);
            Ok(new_str(
                s.strip_suffix(&p).map(|r| r.to_string()).unwrap_or(s),
            ))
        }
        "encode" => Ok(with_host(|h| h.alloc(PyObj::Bytes(s.into_bytes())))),
        "format" => str_dot_format(&s, args),
        _ => Err(format!(
            "AttributeError: 'str' object has no attribute '{name}'"
        )),
    }
}

fn strip_str(s: &str, args: &[Value], mode: u8) -> String {
    let chars: Option<String> = with_host(|h| args.first().and_then(|v| h.as_str(v)));
    let pred = |c: char| match &chars {
        Some(set) => set.contains(c),
        None => c.is_whitespace(),
    };
    let out = match mode {
        1 => s.trim_start_matches(pred),
        2 => s.trim_end_matches(pred),
        _ => s.trim_matches(pred),
    };
    out.to_string()
}

fn pad_str(s: &str, args: &[Value], mode: char) -> String {
    let w = with_host(|h| h.as_int(&args[0])).unwrap_or(0) as usize;
    let fill = with_host(|h| args.get(1).and_then(|v| h.as_str(v)))
        .and_then(|f| f.chars().next())
        .unwrap_or(' ');
    let len = s.chars().count();
    if len >= w {
        return s.to_string();
    }
    let total = w - len;
    match mode {
        'l' => format!("{s}{}", fill.to_string().repeat(total)),
        'r' => format!("{}{s}", fill.to_string().repeat(total)),
        _ => {
            let left = total / 2;
            let right = total - left;
            format!(
                "{}{s}{}",
                fill.to_string().repeat(left),
                fill.to_string().repeat(right)
            )
        }
    }
}

/// `str.format(*args, **kwargs)` — positional `{}` / `{0}` and `{name}` fields.
fn str_dot_format(s: &str, args: &[Value]) -> Result<Value, String> {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut auto = 0;
    while i < chars.len() {
        match chars[i] {
            '{' if chars.get(i + 1) == Some(&'{') => {
                out.push('{');
                i += 2;
            }
            '}' if chars.get(i + 1) == Some(&'}') => {
                out.push('}');
                i += 2;
            }
            '{' => {
                let mut field = String::new();
                i += 1;
                while i < chars.len() && chars[i] != '}' {
                    field.push(chars[i]);
                    i += 1;
                }
                i += 1; // skip }
                let (fname, spec) = match field.split_once(':') {
                    Some((a, b)) => (a.to_string(), b.to_string()),
                    None => (field, String::new()),
                };
                let val = if fname.is_empty() {
                    let v = args.get(auto).cloned().unwrap_or(Value::Undef);
                    auto += 1;
                    v
                } else if let Ok(n) = fname.parse::<usize>() {
                    args.get(n).cloned().unwrap_or(Value::Undef)
                } else {
                    Value::Undef
                };
                let sv = py_str(&val)?;
                out.push_str(&apply_format_spec(&sv, &val, &spec));
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(new_str(out))
}

fn list_method(
    recv: &Value,
    name: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value, String> {
    match name {
        "append" => {
            let v = arg0(args)?;
            with_host(|h| {
                if let Some(PyObj::List(l)) = h.get_mut(recv) {
                    l.push(v);
                }
            });
            Ok(Value::Undef)
        }
        "extend" => {
            let items = with_host(|h| h.iter_items(&arg0(args)?))?;
            with_host(|h| {
                if let Some(PyObj::List(l)) = h.get_mut(recv) {
                    l.extend(items);
                }
            });
            Ok(Value::Undef)
        }
        "insert" => {
            let a0 = arg0(args)?;
            let idx = with_host(|h| h.as_int(&a0)).unwrap_or(0);
            let v = args.get(1).cloned().unwrap_or(Value::Undef);
            with_host(|h| {
                if let Some(PyObj::List(l)) = h.get_mut(recv) {
                    let n = l.len() as i64;
                    let k = if idx < 0 {
                        (idx + n).max(0)
                    } else {
                        idx.min(n)
                    } as usize;
                    l.insert(k, v);
                }
            });
            Ok(Value::Undef)
        }
        "pop" => {
            let idx = args.first().and_then(|v| with_host(|h| h.as_int(v)));
            with_host(|h| {
                if let Some(PyObj::List(l)) = h.get_mut(recv) {
                    if l.is_empty() {
                        return Err("IndexError: pop from empty list".into());
                    }
                    let n = l.len() as i64;
                    let k = match idx {
                        Some(i) => {
                            let k = if i < 0 { i + n } else { i };
                            if k < 0 || k >= n {
                                return Err("IndexError: pop index out of range".into());
                            }
                            k as usize
                        }
                        None => l.len() - 1,
                    };
                    Ok(l.remove(k))
                } else {
                    Err(host::type_error("not a list"))
                }
            })
        }
        "remove" => {
            let v = arg0(args)?;
            with_host(|h| {
                let pos = if let Some(PyObj::List(l)) = h.get(recv) {
                    l.iter().position(|x| h.equal(x, &v))
                } else {
                    None
                };
                match pos {
                    Some(p) => {
                        if let Some(PyObj::List(l)) = h.get_mut(recv) {
                            l.remove(p);
                        }
                        Ok(Value::Undef)
                    }
                    None => Err("ValueError: list.remove(x): x not in list".into()),
                }
            })
        }
        "clear" => {
            with_host(|h| {
                if let Some(PyObj::List(l)) = h.get_mut(recv) {
                    l.clear();
                }
            });
            Ok(Value::Undef)
        }
        "index" => {
            let v = arg0(args)?;
            with_host(|h| {
                if let Some(PyObj::List(l)) = h.get(recv) {
                    match l.iter().position(|x| h.equal(x, &v)) {
                        Some(p) => Ok(Value::Int(p as i64)),
                        None => Err("ValueError: is not in list".into()),
                    }
                } else {
                    Err(host::type_error("not a list"))
                }
            })
        }
        "count" => {
            let v = arg0(args)?;
            Ok(Value::Int(with_host(|h| {
                if let Some(PyObj::List(l)) = h.get(recv) {
                    l.iter().filter(|x| h.equal(x, &v)).count() as i64
                } else {
                    0
                }
            })))
        }
        "reverse" => {
            with_host(|h| {
                if let Some(PyObj::List(l)) = h.get_mut(recv) {
                    l.reverse();
                }
            });
            Ok(Value::Undef)
        }
        "copy" => {
            let items = with_host(|h| match h.get(recv) {
                Some(PyObj::List(l)) => l.clone(),
                _ => vec![],
            });
            Ok(with_host(|h| h.new_list(items)))
        }
        "sort" => {
            let items = with_host(|h| match h.get(recv) {
                Some(PyObj::List(l)) => l.clone(),
                _ => vec![],
            });
            let tmp = with_host(|h| h.new_list(items));
            let sorted = py_sorted(&[tmp], kwargs)?;
            let new_items = with_host(|h| match h.get(&sorted) {
                Some(PyObj::List(l)) => l.clone(),
                _ => vec![],
            });
            with_host(|h| {
                if let Some(PyObj::List(l)) = h.get_mut(recv) {
                    *l = new_items;
                }
            });
            Ok(Value::Undef)
        }
        _ => Err(format!(
            "AttributeError: 'list' object has no attribute '{name}'"
        )),
    }
}

fn dict_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "keys" => {
            let ks = with_host(|h| match h.get(recv) {
                Some(PyObj::Dict(d)) => d.values().map(|(k, _)| k.clone()).collect(),
                _ => vec![],
            });
            Ok(with_host(|h| h.new_list(ks)))
        }
        "values" => {
            let vs = with_host(|h| match h.get(recv) {
                Some(PyObj::Dict(d)) => d.values().map(|(_, v)| v.clone()).collect(),
                _ => vec![],
            });
            Ok(with_host(|h| h.new_list(vs)))
        }
        "items" => {
            let items = with_host(|h| match h.get(recv) {
                Some(PyObj::Dict(d)) => d
                    .values()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
                _ => vec![],
            });
            let tuples: Vec<Value> = with_host(|h| {
                items
                    .into_iter()
                    .map(|(k, v)| h.new_tuple(vec![k, v]))
                    .collect()
            });
            Ok(with_host(|h| h.new_list(tuples)))
        }
        "get" => {
            let key = with_host(|h| h.to_key(&arg0(args)?))?;
            Ok(with_host(|h| match h.get(recv) {
                Some(PyObj::Dict(d)) => d.get(&key).map(|(_, v)| v.clone()),
                _ => None,
            })
            .unwrap_or_else(|| args.get(1).cloned().unwrap_or(Value::Undef)))
        }
        "pop" => {
            let kv = arg0(args)?;
            let key = with_host(|h| h.to_key(&kv))?;
            let got = with_host(|h| {
                if let Some(PyObj::Dict(d)) = h.get_mut(recv) {
                    d.shift_remove(&key).map(|(_, v)| v)
                } else {
                    None
                }
            });
            match got {
                Some(v) => Ok(v),
                None => match args.get(1) {
                    Some(d) => Ok(d.clone()),
                    None => Err(format!("KeyError: {}", with_host(|h| h.repr_of(&kv)))),
                },
            }
        }
        "setdefault" => {
            let kv = arg0(args)?;
            let default = args.get(1).cloned().unwrap_or(Value::Undef);
            let key = with_host(|h| h.to_key(&kv))?;
            Ok(with_host(|h| {
                if let Some(PyObj::Dict(d)) = h.get(recv) {
                    if let Some((_, v)) = d.get(&key) {
                        return v.clone();
                    }
                }
                if let Some(PyObj::Dict(d)) = h.get_mut(recv) {
                    d.insert(key, (kv, default.clone()));
                }
                default
            }))
        }
        "update" => {
            if let Some(other) = args.first() {
                let pairs = with_host(|h| match h.get(other) {
                    Some(PyObj::Dict(d)) => d
                        .iter()
                        .map(|(k, (kv, v))| (k.clone(), kv.clone(), v.clone()))
                        .collect::<Vec<_>>(),
                    _ => vec![],
                });
                with_host(|h| {
                    if let Some(PyObj::Dict(d)) = h.get_mut(recv) {
                        for (k, kv, v) in pairs {
                            d.insert(k, (kv, v));
                        }
                    }
                });
            }
            Ok(Value::Undef)
        }
        "clear" => {
            with_host(|h| {
                if let Some(PyObj::Dict(d)) = h.get_mut(recv) {
                    d.clear();
                }
            });
            Ok(Value::Undef)
        }
        "copy" => {
            let d = with_host(|h| match h.get(recv) {
                Some(PyObj::Dict(d)) => d.clone(),
                _ => IndexMap::new(),
            });
            Ok(with_host(|h| h.new_dict(d)))
        }
        "popitem" => {
            let got = with_host(|h| {
                if let Some(PyObj::Dict(d)) = h.get_mut(recv) {
                    d.pop().map(|(_, (k, v))| (k, v))
                } else {
                    None
                }
            });
            match got {
                Some((k, v)) => Ok(with_host(|h| h.new_tuple(vec![k, v]))),
                None => Err("KeyError: 'popitem(): dictionary is empty'".into()),
            }
        }
        _ => Err(format!(
            "AttributeError: 'dict' object has no attribute '{name}'"
        )),
    }
}

fn set_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "add" => {
            let v = arg0(args)?;
            let k = with_host(|h| h.to_key(&v))?;
            with_host(|h| {
                if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                    s.insert(k, v);
                }
            });
            Ok(Value::Undef)
        }
        "discard" | "remove" => {
            let v = arg0(args)?;
            let k = with_host(|h| h.to_key(&v))?;
            let removed = with_host(|h| {
                if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                    s.shift_remove(&k).is_some()
                } else {
                    false
                }
            });
            if name == "remove" && !removed {
                return Err(format!("KeyError: {}", with_host(|h| h.repr_of(&v))));
            }
            Ok(Value::Undef)
        }
        "clear" => {
            with_host(|h| {
                if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                    s.clear();
                }
            });
            Ok(Value::Undef)
        }
        "union" => set_binop(recv, args, host::binop::BITOR),
        "intersection" => set_binop(recv, args, host::binop::BITAND),
        "symmetric_difference" => set_binop(recv, args, host::binop::BITXOR),
        "difference" => {
            let other = arg0(args)?;
            let other_set = if with_host(|h| matches!(h.get(&other), Some(PyObj::Set(_)))) {
                other
            } else {
                call_builtin_function("set", vec![other], vec![])?
            };
            with_host(|h| h.arith(NumOp::Sub, recv, &other_set))
        }
        "issubset" => {
            let a0 = arg0(args)?;
            let other = with_host(|h| set_keys(h, &a0));
            Ok(Value::Bool(with_host(|h| {
                set_keys(h, recv).iter().all(|k| other.contains(k))
            })))
        }
        "issuperset" => {
            let a0 = arg0(args)?;
            let other = with_host(|h| set_keys(h, &a0));
            Ok(Value::Bool(with_host(|h| {
                other.iter().all(|k| set_keys(h, recv).contains(k))
            })))
        }
        "copy" => {
            let s = with_host(|h| match h.get(recv) {
                Some(PyObj::Set(s)) => s.clone(),
                _ => IndexMap::new(),
            });
            Ok(with_host(|h| h.new_set(s)))
        }
        "update" => {
            let items = with_host(|h| h.iter_items(&arg0(args)?))?;
            for it in items {
                let k = with_host(|h| h.to_key(&it))?;
                with_host(|h| {
                    if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                        s.insert(k, it);
                    }
                });
            }
            Ok(Value::Undef)
        }
        "pop" => with_host(|h| {
            if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                match s.pop() {
                    Some((_, v)) => Ok(v),
                    None => Err("KeyError: 'pop from an empty set'".into()),
                }
            } else {
                Err(host::type_error("not a set"))
            }
        }),
        _ => Err(format!(
            "AttributeError: 'set' object has no attribute '{name}'"
        )),
    }
}

fn set_keys(h: &host::PyHost, v: &Value) -> Vec<PKey> {
    match h.get(v) {
        Some(PyObj::Set(s)) => s.keys().cloned().collect(),
        _ => vec![],
    }
}

fn set_binop(recv: &Value, args: &[Value], tag: i64) -> Result<Value, String> {
    let other = arg0(args)?;
    // Coerce a non-set argument to a set first.
    let other_set = if with_host(|h| matches!(h.get(&other), Some(PyObj::Set(_)))) {
        other
    } else {
        call_builtin_function("set", vec![other], vec![])?
    };
    with_host(|h| h.binop(tag, recv, &other_set))
}

fn tuple_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "count" => {
            let v = arg0(args)?;
            Ok(Value::Int(with_host(|h| match h.get(recv) {
                Some(PyObj::Tuple(l)) => l.iter().filter(|x| h.equal(x, &v)).count() as i64,
                _ => 0,
            })))
        }
        "index" => {
            let v = arg0(args)?;
            with_host(|h| match h.get(recv) {
                Some(PyObj::Tuple(l)) => match l.iter().position(|x| h.equal(x, &v)) {
                    Some(p) => Ok(Value::Int(p as i64)),
                    None => Err("ValueError: tuple.index(x): x not in tuple".into()),
                },
                _ => Err(host::type_error("not a tuple")),
            })
        }
        _ => Err(format!(
            "AttributeError: 'tuple' object has no attribute '{name}'"
        )),
    }
}

fn num_method(recv: &Value, name: &str, _args: &[Value]) -> Result<Value, String> {
    match name {
        "bit_length" => {
            let n = with_host(|h| h.as_int(recv)).unwrap_or(0);
            Ok(Value::Int(64 - n.unsigned_abs().leading_zeros() as i64))
        }
        "is_integer" => Ok(Value::Bool(match recv {
            Value::Float(f) => f.fract() == 0.0,
            Value::Int(_) | Value::Bool(_) => true,
            _ => false,
        })),
        "conjugate" => Ok(recv.clone()),
        _ => Err(format!("AttributeError: object has no attribute '{name}'")),
    }
}

fn new_str(s: String) -> Value {
    with_host(|h| h.new_str(s))
}

// ── format spec (`{:spec}`) ──────────────────────────────────────────────────

/// Apply a `format()` mini-language spec to a stringified value. Supports
/// `[[fill]align][sign][#][0][width][,][.prec][type]` for the common cases.
pub fn apply_format_spec(s: &str, v: &Value, spec: &str) -> String {
    if spec.is_empty() {
        return s.to_string();
    }
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let mut fill = ' ';
    let mut align = '\0';
    // [[fill]align]
    if chars.len() >= 2 && matches!(chars[1], '<' | '>' | '^' | '=') {
        fill = chars[0];
        align = chars[1];
        i = 2;
    } else if !chars.is_empty() && matches!(chars[0], '<' | '>' | '^' | '=') {
        align = chars[0];
        i = 1;
    }
    let mut sign = '\0';
    if i < chars.len() && matches!(chars[i], '+' | '-' | ' ') {
        sign = chars[i];
        i += 1;
    }
    if i < chars.len() && chars[i] == '#' {
        i += 1;
    }
    if i < chars.len() && chars[i] == '0' {
        if align == '\0' {
            align = '=';
            fill = '0';
        }
        i += 1;
    }
    let mut width = 0usize;
    while i < chars.len() && chars[i].is_ascii_digit() {
        width = width * 10 + (chars[i] as usize - '0' as usize);
        i += 1;
    }
    let comma = i < chars.len() && chars[i] == ',';
    if comma {
        i += 1;
    }
    let mut prec: Option<usize> = None;
    if i < chars.len() && chars[i] == '.' {
        i += 1;
        let mut p = 0usize;
        while i < chars.len() && chars[i].is_ascii_digit() {
            p = p * 10 + (chars[i] as usize - '0' as usize);
            i += 1;
        }
        prec = Some(p);
    }
    let ty = chars.get(i).copied().unwrap_or('\0');

    // Render body by type.
    let mut body = match ty {
        'd' => match as_i(v) {
            Some(n) => n.to_string(),
            None => s.to_string(),
        },
        'f' | 'F' => {
            let f = as_f(v).unwrap_or(0.0);
            format!("{:.*}", prec.unwrap_or(6), f)
        }
        'e' | 'E' => {
            let f = as_f(v).unwrap_or(0.0);
            format!("{:.*e}", prec.unwrap_or(6), f)
        }
        'x' => format!("{:x}", as_i(v).unwrap_or(0)),
        'X' => format!("{:X}", as_i(v).unwrap_or(0)),
        'o' => format!("{:o}", as_i(v).unwrap_or(0)),
        'b' => format!("{:b}", as_i(v).unwrap_or(0)),
        '%' => {
            let f = as_f(v).unwrap_or(0.0) * 100.0;
            format!("{:.*}%", prec.unwrap_or(6), f)
        }
        _ => {
            let mut body = s.to_string();
            if let Some(p) = prec {
                if matches!(v, Value::Str(_)) || is_str(v) {
                    body = body.chars().take(p).collect();
                } else if as_f(v).is_some() {
                    body = format!("{:.*}", p, as_f(v).unwrap());
                }
            }
            body
        }
    };

    if comma {
        body = add_thousands(&body);
    }
    if sign == '+' && as_f(v).map(|f| f >= 0.0).unwrap_or(false) {
        body = format!("+{body}");
    }

    let len = body.chars().count();
    if len >= width {
        return body;
    }
    let pad = width - len;
    match align {
        '<' => format!("{body}{}", fill.to_string().repeat(pad)),
        '^' => {
            let l = pad / 2;
            let r = pad - l;
            format!(
                "{}{body}{}",
                fill.to_string().repeat(l),
                fill.to_string().repeat(r)
            )
        }
        '=' => {
            // sign-aware zero pad
            if let Some(rest) = body.strip_prefix('-') {
                format!("-{}{rest}", fill.to_string().repeat(pad))
            } else {
                format!("{}{body}", fill.to_string().repeat(pad))
            }
        }
        '>' => format!("{}{body}", fill.to_string().repeat(pad)),
        _ => {
            // default: numbers right-align, strings left-align
            if as_f(v).is_some() {
                format!("{}{body}", fill.to_string().repeat(pad))
            } else {
                format!("{body}{}", fill.to_string().repeat(pad))
            }
        }
    }
}

fn as_i(v: &Value) -> Option<i64> {
    match v {
        Value::Int(n) => Some(*n),
        Value::Bool(b) => Some(*b as i64),
        Value::Float(f) => Some(*f as i64),
        _ => None,
    }
}

fn is_str(v: &Value) -> bool {
    matches!(v, Value::Str(_)) || with_host(|h| h.as_str(v).is_some())
}

fn add_thousands(s: &str) -> String {
    let (sign, digits) = match s.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", s),
    };
    let (int_part, frac) = match digits.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (digits, None),
    };
    let mut out = String::new();
    let bytes: Vec<char> = int_part.chars().collect();
    for (idx, c) in bytes.iter().enumerate() {
        if idx > 0 && (bytes.len() - idx) % 3 == 0 {
            out.push(',');
        }
        out.push(*c);
    }
    match frac {
        Some(f) => format!("{sign}{out}.{f}"),
        None => format!("{sign}{out}"),
    }
}
