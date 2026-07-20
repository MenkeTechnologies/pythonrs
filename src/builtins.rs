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
    vm.register_builtin(ops::MKBYTES, b_mkbytes);
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
    vm.register_builtin(ops::GENRET, b_genret);
    vm.register_builtin(ops::AWAIT, b_await);
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
    vm.register_builtin(ops::YIELDV, b_yieldv);
    vm.register_builtin(ops::DECLARE_NONLOCAL, b_declare_nonlocal);
    vm.register_builtin(ops::CALL_EX, b_call_ex);
    vm.register_builtin(ops::CALL_VALUE_EX, b_call_value_ex);
    vm.register_builtin(ops::CALL_METHOD_EX, b_call_method_ex);
    vm.register_builtin(ops::BUILD_ARGS, b_build_args);
    vm.register_builtin(ops::BUILD_KWARGS, b_build_kwargs);
    vm.register_builtin(ops::MKDICT_EX, b_mkdict_ex);
    vm.register_builtin(ops::MATCH_SEQ, b_match_seq);
    vm.register_builtin(ops::MATCH_MAP_CHECK, b_match_map_check);
    vm.register_builtin(ops::MATCH_KEY, b_match_key);
    vm.register_builtin(ops::MATCH_MAP_REST, b_match_map_rest);
    vm.register_builtin(ops::MATCH_CLASS, b_match_class);
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

/// Record the source line of the op that is aborting the chunk into the current
/// frame, so an uncaught exception's traceback can name it. The dispatch loop
/// pre-increments `ip`, so the failing op sits at `ip - 1`.
fn record_err_line(vm: &VM) {
    let idx = vm.ip.saturating_sub(1);
    if let Some(&line) = vm.chunk.lines.get(idx) {
        if line != 0 {
            with_host(|h| h.set_cur_line(line));
        }
    }
}

fn abort(vm: &mut VM, e: String) -> Value {
    record_err_line(vm);
    with_host(|h| h.error = Some(e));
    vm.ip = vm.chunk.ops.len();
    Value::Undef
}

/// Halt the chunk if a call left an error or non-local signal pending.
fn finish(vm: &mut VM, r: Result<Value, String>) -> Value {
    match r {
        Ok(v) => {
            if with_host(|h| h.error.is_some() || h.signal.is_some()) {
                if with_host(|h| h.error.is_some()) {
                    record_err_line(vm);
                }
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
    if name == "NotImplemented" {
        return with_host(|h| h.alloc(PyObj::NotImplemented));
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
    if name == "NotImplemented" {
        return with_host(|h| h.alloc(PyObj::NotImplemented));
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

fn b_declare_nonlocal(vm: &mut VM, _: u8) -> Value {
    let name = sval(&vm.pop());
    with_host(|h| h.declare_nonlocal(&name));
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
    let r = get_attr_desc(&recv, &name);
    finish(vm, r)
}

/// `recv.name` with the descriptor protocol and the `__getattr__` fallback. The
/// accessor bodies run user code, so this holds no host borrow across them.
fn get_attr_desc(recv: &Value, name: &str) -> Result<Value, String> {
    match with_host(|h| h.plan_attr_get(recv, name)) {
        host::AttrGet::Property { fget, inst, owner } => {
            if matches!(fget, Value::Undef) {
                let cls = with_host(|h| h.type_name(&inst));
                return Err(format!(
                    "AttributeError: property '{name}' of '{cls}' object has no getter"
                ));
            }
            // Run the getter as a bound method so `self` and the defining class
            // land on the frame — a zero-arg `super()` inside the getter reads
            // them (`invoke` would bind `inst` as a plain arg and leave both unset).
            match with_host(|h| h.get(&fget).cloned()) {
                Some(PyObj::Func(fv)) => {
                    host::run_user_func(&fv, Some(inst), owner, vec![], vec![])
                }
                _ => host::invoke(&fget, vec![inst], vec![]),
            }
        }
        host::AttrGet::Descriptor { desc, inst, cls } => {
            host::call_method(&desc, "__get__", vec![inst, cls], vec![])
        }
        host::AttrGet::Plain => match with_host(|h| h.get_attr(recv, name)) {
            Ok(v) => Ok(v),
            // `__getattr__` fallback: fires only when normal lookup fails.
            Err(e) => {
                let has = with_host(|h| match h.get(recv) {
                    Some(PyObj::Instance(i)) => h.class_lookup(&i.class, "__getattr__").is_some(),
                    _ => false,
                });
                if has {
                    let nm = with_host(|h| h.new_str(name.to_string()));
                    host::call_method(recv, "__getattr__", vec![nm], vec![])
                } else {
                    Err(e)
                }
            }
        },
    }
}

fn b_setattr(vm: &mut VM, _: u8) -> Value {
    let val = vm.pop();
    let name = sval(&vm.pop());
    let recv = vm.pop();
    let r = set_attr_desc(&recv, &name, val);
    match r {
        Ok(()) => Value::Undef,
        Err(e) => abort(vm, e),
    }
}

/// `recv.name = val` with the data-descriptor protocol (`property.fset`,
/// user `__set__`).
fn set_attr_desc(recv: &Value, name: &str, val: Value) -> Result<(), String> {
    match with_host(|h| h.plan_attr_set(recv, name, &val)) {
        host::AttrSet::Property {
            fset,
            inst,
            val,
            owner,
        } => {
            if matches!(fset, Value::Undef) {
                let cls = with_host(|h| h.type_name(&inst));
                return Err(format!(
                    "AttributeError: property '{name}' of '{cls}' object has no setter"
                ));
            }
            // As with the getter, run the setter as a bound method so `self` and
            // the defining class are on the frame for a zero-arg `super()`.
            match with_host(|h| h.get(&fset).cloned()) {
                Some(PyObj::Func(fv)) => {
                    host::run_user_func(&fv, Some(inst), owner, vec![val], vec![]).map(|_| ())
                }
                _ => host::invoke(&fset, vec![inst, val], vec![]).map(|_| ()),
            }
        }
        host::AttrSet::Descriptor { desc, inst, val } => {
            host::call_method(&desc, "__set__", vec![inst, val], vec![]).map(|_| ())
        }
        host::AttrSet::Plain => with_host(|h| h.set_attr(recv, name, val)),
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
    // dict-subclass `__missing__`: Counter → 0, defaultdict → default_factory().
    if let Some(meta) = host::dict_meta_of(&recv) {
        let missing = with_host(|h| match h.to_key(&idx) {
            Ok(k) => match h.get(&recv) {
                Some(PyObj::Dict(d)) => !d.contains_key(&k),
                _ => false,
            },
            Err(_) => false,
        });
        if missing {
            match meta.kind {
                host::DictKind::Counter => return finish(vm, Ok(Value::Int(0))),
                host::DictKind::DefaultDict => {
                    if let Some(factory) = meta.factory {
                        let r = (|| {
                            let default = host::invoke(&factory, vec![], vec![])?;
                            with_host(|h| h.set_item(&recv, &idx, default.clone()))?;
                            Ok(default)
                        })();
                        return finish(vm, r);
                    }
                }
                host::DictKind::OrderedDict => {}
            }
        }
    }
    let cands = host::instance_key_candidates(&recv);
    let r = host::with_instance_key(&idx, &cands, || with_host(|h| h.get_item(&recv, &idx)));
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
    // Slice assignment (`x[i:j] = it`): materialize the RHS iterable here, out
    // of any host borrow (it may be a generator), then splice.
    if with_host(|h| matches!(h.get(&idx), Some(PyObj::Slice { .. }))) {
        let repl = match host::iter_vec(&val) {
            Ok(v) => v,
            Err(e) => return abort(vm, e),
        };
        return match with_host(|h| h.set_slice_vals(&recv, &idx, repl)) {
            Ok(()) => val,
            Err(e) => abort(vm, e),
        };
    }
    let cands = host::instance_key_candidates(&recv);
    match host::with_instance_key(&idx, &cands, || {
        with_host(|h| h.set_item(&recv, &idx, val.clone()))
    }) {
        Ok(()) => val,
        Err(e) => abort(vm, e),
    }
}

fn b_delitem(vm: &mut VM, _: u8) -> Value {
    let idx = vm.pop();
    let recv = vm.pop();
    let cands = host::instance_key_candidates(&recv);
    match host::with_instance_key(&idx, &cands, || with_host(|h| h.del_item(&recv, &idx))) {
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

/// Materialize a `bytes` literal. The compiler packs the literal's bytes into a
/// latin-1 string constant (one code point per byte); here we unpack it back to
/// the raw `Vec<u8>`.
fn b_mkbytes(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.pop();
    let bytes: Vec<u8> = with_host(|h| match h.get(&v) {
        Some(PyObj::Str(s)) => s.chars().map(|c| c as u32 as u8).collect(),
        _ => match &v {
            Value::Str(s) => s.chars().map(|c| c as u32 as u8).collect(),
            _ => vec![],
        },
    });
    with_host(|h| h.alloc(PyObj::Bytes(bytes)))
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
        // Instance elements resolve their key (running `__hash__`, collapsing a
        // value-equal earlier element) before the borrowed `to_key`.
        let cands = host::set_local_candidates(&set);
        let key = host::with_instance_key(&it, &cands, || with_host(|h| h.to_key(&it)));
        match key {
            Ok(k) => host::set_put(&mut set, k, it),
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
        let cands = host::dict_local_candidates(&d);
        let key = host::with_instance_key(&k, &cands, || with_host(|h| h.to_key(&k)));
        match key {
            Ok(key) => host::dict_put(&mut d, key, k, v),
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

// ── generators ────────────────────────────────────────────────────────────────

fn b_yieldv(vm: &mut VM, _: u8) -> Value {
    let v = vm.pop();
    match host::gen_yield(v) {
        Ok(sent) => sent,
        Err(e) => abort(vm, e),
    }
}

/// `await E` — drive the awaitable, suspending the coroutine until it settles.
fn b_await(vm: &mut VM, _: u8) -> Value {
    let awaitable = vm.pop();
    match crate::async_rt::await_value(awaitable) {
        Ok(v) => v,
        Err(e) => abort(vm, e),
    }
}

// ── call-site * / ** unpacking (EX ops) ───────────────────────────────────────

/// Positional args from a `list` value.
fn list_args(v: &Value) -> Vec<Value> {
    with_host(|h| match h.get(v) {
        Some(PyObj::List(l)) => l.clone(),
        _ => Vec::new(),
    })
}

fn b_call_ex(vm: &mut VM, _: u8) -> Value {
    let kwd = vm.pop();
    let argl = vm.pop();
    let name = sval(&vm.pop());
    let r = host::call_named(&name, list_args(&argl), kw_pairs(&kwd));
    finish(vm, r)
}

fn b_call_value_ex(vm: &mut VM, _: u8) -> Value {
    let kwd = vm.pop();
    let argl = vm.pop();
    let callable = vm.pop();
    let r = host::invoke(&callable, list_args(&argl), kw_pairs(&kwd));
    finish(vm, r)
}

fn b_call_method_ex(vm: &mut VM, _: u8) -> Value {
    let kwd = vm.pop();
    let argl = vm.pop();
    let name = sval(&vm.pop());
    let recv = vm.pop();
    let r = host::call_method(&recv, &name, list_args(&argl), kw_pairs(&kwd));
    finish(vm, r)
}

/// Flatten a positional-arg spread: pairs `(tag, value)`, tag 1 = `*` spread.
fn b_build_args(vm: &mut VM, argc: u8) -> Value {
    let flat = pop_n(vm, argc as usize);
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < flat.len() {
        let spread = matches!(flat[i], Value::Int(1));
        let val = flat[i + 1].clone();
        if spread {
            match host::iter_vec(&val) {
                Ok(items) => out.extend(items),
                Err(e) => return abort(vm, e),
            }
        } else {
            out.push(val);
        }
        i += 2;
    }
    with_host(|h| h.new_list(out))
}

/// Build a kwargs `dict`: pairs `(key, value)`, a `None`(Undef) key = `**` spread.
fn b_build_kwargs(vm: &mut VM, argc: u8) -> Value {
    let flat = pop_n(vm, argc as usize);
    let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
    let mut i = 0;
    while i + 1 < flat.len() {
        let key = flat[i].clone();
        let val = flat[i + 1].clone();
        if matches!(key, Value::Undef) {
            // **mapping spread — copy each str key/value.
            let pairs = with_host(|h| match h.get(&val) {
                Some(PyObj::Dict(m)) => m
                    .iter()
                    .map(|(k, (kv, v))| (k.clone(), kv.clone(), v.clone()))
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            });
            for (k, kv, v) in pairs {
                host::dict_put(&mut d, k, kv, v);
            }
        } else {
            let kstr = sval(&key);
            let kv = with_host(|h| h.new_str(kstr.clone()));
            d.insert(PKey::Str(kstr), (kv, val));
        }
        i += 2;
    }
    with_host(|h| h.new_dict(d))
}

/// Build a dict from `{**a, k: v}` literal entries: triples `(tag, a, b)` where
/// tag 1 = `**` spread of `a` (b unused), tag 0 = plain `(key a, val b)`.
fn b_mkdict_ex(vm: &mut VM, argc: u8) -> Value {
    let flat = pop_n(vm, argc as usize);
    let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
    let mut i = 0;
    while i + 2 < flat.len() {
        let spread = matches!(flat[i], Value::Int(1));
        if spread {
            let m = flat[i + 1].clone();
            let pairs = with_host(|h| match h.get(&m) {
                Some(PyObj::Dict(map)) => map
                    .iter()
                    .map(|(k, (kv, v))| (k.clone(), kv.clone(), v.clone()))
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            });
            for (k, kv, v) in pairs {
                host::dict_put(&mut d, k, kv, v);
            }
        } else {
            let k = flat[i + 1].clone();
            let v = flat[i + 2].clone();
            match with_host(|h| h.to_key(&k)) {
                Ok(key) => {
                    host::dict_put(&mut d, key, k, v);
                }
                Err(e) => return abort(vm, e),
            }
        }
        i += 3;
    }
    with_host(|h| h.new_dict(d))
}

// ── truthiness / str / format ────────────────────────────────────────────────

/// Python truthiness with instance dunder dispatch: `__bool__`, else `__len__`,
/// else the host's structural truthiness. Used by the TRUTHY op, `bool()`,
/// `any`/`all`/`filter`.
fn py_bool(v: &Value) -> Result<bool, String> {
    let (has_bool, has_len) = with_host(|h| match h.get(v) {
        Some(PyObj::Instance(i)) => (
            instance_has(h, i, "__bool__"),
            instance_has(h, i, "__len__"),
        ),
        _ => (false, false),
    });
    if has_bool {
        let x = host::call_method(v, "__bool__", vec![], vec![])?;
        return Ok(with_host(|h| h.truthy(&x)));
    }
    if has_len {
        let x = host::call_method(v, "__len__", vec![], vec![])?;
        return Ok(with_host(|h| h.truthy(&x)));
    }
    Ok(with_host(|h| h.truthy(v)))
}

fn b_truthy(vm: &mut VM, _: u8) -> Value {
    let v = vm.pop();
    match py_bool(&v) {
        Ok(b) => Value::Bool(b),
        Err(e) => abort(vm, e),
    }
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

/// Format one replacement field: apply the `!r`/`!s`/`!a` conversion (codes
/// 2/1/3, 0 = none), honor an instance's `__format__(spec)`, then apply the
/// format spec. Shared by f-strings (`ops::FORMAT`) and `str.format`.
fn format_field(v: &Value, conv: i64, spec: &str) -> Result<String, String> {
    // A conversion turns the value into a string first; `__format__` is bypassed.
    if conv != 0 {
        let s = match conv {
            2 => py_repr(v)?,                  // !r
            3 => host::ascii_of(&py_repr(v)?), // !a (ascii-escaped repr)
            _ => py_str(v)?,                   // !s
        };
        let sv = with_host(|h| h.new_str(s.clone()));
        return Ok(apply_format_spec(&s, &sv, spec));
    }
    // No conversion: an instance's `__format__(spec)` wins outright.
    let has_format = with_host(|h| match h.get(v) {
        Some(PyObj::Instance(i)) => h.class_lookup(&i.class, "__format__").is_some(),
        _ => false,
    });
    if has_format {
        let specv = with_host(|h| h.new_str(spec.to_string()));
        let r = host::call_method(v, "__format__", vec![specv], vec![])?;
        return Ok(with_host(|h| h.str_of(&r)));
    }
    let s = py_str(v)?;
    Ok(apply_format_spec(&s, v, spec))
}

fn b_format(vm: &mut VM, _: u8) -> Value {
    let spec = sval(&vm.pop());
    let conv = match vm.pop() {
        Value::Int(n) => n,
        _ => 0,
    };
    let v = vm.pop();
    match format_field(&v, conv, &spec) {
        Ok(out) => with_host(|h| h.new_str(out)),
        Err(e) => abort(vm, e),
    }
}

// ── functions / classes ──────────────────────────────────────────────────────

fn b_mkfunc(vm: &mut VM, argc: u8) -> Value {
    // Stack layout (bottom→top): pos_defaults…, kw_defaults…, kw_count, func_id.
    let mut args = pop_n(vm, argc as usize);
    let def_id = match args.pop() {
        Some(Value::Int(n)) => n as usize,
        _ => return abort(vm, "internal: MKFUNC without func id".into()),
    };
    let nkw = match args.pop() {
        Some(Value::Int(n)) => n as usize,
        _ => return abort(vm, "internal: MKFUNC without kwonly-default count".into()),
    };
    let split = args.len().saturating_sub(nkw);
    let kwonly_defaults = args.split_off(split);
    let defaults = args; // remaining are positional defaults, in order
    let env = with_host(|h| h.current_env_capture());
    with_host(|h| {
        h.alloc(PyObj::Func(host::FuncVal {
            def_id,
            env: Some(env),
            defaults,
            kwonly_defaults,
            bound: None,
            owner: None,
        }))
    })
}

fn b_build_class(vm: &mut VM, _: u8) -> Value {
    let kwargs_val = vm.pop();
    let body_func = vm.pop();
    let name = sval(&vm.pop());
    let bases_val = vm.pop();
    let metaclass = vm.pop();
    let bases: Vec<String> = with_host(|h| match h.get(&bases_val) {
        Some(PyObj::List(l)) => l.iter().filter_map(|b| callable_name(h, b)).collect(),
        _ => Vec::new(),
    });
    // An explicit `metaclass=` that names a user class drives construction.
    let meta_name = match &metaclass {
        Value::Undef => None,
        _ => with_host(|h| callable_name(h, &metaclass)),
    };
    // The class-header keywords (minus `metaclass`), forwarded to
    // `__init_subclass__` as `(name, value)` pairs in definition order.
    let class_kwargs: Vec<(String, Value)> = with_host(|h| match h.get(&kwargs_val) {
        Some(PyObj::Dict(d)) => d.values().map(|(k, v)| (h.str_of(k), v.clone())).collect(),
        _ => Vec::new(),
    });
    let r = host::build_class(&name, bases, &body_func, meta_name, class_kwargs);
    finish(vm, r)
}

// ── iteration ────────────────────────────────────────────────────────────────

fn b_getiter(vm: &mut VM, _: u8) -> Value {
    let v = vm.pop();
    // On an instance, prefer a *lazy* __iter__ result; else materialize via the
    // __iter__/__next__ or __getitem__ protocol (host::iter_instance_items).
    if with_host(|h| matches!(h.get(&v), Some(PyObj::Instance(_)))) {
        let r = iter_instance(&v);
        return finish(vm, r);
    }
    let r = with_host(|h| h.make_iter(&v));
    finish(vm, r)
}

/// Drive a user iterable into a concrete seq iterator. A `__iter__` that returns
/// a native iterator is used directly (stays lazy); everything else materializes
/// through the shared `__iter__`/`__next__`/`__getitem__` protocol.
fn iter_instance(v: &Value) -> Result<Value, String> {
    let has_iter = with_host(|h| match h.get(v) {
        Some(PyObj::Instance(i)) => h.class_lookup(&i.class, "__iter__").is_some(),
        _ => false,
    });
    if has_iter {
        let it = host::call_method(v, "__iter__", vec![], vec![])?;
        if with_host(|h| {
            matches!(
                h.get(&it),
                Some(PyObj::Iter(_)) | Some(PyObj::Generator { .. })
            )
        }) {
            return Ok(it);
        }
    }
    let items = host::iter_instance_items(v)?;
    Ok(with_host(|h| {
        h.alloc(PyObj::Iter(IterState::Seq { items, idx: 0 }))
    }))
}

/// `yield from` result: pop the delegated iterator and push the value it
/// `return`ed (a generator's `StopIteration.value`), or `None` otherwise.
fn b_genret(vm: &mut VM, _: u8) -> Value {
    let it = vm.pop();
    with_host(|h| match h.get(&it) {
        Some(PyObj::Generator { id }) => h.gen_return_value(*id),
        _ => Value::Undef,
    })
}

fn b_foriter(vm: &mut VM, _: u8) -> Value {
    let it = match vm.stack.last() {
        Some(v) => v.clone(),
        None => return abort(vm, "internal: FORITER with empty stack".into()),
    };
    match host::iter_step(&it) {
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
    // Instance `__contains__` wins; else fall back to iterating the instance
    // (via __iter__/__getitem__) and comparing.
    if with_host(|h| matches!(h.get(&container), Some(PyObj::Instance(_)))) {
        let has_contains = with_host(|h| match h.get(&container) {
            Some(PyObj::Instance(i)) => h.class_lookup(&i.class, "__contains__").is_some(),
            _ => false,
        });
        if has_contains {
            let r = host::call_method(&container, "__contains__", vec![item], vec![]);
            return match r {
                Ok(v) => Value::Bool(with_host(|h| h.truthy(&v))),
                Err(e) => abort(vm, e),
            };
        }
        return match iter_membership(&container, &item) {
            Ok(b) => Value::Bool(b),
            Err(e) => abort(vm, e),
        };
    }
    // A generator is consumed to test membership (no host borrow held).
    if with_host(|h| matches!(h.get(&container), Some(PyObj::Generator { .. }))) {
        return match host::iter_vec(&container) {
            Ok(items) => Value::Bool(with_host(|h| items.iter().any(|x| h.equal(x, &item)))),
            Err(e) => abort(vm, e),
        };
    }
    let cands = host::instance_key_candidates(&container);
    let r = host::with_instance_key(&item, &cands, || {
        with_host(|h| h.contains(&item, &container))
    });
    match r {
        Ok(b) => Value::Bool(b),
        Err(e) => abort(vm, e),
    }
}

/// Materialize an instance iterable and test whether `item` is a member (the
/// `in` fallback when no `__contains__` is defined).
fn iter_membership(container: &Value, item: &Value) -> Result<bool, String> {
    let items = host::iter_instance_items(container)?;
    Ok(with_host(|h| items.iter().any(|x| h.equal(x, item))))
}

fn b_is(vm: &mut VM, _: u8) -> Value {
    let b = vm.pop();
    let a = vm.pop();
    let same = match (&a, &b) {
        (Value::Obj(x), Value::Obj(y)) => {
            // Type / builtin objects are conceptual singletons: `type(5) is int`
            // and `type(b) is B` hold even across distinct heap allocations.
            x == y
                || with_host(|h| match (h.get(&a), h.get(&b)) {
                    (Some(PyObj::Class(m)), Some(PyObj::Class(n))) => m == n,
                    (Some(PyObj::Builtin(m)), Some(PyObj::Builtin(n))) => m == n,
                    _ => false,
                })
        }
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

fn b_raise(vm: &mut VM, argc: u8) -> Value {
    let exc = vm.pop();
    // `raise E from C` pushes cause under exc.
    let cause = if argc >= 2 { vm.pop() } else { Value::Undef };
    // The exception currently being handled (set by `b_try`) becomes the new
    // exception's implicit `__context__`. Capture it before `raise_value`
    // overwrites `h.exc` with the freshly-raised object.
    let context = with_host(|h| h.exc.clone().unwrap_or(Value::Undef));
    match host::raise_value(&exc) {
        Ok(msg) => {
            with_host(|h| {
                if let Some(new_exc) = h.exc.clone() {
                    let ctx = match &context {
                        Value::Obj(_) if new_exc != context => context.clone(),
                        _ => Value::Undef,
                    };
                    h.set_exc_link(&new_exc, cause.clone(), ctx);
                }
            });
            abort(vm, msg)
        }
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
    // Under `--dap` the debugger pauses here at each statement boundary; a normal
    // run's hook is a no-op (returns immediately).
    crate::dap::on_debug_line(vm);
    Value::Undef
}

// ── binary / unary operators ─────────────────────────────────────────────────

/// Whether `v` is a user instance whose class defines method `name` — the guard
/// for operator-overloading dunder dispatch.
fn is_instance_with(h: &host::PyHost, v: &Value, name: &str) -> bool {
    matches!(h.get(v), Some(PyObj::Instance(i)) if instance_has(h, i, name))
}

/// Is `v` the `NotImplemented` singleton?
fn is_not_implemented(v: &Value) -> bool {
    with_host(|h| matches!(h.get(v), Some(PyObj::NotImplemented)))
}

/// The outcome of dispatching a binary/comparison dunder pair.
enum Dunder {
    /// A concrete result the dunder produced.
    Value(Value),
    /// Both operands declined (no dunder, or all returned `NotImplemented`).
    NotImplemented,
    Err(String),
}

/// Dispatch the forward/reflected dunder pair, honoring `NotImplemented`: try
/// `a.lname(b)` then `b.rname(a)`, skipping any that return the `NotImplemented`
/// singleton. Only instance operands are consulted; a `NotImplemented` outcome
/// means the caller should fall back (native op, identity, or `TypeError`).
fn dispatch_binop(a: &Value, b: &Value, lname: &str, rname: &str) -> Dunder {
    if with_host(|h| is_instance_with(h, a, lname)) {
        match host::call_method(a, lname, vec![b.clone()], vec![]) {
            Ok(v) if is_not_implemented(&v) => {}
            Ok(v) => return Dunder::Value(v),
            Err(e) => return Dunder::Err(e),
        }
    }
    if with_host(|h| is_instance_with(h, b, rname)) {
        match host::call_method(b, rname, vec![a.clone()], vec![]) {
            Ok(v) if is_not_implemented(&v) => {}
            Ok(v) => return Dunder::Value(v),
            Err(e) => return Dunder::Err(e),
        }
    }
    Dunder::NotImplemented
}

/// Python operator overloading for the non-native `BINOP` tags (`//`, `%`, `&`,
/// …): dispatch dunders, or `None` to fall through to native handling when
/// neither operand overloads. On both-declined (`NotImplemented`) with an
/// instance operand, raise the unsupported-operand `TypeError`.
fn try_binop_dunder(
    a: &Value,
    b: &Value,
    lname: &str,
    rname: &str,
) -> Option<Result<Value, String>> {
    // `str % obj` is native string formatting (`str.__mod__`), which is
    // authoritative — CPython never returns `NotImplemented` from it, so the
    // right operand's `__rmod__` is never consulted. Route straight to native.
    if lname == "__mod__"
        && with_host(|h| matches!(a, Value::Str(_)) || matches!(h.get(a), Some(PyObj::Str(_))))
    {
        return None;
    }
    let involved = with_host(|h| {
        matches!(h.get(a), Some(PyObj::Instance(_))) || matches!(h.get(b), Some(PyObj::Instance(_)))
    });
    if !involved {
        return None;
    }
    match dispatch_binop(a, b, lname, rname) {
        Dunder::Value(v) => Some(Ok(v)),
        Dunder::Err(e) => Some(Err(e)),
        Dunder::NotImplemented => {
            let sym = binop_symbol(lname);
            Some(Err(unsupported_operand(sym, a, b)))
        }
    }
}

/// The operator glyph for an unsupported-operand `TypeError` message.
fn binop_symbol(lname: &str) -> &'static str {
    match lname {
        "__add__" => "+",
        "__sub__" => "-",
        "__mul__" => "*",
        "__truediv__" => "/",
        "__floordiv__" => "//",
        "__mod__" => "%",
        "__pow__" => "** or pow()",
        "__matmul__" => "@",
        "__and__" => "&",
        "__or__" => "|",
        "__xor__" => "^",
        "__lshift__" => "<<",
        "__rshift__" => ">>",
        _ => "?",
    }
}

fn unsupported_operand(sym: &str, a: &Value, b: &Value) -> String {
    with_host(|h| {
        host::type_error(&format!(
            "unsupported operand type(s) for {sym}: '{}' and '{}'",
            h.type_name(a),
            h.type_name(b)
        ))
    })
}

/// Identity comparison (mirrors the `is` operator) for the `==`/`!=` fallback.
fn identity_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Obj(x), Value::Obj(y)) => x == y,
        (Value::Undef, Value::Undef) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        _ => false,
    }
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
    // `str % args`: pre-resolve any instance / instance-bearing container's
    // dispatched str()/repr()/ascii() OUTSIDE the host borrow (the host `%`
    // formatter runs inside the borrow and cannot call back into __str__/__repr__),
    // then format with the pre-resolved table. Covers `%` and `%=` (desugared).
    if tag == host::binop::MOD && with_host(|h| matches!(h.get(&a), Some(PyObj::Str(_)))) {
        let r = str_percent_format(&a, &b);
        return finish(vm, r);
    }
    let r = with_host(|h| h.binop(tag, &a, &b));
    finish(vm, r)
}

/// `str % args` with instance-aware `%s`/`%r`/`%a`. Builds a dispatch table of
/// pre-resolved `(str, repr, ascii)` for every user instance / instance-bearing
/// container among the top-level format args (computed here, outside any host
/// borrow, so `__str__`/`__repr__` can fire), then hands it to the host formatter.
fn str_percent_format(fmt_val: &Value, args: &Value) -> Result<Value, String> {
    // The top-level format arguments: tuple elements, mapping values, or the
    // bare single arg.
    let items: Vec<Value> = with_host(|h| match h.get(args) {
        Some(PyObj::Tuple(t)) => t.clone(),
        Some(PyObj::Dict(d)) => d.values().map(|(_, v)| v.clone()).collect(),
        _ => vec![args.clone()],
    });
    let mut premap: std::collections::HashMap<u32, (String, String, String)> =
        std::collections::HashMap::new();
    for it in &items {
        let Value::Obj(id) = it else { continue };
        if premap.contains_key(id) {
            continue;
        }
        // Only instances and containers that may hold instances need the
        // dispatching path; everything else the host renders correctly itself.
        let needs = with_host(|h| {
            matches!(
                h.get(it),
                Some(PyObj::Instance(_))
                    | Some(PyObj::List(_))
                    | Some(PyObj::Tuple(_))
                    | Some(PyObj::Dict(_))
                    | Some(PyObj::Set(_))
                    | Some(PyObj::Frozenset(_))
            )
        });
        if !needs {
            continue;
        }
        let s = py_str(it)?;
        let r = py_repr(it)?;
        let a = host::ascii_of(&r);
        premap.insert(*id, (s, r, a));
    }
    let fmt = with_host(|h| match h.get(fmt_val) {
        Some(PyObj::Str(s)) => s.clone(),
        _ => String::new(),
    });
    with_host(|h| h.str_format_percent(&fmt, args, &premap))
}

fn b_unary(vm: &mut VM, _: u8) -> Value {
    let v = vm.pop();
    let tag = match vm.pop() {
        Value::Int(n) => n,
        _ => return abort(vm, "internal: UNARY tag".into()),
    };
    // Instance overloading: `~x` → __invert__, unary `+x` → __pos__.
    let dunder = match tag {
        host::unop::INVERT => "__invert__",
        host::unop::POS => "__pos__",
        _ => "",
    };
    if !dunder.is_empty()
        && with_host(
            |h| matches!(h.get(&v), Some(PyObj::Instance(i)) if instance_has(h, i, dunder)),
        )
    {
        let r = host::call_method(&v, dunder, vec![], vec![]);
        return finish(vm, r);
    }
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
    let items = match host::iter_vec(&iterable) {
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
                    // Clear the propagating-error state but keep the caught
                    // exception as the "currently handled" one, so a bare `raise`
                    // in the handler body re-raises it (`b_reraise` reads `h.exc`).
                    // The exception is caught: the frames it unwound past are no
                    // longer part of an uncaught trace, so discard them.
                    with_host(|h| {
                        h.error = None;
                        h.exc = Some(exc.clone());
                        h.traceback.clear();
                    });
                    let hres = host::run_chunk_on(hbody.clone());
                    match hres {
                        Ok(_) => with_host(|h| {
                            // Handler finished without raising — clear the handled
                            // exception (unless the body set a return/break signal).
                            if h.signal.is_none() {
                                h.exc = None;
                            }
                        }),
                        Err(e2) => pending = Some(e2),
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

// ── match / case structural helpers ───────────────────────────────────────────

/// `[subject, count, star]` -> on match push the `count` destructured elements
/// as a `list` then `Bool(true)`; on mismatch just `Bool(false)`. Sequence
/// patterns match `list`/`tuple` (not str/bytes/dict/set), mirroring PEP 634.
fn b_match_seq(vm: &mut VM, _: u8) -> Value {
    let star = match vm.pop() {
        Value::Int(n) => n,
        _ => -1,
    };
    let count = match vm.pop() {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let subject = vm.pop();
    let items = with_host(|h| match h.get(&subject) {
        Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => Some(l.clone()),
        _ => None,
    });
    let items = match items {
        Some(v) => v,
        None => return Value::Bool(false),
    };
    let ordered: Vec<Value> = if star < 0 {
        if items.len() != count {
            return Value::Bool(false);
        }
        items
    } else {
        let si = star as usize;
        let after = count - si - 1;
        if items.len() < si + after {
            return Value::Bool(false);
        }
        let mid = items[si..items.len() - after].to_vec();
        let mid_list = with_host(|h| h.new_list(mid));
        let mut out = Vec::with_capacity(count);
        out.extend_from_slice(&items[..si]);
        out.push(mid_list);
        out.extend_from_slice(&items[items.len() - after..]);
        out
    };
    let list = with_host(|h| h.new_list(ordered));
    vm.push(list);
    Value::Bool(true)
}

/// `[subject]` -> `Bool` (is a mapping — a dict).
fn b_match_map_check(vm: &mut VM, _: u8) -> Value {
    let subject = vm.pop();
    Value::Bool(with_host(|h| {
        matches!(h.get(&subject), Some(PyObj::Dict(_)))
    }))
}

/// `[subject, key]` -> on hit push `value` then `Bool(true)`; else `Bool(false)`.
fn b_match_key(vm: &mut VM, _: u8) -> Value {
    let key = vm.pop();
    let subject = vm.pop();
    let k = match with_host(|h| h.to_key(&key)) {
        Ok(k) => k,
        Err(e) => return abort(vm, e),
    };
    let got = with_host(|h| match h.get(&subject) {
        Some(PyObj::Dict(d)) => d.get(&k).map(|(_, v)| v.clone()),
        _ => None,
    });
    match got {
        Some(v) => {
            vm.push(v);
            Value::Bool(true)
        }
        None => Value::Bool(false),
    }
}

/// `[subject, keylist]` -> a new dict of `subject` minus the matched keys.
fn b_match_map_rest(vm: &mut VM, _: u8) -> Value {
    let keylist = vm.pop();
    let subject = vm.pop();
    let keys = match host::iter_vec(&keylist) {
        Ok(ks) => ks,
        Err(e) => return abort(vm, e),
    };
    let mut d = with_host(|h| match h.get(&subject) {
        Some(PyObj::Dict(d)) => d.clone(),
        _ => IndexMap::new(),
    });
    for kv in &keys {
        if let Ok(k) = with_host(|h| h.to_key(kv)) {
            d.shift_remove(&k);
        }
    }
    with_host(|h| h.new_dict(d))
}

/// `[subject, class, npos, kwnames...]` -> on match push extracted sub-values
/// (positional via `__match_args__` / builtin self-match, then keyword via
/// attributes) as a `list`, then `Bool(true)`; else `Bool(false)`.
fn b_match_class(vm: &mut VM, argc: u8) -> Value {
    let all = pop_n(vm, argc as usize);
    if all.len() < 3 {
        return abort(vm, "internal: MATCH_CLASS arity".into());
    }
    let subject = all[0].clone();
    let class = all[1].clone();
    let npos = match all[2] {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let kwnames: Vec<String> = all[3..].iter().map(sval).collect();
    if !with_host(|h| isinstance(h, &subject, &class)) {
        return Value::Bool(false);
    }
    let cname = with_host(|h| callable_name(h, &class)).unwrap_or_default();
    let mut vals: Vec<Value> = Vec::new();
    if npos > 0 {
        if is_builtin_type(&cname) {
            // Builtin types (int, str, …) allow a single positional self-match.
            vals.push(subject.clone());
        } else {
            let margs = with_host(|h| h.class_lookup(&cname, "__match_args__"));
            let names: Vec<String> = match margs {
                Some(v) => match host::iter_vec(&v) {
                    Ok(items) => items.iter().map(sval).collect(),
                    Err(e) => return abort(vm, e),
                },
                None => {
                    return abort(
                        vm,
                        host::type_error(&format!(
                            "{cname}() accepts 0 positional sub-patterns ({npos} given)"
                        )),
                    )
                }
            };
            for i in 0..npos {
                let attr = match names.get(i) {
                    Some(a) => a.clone(),
                    None => return Value::Bool(false),
                };
                match with_host(|h| h.get_attr(&subject, &attr)) {
                    Ok(v) => vals.push(v),
                    Err(_) => return Value::Bool(false),
                }
            }
        }
    }
    for name in &kwnames {
        match with_host(|h| h.get_attr(&subject, name)) {
            Ok(v) => vals.push(v),
            Err(_) => return Value::Bool(false),
        }
    }
    let list = with_host(|h| h.new_list(vals));
    vm.push(list);
    Value::Bool(true)
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
        Some(PyObj::NamedTupleType { type_name, .. }) => Some(type_name.clone()),
        _ => None,
    }
}

// ── the strict numeric hook ──────────────────────────────────────────────────

/// Python arithmetic/comparison for operands the VM can't handle natively. User
/// instances defining an operator dunder (`__add__`, `__eq__`, `__lt__`, …) are
/// dispatched first; everything else falls to the host's native numeric logic.
pub fn numeric_hook(op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
    use NumOp::*;
    let a_inst = with_host(|h| matches!(h.get(a), Some(PyObj::Instance(_))));
    let b_inst = with_host(|h| matches!(h.get(b), Some(PyObj::Instance(_))));
    // No user instance involved → native handling (preserves `1 == 1.0`, etc.).
    if !a_inst && !b_inst {
        return with_host(|h| h.arith(op, a, b));
    }
    match op {
        Eq => match dispatch_binop(a, b, "__eq__", "__eq__") {
            Dunder::Value(v) => Ok(v),
            Dunder::Err(e) => Err(e),
            Dunder::NotImplemented => Ok(Value::Bool(identity_eq(a, b))),
        },
        Ne => match dispatch_binop(a, b, "__ne__", "__ne__") {
            Dunder::Value(v) => Ok(v),
            Dunder::Err(e) => Err(e),
            // Default `__ne__` derives from `__eq__` (inverting its truthiness).
            Dunder::NotImplemented => match dispatch_binop(a, b, "__eq__", "__eq__") {
                Dunder::Value(v) => Ok(Value::Bool(!with_host(|h| h.truthy(&v)))),
                Dunder::Err(e) => Err(e),
                Dunder::NotImplemented => Ok(Value::Bool(!identity_eq(a, b))),
            },
        },
        Lt | Le | Gt | Ge => {
            let (l, r) = numop_dunders(op).unwrap();
            match dispatch_binop(a, b, l, r) {
                Dunder::Value(v) => Ok(v),
                Dunder::Err(e) => Err(e),
                Dunder::NotImplemented => {
                    let sym = match op {
                        Lt => "<",
                        Le => "<=",
                        Gt => ">",
                        _ => ">=",
                    };
                    Err(with_host(|h| {
                        host::type_error(&format!(
                            "'{sym}' not supported between instances of '{}' and '{}'",
                            h.type_name(a),
                            h.type_name(b)
                        ))
                    }))
                }
            }
        }
        // Unary negation reaches the hook with the operand in `a`; an instance
        // defining `__neg__` overloads it.
        Neg => {
            if a_inst
                && with_host(
                    |h| matches!(h.get(a), Some(PyObj::Instance(i)) if instance_has(h, i, "__neg__")),
                )
            {
                host::call_method(a, "__neg__", vec![], vec![])
            } else {
                with_host(|h| h.arith(op, a, b))
            }
        }
        // Arithmetic: forward/reflected dunder, else unsupported-operand TypeError.
        Add | Sub | Mul | Div | Mod | Pow => {
            let (l, r) = numop_dunders(op).unwrap();
            match dispatch_binop(a, b, l, r) {
                Dunder::Value(v) => Ok(v),
                Dunder::Err(e) => Err(e),
                Dunder::NotImplemented => Err(unsupported_operand(binop_symbol(l), a, b)),
            }
        }
    }
}

// ── builtin predicates ───────────────────────────────────────────────────────

pub fn is_builtin_function(name: &str) -> bool {
    BUILTIN_FUNCS.contains(&name)
        || name.starts_with("math.")
        || name.starts_with("collections.")
        || name.starts_with("textwrap.")
        || name.starts_with("statistics.")
        || name.starts_with("asyncio.")
}

/// Whether `name` is a builtin *type* (`int`, `list`, …) as opposed to a builtin
/// function — controls `<class 'X'>` vs `<built-in function X>` repr.
pub fn is_builtin_type_name(name: &str) -> bool {
    is_builtin_type(name)
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
            | "bytearray"
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
    "open",
    "super",
    "staticmethod",
    "classmethod",
    "property",
    "exit",
    "quit",
];

// ── builtin functions ────────────────────────────────────────────────────────

/// str()/repr() with instance dunder dispatch (free-function form).
fn py_str(v: &Value) -> Result<String, String> {
    if with_host(|h| matches!(h.get(v), Some(PyObj::Instance(_)))) {
        let (has_str, has_repr, is_exc) = with_host(|h| match h.get(v) {
            Some(PyObj::Instance(i)) => (
                h.class_lookup(&i.class, "__str__").is_some(),
                h.class_lookup(&i.class, "__repr__").is_some(),
                h.class_is_exception(&i.class),
            ),
            _ => (false, false, false),
        });
        if has_str {
            let r = host::call_method(v, "__str__", vec![], vec![])?;
            return Ok(with_host(|h| h.str_of(&r)));
        }
        // `BaseException.__str__` (the message) wins over a user `__repr__` for
        // exception instances — CPython never falls str→repr for these.
        if is_exc {
            return Ok(with_host(|h| h.str_of(v)));
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
                | Some(PyObj::Frozenset(_))
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
        // A tuple, plus `(type_name, field_names)` when it is a namedtuple.
        Tuple(Vec<Value>, Option<(String, Vec<String>)>),
        Set(Vec<Value>),
        Frozenset(Vec<Value>),
        Dict(Vec<(Value, Value)>),
    }
    let cont = with_host(|h| match h.get(v) {
        Some(PyObj::List(l)) => Some(Cont::List(l.clone())),
        Some(PyObj::Tuple(l)) => {
            let nt = match v {
                Value::Obj(i) => h
                    .nt_meta
                    .get(i)
                    .map(|m| (m.type_name.clone(), m.fields.clone())),
                _ => None,
            };
            Some(Cont::Tuple(l.clone(), nt))
        }
        // Element order follows CPython's set hash-table layout (int subset).
        Some(PyObj::Set(s)) => Some(Cont::Set(h.set_ordered_values(s))),
        Some(PyObj::Frozenset(s)) => Some(Cont::Frozenset(h.set_ordered_values(s))),
        Some(PyObj::Dict(d)) => Some(Cont::Dict(d.values().cloned().collect())),
        _ => None,
    });
    let reprs =
        |elems: &[Value]| -> Result<Vec<String>, String> { elems.iter().map(py_repr).collect() };
    if let Some(cont) = cont {
        return Ok(match cont {
            Cont::List(e) => format!("[{}]", reprs(&e)?.join(", ")),
            Cont::Tuple(e, Some((type_name, fields))) => {
                let p = reprs(&e)?;
                let inner: Vec<String> = fields
                    .iter()
                    .zip(p.iter())
                    .map(|(f, x)| format!("{f}={x}"))
                    .collect();
                format!("{type_name}({})", inner.join(", "))
            }
            Cont::Tuple(e, None) => {
                let p = reprs(&e)?;
                if p.len() == 1 {
                    format!("({},)", p[0])
                } else {
                    format!("({})", p.join(", "))
                }
            }
            Cont::Set(e) if e.is_empty() => "set()".into(),
            Cont::Set(e) => format!("{{{}}}", reprs(&e)?.join(", ")),
            Cont::Frozenset(e) if e.is_empty() => "frozenset()".into(),
            Cont::Frozenset(e) => format!("frozenset({{{}}})", reprs(&e)?.join(", ")),
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
    // sys.* module functions.
    if let Some(f) = name.strip_prefix("sys.") {
        return call_sys(f, args);
    }
    // asyncio.* module functions (native event loop / futures).
    if let Some(f) = name.strip_prefix("asyncio.") {
        return call_asyncio(f, args, kwargs);
    }
    // `dict.fromkeys(iterable[, value])` reached via the `dict` type object.
    if name == "dict.fromkeys" {
        return dict_method(&Value::Undef, "fromkeys", &args, &[]);
    }
    // `str.maketrans(...)` reached via the `str` type object.
    if name == "str.maketrans" {
        return str_maketrans(&args);
    }
    // `bytes.fromhex(...)` / `bytearray.fromhex(...)` via the type object.
    if name == "bytes.fromhex" || name == "bytearray.fromhex" {
        let b = bytes_fromhex(&args)?;
        return Ok(with_host(|h| {
            if name == "bytearray.fromhex" {
                h.alloc(PyObj::Bytearray(b))
            } else {
                h.alloc(PyObj::Bytes(b))
            }
        }));
    }
    // Native stdlib module functions (src/stdlib). These take `&mut PyHost`.
    if let Some(f) = name.strip_prefix("textwrap.") {
        if let Some(r) = with_host(|h| crate::stdlib::textwrap::call(h, f, &args)) {
            return r;
        }
    }
    if let Some(f) = name.strip_prefix("statistics.") {
        if let Some(r) = with_host(|h| crate::stdlib::statistics::call(h, f, &args)) {
            return r;
        }
    }
    // collections constructors (host-backed types).
    if let Some(f) = name.strip_prefix("collections.") {
        return construct_collection(f, args, kwargs);
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
            let out = format!("{}{}", parts.join(&sep), end);
            // `file=` routes to a file/stream object (e.g. `sys.stderr`); the
            // default and an explicit `sys.stdout` go to stdout.
            let file_id = kw_get(&kwargs, "file")
                .filter(|f| !matches!(f, Value::Undef))
                .and_then(|f| with_host(|h| h.file_id(&f)));
            match file_id {
                Some(id) if id != 0 => {
                    with_host(|h| h.io_write(id, &out))?;
                }
                _ => {
                    use std::io::Write;
                    let _ = std::io::stdout().write_all(out.as_bytes());
                    let _ = std::io::stdout().flush();
                }
            }
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
            // Instance overloading: `abs(x)` → `x.__abs__()`.
            if with_host(
                |h| matches!(h.get(&v), Some(PyObj::Instance(i)) if instance_has(h, i, "__abs__")),
            ) {
                return host::call_method(&v, "__abs__", vec![], vec![]);
            }
            with_host(|h| match &v {
                Value::Int(n) => Ok(Value::Int(n.abs())),
                Value::Float(f) => Ok(Value::Float(f.abs())),
                Value::Bool(b) => Ok(Value::Int(*b as i64)),
                Value::Obj(_) if matches!(h.get(&v), Some(PyObj::BigInt(_))) => match h.get(&v) {
                    Some(PyObj::BigInt(b)) => {
                        let b = b.clone();
                        Ok(h.norm_big(b.magnitude().clone().into()))
                    }
                    _ => unreachable!(),
                },
                Value::Obj(_) if matches!(h.get(&v), Some(PyObj::Complex(..))) => match h.get(&v) {
                    Some(PyObj::Complex(r, i)) => Ok(Value::Float(r.hypot(*i))),
                    _ => unreachable!(),
                },
                // A CPython `Foreign` object (stdlib-ffi): `abs(Decimal(...))`,
                // `abs(timedelta(...))`, … dispatch to the real `__abs__`.
                #[cfg(feature = "stdlib-ffi")]
                Value::Obj(_) if h.foreign_id(&v).is_some() => crate::ffi::unary_op(h, "abs", &v),
                _ => Err(host::type_error(&format!(
                    "bad operand type for abs(): '{}'",
                    h.type_name(&v)
                ))),
            })
        }
        "min" => reduce_minmax(&args, &kwargs, false),
        "max" => reduce_minmax(&args, &kwargs, true),
        "sum" => {
            let items = host::iter_vec(&arg0(&args)?)?;
            let mut acc = args.get(1).cloned().unwrap_or(Value::Int(0));
            for it in items {
                acc = with_host(|h| h.arith(NumOp::Add, &acc, &it))?;
            }
            Ok(acc)
        }
        "sorted" => py_sorted(&args, &kwargs),
        "reversed" => {
            let v = arg0(&args)?;
            // Instance `__reversed__` wins; else `__getitem__`+`__len__` reverse.
            if with_host(|h| matches!(h.get(&v), Some(PyObj::Instance(_)))) {
                let (has_rev, has_gi) = with_host(|h| match h.get(&v) {
                    Some(PyObj::Instance(i)) => (
                        h.class_lookup(&i.class, "__reversed__").is_some(),
                        h.class_lookup(&i.class, "__getitem__").is_some()
                            && h.class_lookup(&i.class, "__len__").is_some(),
                    ),
                    _ => (false, false),
                });
                if has_rev {
                    return host::call_method(&v, "__reversed__", vec![], vec![]);
                }
                if has_gi {
                    let n = py_len(&v)?;
                    let mut items = Vec::with_capacity(n);
                    for i in (0..n as i64).rev() {
                        items.push(host::call_method(
                            &v,
                            "__getitem__",
                            vec![Value::Int(i)],
                            vec![],
                        )?);
                    }
                    return Ok(with_host(|h| h.new_iter_seq(items)));
                }
                return Err(host::type_error(&format!(
                    "'{}' object is not reversible",
                    with_host(|h| h.type_name(&v))
                )));
            }
            // `reversed` requires a known-length sequence (never infinite), so a
            // materialize-then-reverse is still lazy in the observable sense: the
            // result is a one-shot iterator (`next()` works, exhausts once).
            let mut items = host::iter_vec(&v)?;
            items.reverse();
            Ok(with_host(|h| h.new_iter_seq(items)))
        }
        "enumerate" => {
            // Lazy: pairs `(index, value)` pulled on demand. `start=` kwarg or
            // positional second arg sets the initial index.
            let source = with_host(|h| h.make_iter(&arg0(&args)?))?;
            let start = kw_get(&kwargs, "start")
                .or_else(|| args.get(1).cloned())
                .and_then(|v| with_host(|h| h.as_int(&v)))
                .unwrap_or(0);
            Ok(with_host(|h| {
                h.alloc(PyObj::EnumerateObj {
                    source,
                    next: start,
                    done: false,
                })
            }))
        }
        "zip" => {
            // Lazy `zip`: one iterator per argument, tuple pulled on demand.
            let mut sources = Vec::with_capacity(args.len());
            for a in &args {
                sources.push(with_host(|h| h.make_iter(a))?);
            }
            let strict = kw_get(&kwargs, "strict")
                .map(|v| with_host(|h| h.truthy(&v)))
                .unwrap_or(false);
            Ok(with_host(|h| {
                h.alloc(PyObj::Zip {
                    sources,
                    strict,
                    done: false,
                })
            }))
        }
        "map" => {
            // Lazy `map`: `func` applied to items pulled from each iterable.
            let f = arg0(&args)?;
            let mut sources = Vec::with_capacity(args.len().saturating_sub(1));
            for a in &args[1..] {
                sources.push(with_host(|h| h.make_iter(a))?);
            }
            Ok(with_host(|h| {
                h.alloc(PyObj::MapObj {
                    func: f,
                    sources,
                    done: false,
                })
            }))
        }
        "filter" => {
            // Lazy `filter`: items pulled and predicate-tested on demand.
            let f = arg0(&args)?;
            let source = with_host(|h| h.make_iter(&args[1]))?;
            Ok(with_host(|h| {
                h.alloc(PyObj::FilterObj {
                    func: f,
                    source,
                    done: false,
                })
            }))
        }
        "any" => {
            let items = host::iter_vec(&arg0(&args)?)?;
            for x in &items {
                if py_bool(x)? {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }
        "all" => {
            let items = host::iter_vec(&arg0(&args)?)?;
            for x in &items {
                if !py_bool(x)? {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
        "round" => {
            let v = arg0(&args)?;
            // `ndigits` is present only when a second arg was passed and it is not None.
            let has_nd = matches!(args.get(1), Some(x) if !matches!(x, Value::Undef));
            let nd = args.get(1).and_then(|v| with_host(|h| h.as_int(v)));
            match &v {
                Value::Bool(b) => Ok(round_int(&num_bigint::BigInt::from(*b as i64), has_nd, nd)),
                Value::Int(n) => Ok(round_int(&num_bigint::BigInt::from(*n), has_nd, nd)),
                Value::Obj(_) if with_host(|h| matches!(h.get(&v), Some(PyObj::BigInt(_)))) => {
                    let n = with_host(|h| h.big_val(&v)).unwrap();
                    Ok(round_int(&n, has_nd, nd))
                }
                Value::Float(f) => round_float(*f, has_nd, nd),
                _ => Err(host::type_error("round() argument must be a number")),
            }
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
            match args.get(2) {
                None | Some(Value::Undef) => with_host(|h| h.binop(host::binop::POW, &a, &b)),
                Some(m) => pow_mod(&a, &b, m),
            }
        }
        "type" => {
            // 3-arg `type(name, bases, ns)`: dynamic class creation.
            if args.len() == 3 {
                return type_new(&args[0], &args[1], &args[2]);
            }
            // 1-arg form: the object's type.
            let v = arg0(&args)?;
            let tn = with_host(|h| h.type_name(&v));
            Ok(with_host(|h| {
                if h.classes.contains_key(&tn) {
                    h.alloc(PyObj::Class(tn))
                } else {
                    h.alloc(PyObj::Builtin(tn))
                }
            }))
        }
        "staticmethod" => {
            let f = arg0(&args)?;
            Ok(with_host(|h| h.alloc(PyObj::StaticMethod(f))))
        }
        "classmethod" => {
            let f = arg0(&args)?;
            Ok(with_host(|h| h.alloc(PyObj::ClassMethod(f))))
        }
        "super" => {
            // Zero-arg `super()` reads the enclosing method's defining class and
            // `self`; `super(C, obj)` takes them explicitly.
            let (owner, instance) = if args.is_empty() {
                let owner = with_host(|h| h.current_owner())
                    .ok_or_else(|| host::type_error("super(): no arguments"))?;
                let inst = with_host(|h| h.current_self())
                    .ok_or_else(|| host::type_error("super(): no arguments"))?;
                (owner, inst)
            } else {
                let cls = arg0(&args)?;
                let owner = with_host(|h| callable_name(h, &cls))
                    .ok_or_else(|| host::type_error("super() argument 1 must be a type"))?;
                let inst = args.get(1).cloned().unwrap_or(Value::Undef);
                (owner, inst)
            };
            Ok(with_host(|h| h.alloc(PyObj::Super { owner, instance })))
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
            Ok(Value::Bool(get_attr_desc(&v, &n).is_ok()))
        }
        "getattr" => {
            let v = arg0(&args)?;
            let n = with_host(|h| h.str_of(&args.get(1).cloned().unwrap_or(Value::Undef)));
            match get_attr_desc(&v, &n) {
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
            set_attr_desc(&v, &n, val)?;
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
            // A user instance's `hash()` is its `__hash__()` result verbatim
            // (CPython does not re-hash it). Delegate other types to the key hash.
            if with_host(|h| matches!(h.get(&v), Some(PyObj::Instance(_)))) {
                return host::instance_hash_value(&v).map(Value::Int);
            }
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
        "repr" => {
            let v = arg0(&args)?;
            let s = py_repr(&v)?;
            Ok(with_host(|h| h.new_str(s)))
        }
        "ascii" => {
            // Like `repr`, but every non-ASCII char is `\x`/`\u`/`\U`-escaped.
            let v = arg0(&args)?;
            let out = host::ascii_of(&py_repr(&v)?);
            Ok(with_host(|h| h.new_str(out)))
        }
        "format" => {
            let v = arg0(&args)?;
            let spec =
                with_host(|h| h.str_of(&args.get(1).cloned().unwrap_or_else(|| Value::str(""))));
            let out = format_field(&v, 0, &spec)?;
            Ok(with_host(|h| h.new_str(out)))
        }
        "iter" => {
            let v = arg0(&args)?;
            with_host(|h| h.make_iter(&v))
        }
        "next" => {
            let it = arg0(&args)?;
            match host::iter_step(&it)? {
                Some(v) => Ok(v),
                None => match args.get(1) {
                    Some(d) => Ok(d.clone()),
                    // An exhausted generator raises `StopIteration(value)` carrying
                    // its `return` value; any other iterator raises a bare one.
                    None => {
                        if with_host(|h| matches!(h.get(&it), Some(PyObj::Generator { .. }))) {
                            let e = host::gen_stop_iteration(&it);
                            Err(exc_error_string(&e))
                        } else {
                            Err("StopIteration".into())
                        }
                    }
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
        // `exit([code])` / `quit([code])` — the `site` module's `Quitter`
        // objects. Equivalent to `sys.exit`: raise a catchable `SystemExit`
        // (int→that code, None→0, str→message on stderr + exit 1).
        "exit" | "quit" => {
            let exc = with_host(|h| {
                h.alloc(PyObj::Exception {
                    class: "SystemExit".into(),
                    args,
                })
            });
            Err(host::raise_value(&exc)?)
        }
        "callable" => {
            let v = arg0(&args)?;
            Ok(Value::Bool(with_host(|h| {
                match h.get(&v) {
                    Some(PyObj::Func(_))
                    | Some(PyObj::Builtin(_))
                    | Some(PyObj::Class(_))
                    | Some(PyObj::NamedTupleType { .. })
                    | Some(PyObj::Partial { .. })
                    | Some(PyObj::LruCache { .. })
                    | Some(PyObj::StaticMethod(_))
                    | Some(PyObj::ClassMethod(_))
                    | Some(PyObj::BoundMethod { .. }) => true,
                    // An instance is callable iff its class defines `__call__`.
                    Some(PyObj::Instance(i)) => h.class_lookup(&i.class, "__call__").is_some(),
                    _ => false,
                }
            })))
        }
        "property" => {
            let fget = args
                .first()
                .cloned()
                .or_else(|| kw_get(&kwargs, "fget"))
                .unwrap_or(Value::Undef);
            let fset = args
                .get(1)
                .cloned()
                .or_else(|| kw_get(&kwargs, "fset"))
                .unwrap_or(Value::Undef);
            let fdel = args
                .get(2)
                .cloned()
                .or_else(|| kw_get(&kwargs, "fdel"))
                .unwrap_or(Value::Undef);
            Ok(with_host(|h| h.alloc(PyObj::Property { fget, fset, fdel })))
        }
        "vars" => match args.first() {
            // `vars(obj)` == `obj.__dict__`.
            Some(v) => with_host(|h| h.get_attr(v, "__dict__")),
            None => Ok(with_host(|h| h.new_dict(IndexMap::new()))),
        },
        "dir" => Ok(with_host(|h| h.new_list(vec![]))),
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
            Ok(Value::Bool(py_bool(&v)?))
        }
        "list" => {
            let items = match args.first() {
                Some(v) => host::iter_vec(v)?,
                None => vec![],
            };
            Ok(with_host(|h| h.new_list(items)))
        }
        "tuple" => {
            let items = match args.first() {
                Some(v) => host::iter_vec(v)?,
                None => vec![],
            };
            Ok(with_host(|h| h.new_tuple(items)))
        }
        "set" | "frozenset" => {
            let items = match args.first() {
                Some(v) => host::iter_vec(v)?,
                None => vec![],
            };
            let mut s: IndexMap<PKey, Value> = IndexMap::new();
            for it in items {
                let cands = host::set_local_candidates(&s);
                let k = host::with_instance_key(&it, &cands, || with_host(|h| h.to_key(&it)))?;
                host::set_put(&mut s, k, it);
            }
            if name == "frozenset" {
                Ok(with_host(|h| h.new_frozenset(s)))
            } else {
                Ok(with_host(|h| h.new_set(s)))
            }
        }
        "dict" => construct_dict(&args, &kwargs),
        "complex" => {
            // `complex("1+2j")` — string parsing (single string arg only).
            if let Some(first) = args.first() {
                if let Some(s) = with_host(|h| h.as_str(first)) {
                    if args.len() > 1 {
                        return Err(host::type_error(
                            "complex() can't take second arg if first is a string",
                        ));
                    }
                    let (r, i) = parse_complex(&s)?;
                    return Ok(with_host(|h| h.alloc(PyObj::Complex(r, i))));
                }
                // `complex(z)` from another complex.
                if let Some((r, i)) = with_host(|h| match h.get(first) {
                    Some(PyObj::Complex(r, i)) => Some((*r, *i)),
                    _ => None,
                }) {
                    if args.len() == 1 {
                        return Ok(with_host(|h| h.alloc(PyObj::Complex(r, i))));
                    }
                }
            }
            let r = args
                .first()
                .and_then(|v| with_host(|h| h.as_int(v)).map(|n| n as f64).or(as_f(v)))
                .unwrap_or(0.0);
            let i = args.get(1).and_then(as_f).unwrap_or(0.0);
            Ok(with_host(|h| h.alloc(PyObj::Complex(r, i))))
        }
        "bytes" => {
            let b = build_bytes(&args)?;
            Ok(with_host(|h| h.alloc(PyObj::Bytes(b))))
        }
        "bytearray" => {
            let b = build_bytes(&args)?;
            Ok(with_host(|h| h.alloc(PyObj::Bytearray(b))))
        }
        "open" => {
            let file = kw_get(&kwargs, "file")
                .or_else(|| args.first().cloned())
                .ok_or_else(|| host::type_error("open() missing required argument: 'file'"))?;
            let path = with_host(|h| h.as_str(&file))
                .ok_or_else(|| host::type_error("open() argument 'file' must be str"))?;
            let mode = kw_get(&kwargs, "mode")
                .or_else(|| args.get(1).cloned())
                .and_then(|v| with_host(|h| h.as_str(&v)))
                .unwrap_or_else(|| "r".into());
            host::open_file(&path, &mode)
        }
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

/// 3-argument `pow(base, exp, mod)` — modular exponentiation. All three must be
/// integers; `exp` must be non-negative (a negative exponent needs a modular
/// inverse, which is not yet implemented). The result takes the sign of `mod`
/// (Python's floored convention).
fn pow_mod(a: &Value, b: &Value, m: &Value) -> Result<Value, String> {
    use num_bigint::BigInt;
    use num_traits::ToPrimitive;
    let (base, exp, modulus) = match with_host(|h| (h.big_val(a), h.big_val(b), h.big_val(m))) {
        (Some(x), Some(y), Some(z)) => (x, y, z),
        _ => {
            return Err(host::type_error(
                "pow() 3rd argument not allowed unless all arguments are integers",
            ))
        }
    };
    let zero = BigInt::from(0);
    if modulus == zero {
        return Err("ValueError: pow() 3rd argument cannot be 0".into());
    }
    if exp < zero {
        return Err(
            "ValueError: pow() 2nd argument cannot be negative when 3rd argument specified".into(),
        );
    }
    // `modpow` reduces modulo `|modulus|`; re-apply a floored mod so the sign
    // matches Python (result sign == modulus sign).
    let raw = base.modpow(&exp, &modulus);
    let r = &raw % &modulus;
    let r = if r != zero && (r < zero) != (modulus < zero) {
        r + &modulus
    } else {
        r
    };
    Ok(with_host(|h| match r.to_i64() {
        Some(n) => Value::Int(n),
        None => h.alloc(PyObj::BigInt(r)),
    }))
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
        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Ok(b.len()),
        Some(PyObj::Deque { items, .. }) => Ok(items.len()),
        Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => Ok(l.len()),
        Some(PyObj::Dict(d)) => Ok(d.len()),
        Some(PyObj::Set(s)) | Some(PyObj::Frozenset(s)) => Ok(s.len()),
        Some(PyObj::DictView { dict, .. }) => Ok(match h.get(dict) {
            Some(PyObj::Dict(d)) => d.len(),
            _ => 0,
        }),
        Some(PyObj::Range { start, stop, step }) => {
            Ok(host::range_len(*start, *stop, *step).max(0) as usize)
        }
        #[cfg(feature = "stdlib-ffi")]
        Some(PyObj::Foreign(id)) => crate::ffi::len(*id),
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
        host::iter_vec(&args[0])?
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
    let mut items = host::iter_vec(&arg0(args)?)?;
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

/// `round(int[, ndigits])`. Positive/absent ndigits leave an integer unchanged
/// (returning `int`); negative ndigits round to the nearest `10**-ndigits` with
/// round-half-to-even. Always returns an `int` (CPython's `int.__round__`).
fn round_int(n: &num_bigint::BigInt, has_nd: bool, nd: Option<i64>) -> Value {
    let d = if has_nd { nd.unwrap_or(0) } else { 0 };
    if d >= 0 {
        return with_host(|h| h.norm_big(n.clone()));
    }
    use num_integer::Integer;
    use num_traits::Zero;
    let k = (-d) as u32;
    let base = num_bigint::BigInt::from(10).pow(k);
    let (q, r) = n.div_mod_floor(&base); // r in [0, base)
    let half = &base / 2u32; // base = 10**k (k>=1) is even -> exact half
    let rounded = if r < half {
        q
    } else if r > half {
        q + 1
    } else if (&q % 2u32).is_zero() {
        q // tie -> round to even
    } else {
        q + 1
    };
    with_host(|h| h.norm_big(rounded * base))
}

/// `round(float[, ndigits])`. With no ndigits (or `None`) returns an `int`;
/// otherwise a `float`. Rounding is round-half-to-even on the true decimal value,
/// matching CPython — Rust's `{:.N}` formatting already rounds half-to-even, so we
/// format-then-parse to get the correctly-rounded result (also fixing the
/// `2.675`-is-really-2.6749… representation issue).
fn round_float(f: f64, has_nd: bool, nd: Option<i64>) -> Result<Value, String> {
    let ndigits = if has_nd { nd } else { None };
    if !f.is_finite() {
        // With ndigits, a non-finite float rounds to itself; with none, it raises.
        if ndigits.is_some() {
            return Ok(Value::Float(f));
        }
        return Err(if f.is_nan() {
            "ValueError: cannot convert float NaN to integer".into()
        } else {
            "OverflowError: cannot convert float infinity to integer".into()
        });
    }
    Ok(match ndigits {
        None => {
            // No ndigits -> nearest integer, half-to-even, as an int.
            let s = format!("{f:.0}");
            match s.parse::<i64>() {
                Ok(v) => Value::Int(v),
                Err(_) => match s.parse::<num_bigint::BigInt>() {
                    Ok(b) => with_host(|h| h.norm_big(b)),
                    Err(_) => Value::Int(0),
                },
            }
        }
        Some(d) if d >= 0 => {
            let s = format!("{f:.*}", d as usize);
            Value::Float(s.parse::<f64>().unwrap_or(f))
        }
        Some(d) => {
            // Negative ndigits: round-half-even at 10**-d, keep the float type.
            let p = 10f64.powi((-d) as i32);
            let scaled = f / p;
            let rounded: f64 = format!("{scaled:.0}").parse().unwrap_or(scaled);
            Value::Float(rounded * p)
        }
    })
}

fn int_radix(args: &[Value], radix: u32, prefix: &str) -> Result<Value, String> {
    let a0 = arg0(args)?;
    let n = with_host(|h| h.big_val(&a0))
        .ok_or_else(|| host::type_error("'float' object cannot be interpreted as an integer"))?;
    use num_bigint::Sign;
    let sign = if n.sign() == Sign::Minus { "-" } else { "" };
    let body = n.magnitude().to_str_radix(radix);
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
        Value::Float(f) => {
            if !f.is_finite() {
                let what = if f.is_nan() {
                    "float NaN"
                } else {
                    "float infinity"
                };
                return Err(format!("OverflowError: cannot convert {what} to integer"));
            }
            // Truncate toward zero, bignum-safe for values beyond i64.
            let t = f.trunc();
            if t >= i64::MIN as f64 && t <= i64::MAX as f64 {
                Ok(Value::Int(t as i64))
            } else {
                use num_traits::FromPrimitive;
                let b = num_bigint::BigInt::from_f64(t)
                    .ok_or_else(|| host::type_error("cannot convert float to integer"))?;
                Ok(h.norm_big(b))
            }
        }
        Value::Obj(_) if matches!(h.get(&v), Some(PyObj::BigInt(_))) => Ok(v.clone()),
        _ => {
            let s = h
                .as_str(&v)
                .ok_or_else(|| host::type_error("int() argument must be a string or a number"))?;
            let orig = s.clone();
            let s = s.trim();
            let (neg, rest) = if let Some(r) = s.strip_prefix('-') {
                (true, r)
            } else if let Some(r) = s.strip_prefix('+') {
                (false, r)
            } else {
                (false, s)
            };
            // Detect / strip a base prefix. base 0 → auto-detect from prefix.
            let mut base = base;
            let body = if let Some(r) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X"))
            {
                if base == 0 {
                    base = 16;
                }
                if base == 16 {
                    r
                } else {
                    rest
                }
            } else if let Some(r) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
                if base == 0 {
                    base = 8;
                }
                if base == 8 {
                    r
                } else {
                    rest
                }
            } else if let Some(r) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
                if base == 0 {
                    base = 2;
                }
                if base == 2 {
                    r
                } else {
                    rest
                }
            } else {
                if base == 0 {
                    base = 10;
                }
                rest
            };
            let digits = body.replace('_', "");
            let err =
                || format!("ValueError: invalid literal for int() with base {base}: '{orig}'");
            if digits.is_empty() {
                return Err(err());
            }
            match num_bigint::BigInt::parse_bytes(digits.as_bytes(), base as u32) {
                Some(b) => {
                    let b = if neg { -b } else { b };
                    Ok(h.norm_big(b))
                }
                None => Err(err()),
            }
        }
    })
}

/// Parse a `complex()` string argument (`"1+2j"`, `"2j"`, `"-1-3j"`, `"(1+2j)"`,
/// `"j"`, `"5"`). Faithful to CPython's split rule: the last unescaped `+`/`-`
/// that isn't an exponent sign separates the real and imaginary parts.
fn parse_complex(s: &str) -> Result<(f64, f64), String> {
    let err = || "ValueError: complex() arg is a malformed string".to_string();
    let mut t = s.trim();
    if let Some(inner) = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        t = inner.trim();
    }
    if t.is_empty() {
        return Err(err());
    }
    let bytes = t.as_bytes();
    let has_j = matches!(bytes[bytes.len() - 1], b'j' | b'J');
    if !has_j {
        return Ok((parse_py_float(t).ok_or_else(err)?, 0.0));
    }
    let body = &t[..t.len() - 1]; // strip the trailing `j`
    let bb = body.as_bytes();
    let mut split = None;
    for i in (1..bb.len()).rev() {
        if (bb[i] == b'+' || bb[i] == b'-') && !matches!(bb[i - 1], b'e' | b'E') {
            split = Some(i);
            break;
        }
    }
    match split {
        Some(k) => {
            let r = parse_py_float(body[..k].trim()).ok_or_else(err)?;
            let i = parse_imag(&body[k..])?;
            Ok((r, i))
        }
        None => Ok((0.0, parse_imag(body)?)),
    }
}

/// The imaginary magnitude of a complex string part (already `j`-stripped): a
/// bare/empty/`+`/`-` means ±1, otherwise a float.
fn parse_imag(s: &str) -> Result<f64, String> {
    let err = || "ValueError: complex() arg is a malformed string".to_string();
    match s.trim() {
        "" | "+" => Ok(1.0),
        "-" => Ok(-1.0),
        t => parse_py_float(t).ok_or_else(err),
    }
}

/// Parse a Python float literal (with `inf`/`nan`/underscores), or `None`.
fn parse_py_float(s: &str) -> Option<f64> {
    let cleaned = s.trim().replace('_', "");
    match cleaned.as_str() {
        "inf" | "infinity" | "Infinity" | "+inf" | "+infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => Some(f64::NAN),
        t => t.parse::<f64>().ok(),
    }
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
        Value::Obj(_) if matches!(h.get(&v), Some(PyObj::BigInt(_))) => {
            use num_traits::ToPrimitive;
            match h.get(&v) {
                Some(PyObj::BigInt(b)) => Ok(Value::Float(b.to_f64().unwrap_or(f64::INFINITY))),
                _ => unreachable!(),
            }
        }
        _ => {
            let s = h
                .as_str(&v)
                .ok_or_else(|| host::type_error("float() argument must be a string or a number"))?;
            // Underscores may group digits (`float("1_000.5")`).
            let cleaned = s.trim().replace('_', "");
            match cleaned.as_str() {
                "inf" | "infinity" | "Infinity" | "+inf" | "+infinity" => {
                    Ok(Value::Float(f64::INFINITY))
                }
                "-inf" | "-infinity" => Ok(Value::Float(f64::NEG_INFINITY)),
                "nan" | "+nan" | "-nan" => Ok(Value::Float(f64::NAN)),
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
            let pairs = host::iter_vec(v)?;
            for p in pairs {
                let kv = host::iter_vec(&p)?;
                if kv.len() == 2 {
                    let cands = host::dict_local_candidates(&d);
                    let key = host::with_instance_key(&kv[0], &cands, || {
                        with_host(|h| h.to_key(&kv[0]))
                    })?;
                    host::dict_put(&mut d, key, kv[0].clone(), kv[1].clone());
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

/// `sys.*` module functions. `sys.exit` raises a catchable `SystemExit`; the
/// recursion-limit accessors report/accept a fixed 1000 (pythonrs has no
/// Python-level recursion counter to enforce).
fn call_sys(name: &str, args: Vec<Value>) -> Result<Value, String> {
    match name {
        "exit" => {
            // `sys.exit([code])` == `raise SystemExit(code)`. Build the exception
            // with the given args (0 or 1) and raise it so `except SystemExit`
            // catches it and an uncaught one drives the process exit code.
            let exc = with_host(|h| {
                h.alloc(PyObj::Exception {
                    class: "SystemExit".into(),
                    args,
                })
            });
            Err(host::raise_value(&exc)?)
        }
        "getrecursionlimit" => Ok(Value::Int(1000)),
        "setrecursionlimit" => Ok(Value::Undef),
        _ => Err(format!(
            "AttributeError: module 'sys' has no attribute '{name}'"
        )),
    }
}

/// `asyncio.*` module functions — dispatched to the native async runtime.
fn call_asyncio(
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    use crate::async_rt;
    match name {
        "run" => async_rt::run(args.into_iter().next().unwrap_or(Value::Undef)),
        "sleep" => {
            let mut it = args.into_iter();
            let delay = it.next().and_then(num_f).unwrap_or(0.0);
            let result = it.next().unwrap_or(Value::Undef);
            Ok(async_rt::sleep(delay, result))
        }
        "gather" => {
            let return_exceptions = kw_get(&kwargs, "return_exceptions")
                .map(|v| with_host(|h| h.truthy(&v)))
                .unwrap_or(false);
            async_rt::gather(args, return_exceptions)
        }
        "create_task" => {
            let coro = args.into_iter().next().unwrap_or(Value::Undef);
            let name = kw_get(&kwargs, "name").map(|v| with_host(|h| h.str_of(&v)));
            async_rt::create_task(coro, name)
        }
        "ensure_future" => async_rt::ensure_future(args.into_iter().next().unwrap_or(Value::Undef)),
        "wait_for" => {
            let mut it = args.into_iter();
            let aw = it.next().unwrap_or(Value::Undef);
            let timeout = it.next().and_then(num_f);
            async_rt::wait_for(aw, timeout)
        }
        "get_event_loop" | "get_running_loop" | "new_event_loop" => Ok(async_rt::event_loop()),
        "Future" => Ok(async_rt::new_future()),
        // `wait`/`as_completed` take a single iterable of awaitables (not varargs).
        "wait" => {
            let aws = host::iter_vec(&args.into_iter().next().unwrap_or(Value::Undef))?;
            async_rt::wait(aws)
        }
        "as_completed" => {
            let aws = host::iter_vec(&args.into_iter().next().unwrap_or(Value::Undef))?;
            async_rt::as_completed(aws)
        }
        "Event" => Ok(async_rt::new_event()),
        "Lock" => Ok(async_rt::new_lock()),
        "Queue" => {
            let maxsize = kw_get(&kwargs, "maxsize")
                .or_else(|| args.into_iter().next())
                .and_then(num_f)
                .map(|f| f.max(0.0) as usize)
                .unwrap_or(0);
            Ok(async_rt::new_queue(maxsize))
        }
        _ => Err(format!(
            "AttributeError: module 'asyncio' has no attribute '{name}'"
        )),
    }
}

/// Coerce a value to `f64` for numeric asyncio args (delay/timeout).
fn num_f(v: Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(n as f64),
        Value::Float(f) => Some(f),
        Value::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
        _ => None,
    }
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
    ("ConnectionError", "OSError"),
    ("BrokenPipeError", "ConnectionError"),
    ("TimeoutError", "OSError"),
    // asyncio exceptions: CancelledError is a BaseException (never swallowed by
    // `except Exception`); InvalidStateError guards double set_result.
    ("CancelledError", "BaseException"),
    ("InvalidStateError", "Exception"),
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
    // A class object (a user `Class` or a builtin type) is an instance of `type`.
    if want == "type" {
        match h.get(v) {
            Some(PyObj::Class(_)) => return true,
            Some(PyObj::Builtin(n)) if is_builtin_type(n) => return true,
            _ => {}
        }
    }
    let vt = h.type_name(v);
    if type_isa(h, &vt, &want) {
        return true;
    }
    // Structural subclass checks: a namedtuple instance is-a `tuple`; a
    // Counter/defaultdict/OrderedDict is-a `dict` (their `type_name` is the
    // subclass, so the string compare above misses these).
    match h.get(v) {
        Some(PyObj::Tuple(_)) if want == "tuple" => true,
        Some(PyObj::Dict(_)) if want == "dict" => true,
        _ => false,
    }
}

fn type_isa(h: &host::PyHost, a: &str, b: &str) -> bool {
    if a == b || b == "object" {
        return true;
    }
    // Numeric duck: bool is-a int in Python.
    if a == "bool" && b == "int" {
        return true;
    }
    // collections dict subclasses are-a dict; namedtuple instances are-a tuple.
    if b == "dict" && matches!(a, "Counter" | "defaultdict" | "OrderedDict") {
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
        "bytes" => BYTES_METHODS,
        "bytearray" => BYTEARRAY_METHODS,
        "list" => LIST_METHODS,
        "dict" => DICT_METHODS,
        "OrderedDict" => return DICT_METHODS.contains(&name) || name == "move_to_end",
        "defaultdict" => return DICT_METHODS.contains(&name),
        "Counter" => {
            return DICT_METHODS.contains(&name)
                || matches!(
                    name,
                    "most_common" | "elements" | "subtract" | "update" | "total"
                )
        }
        "set" | "frozenset" => SET_METHODS,
        "tuple" => TUPLE_METHODS,
        "range" => &["index", "count"],
        "deque" => DEQUE_METHODS,
        "TextIOWrapper" => FILE_METHODS,
        "int" | "float" | "bool" => NUM_METHODS,
        "complex" => COMPLEX_METHODS,
        "property" => PROPERTY_METHODS,
        "generator" => GENERATOR_METHODS,
        "coroutine" => return GENERATOR_METHODS.contains(&name) || name == "__await__",
        "Future" | "Task" => return FUTURE_METHODS.contains(&name),
        "_UnixSelectorEventLoop" => return LOOP_METHODS.contains(&name),
        "Event" => return matches!(name, "set" | "clear" | "is_set" | "wait"),
        "Lock" => {
            return matches!(
                name,
                "acquire" | "release" | "locked" | "__aenter__" | "__aexit__"
            )
        }
        "Queue" => {
            return matches!(
                name,
                "put" | "get" | "put_nowait" | "get_nowait" | "qsize" | "empty" | "full"
            )
        }
        _ => &[],
    };
    list.contains(&name)
}

const FUTURE_METHODS: &[&str] = &[
    "set_result",
    "set_exception",
    "result",
    "exception",
    "done",
    "cancelled",
    "cancel",
    "add_done_callback",
    "get_name",
    "__await__",
    "__iter__",
];

const LOOP_METHODS: &[&str] = &[
    "run_until_complete",
    "create_task",
    "create_future",
    "call_soon",
    "call_later",
    "time",
    "is_running",
    "is_closed",
    "stop",
    "close",
    "run_forever",
    "get_debug",
    "set_debug",
];

const PROPERTY_METHODS: &[&str] = &["getter", "setter", "deleter"];
const GENERATOR_METHODS: &[&str] = &["send", "throw", "close", "__next__", "__iter__"];

const BYTES_METHODS: &[&str] = &[
    "decode",
    "fromhex",
    "hex",
    "index",
    "rindex",
    "find",
    "rfind",
    "count",
    "startswith",
    "endswith",
    "upper",
    "lower",
    "split",
    "rsplit",
    "splitlines",
    "join",
    "replace",
    "strip",
    "lstrip",
    "rstrip",
    "partition",
    "rpartition",
    "removeprefix",
    "removesuffix",
];
const BYTEARRAY_METHODS: &[&str] = &[
    "append",
    "extend",
    "pop",
    "clear",
    "decode",
    "fromhex",
    "hex",
    "index",
    "rindex",
    "find",
    "rfind",
    "count",
    "startswith",
    "endswith",
    "upper",
    "lower",
    "split",
    "rsplit",
    "splitlines",
    "join",
    "replace",
    "strip",
    "lstrip",
    "rstrip",
    "partition",
    "rpartition",
    "removeprefix",
    "removesuffix",
];
const DEQUE_METHODS: &[&str] = &[
    "append",
    "appendleft",
    "pop",
    "popleft",
    "extend",
    "extendleft",
    "rotate",
    "clear",
    "count",
    "index",
    "remove",
];
const FILE_METHODS: &[&str] = &[
    "read",
    "readline",
    "readlines",
    "write",
    "writelines",
    "close",
    "flush",
    "readable",
    "writable",
    "seekable",
    "__enter__",
    "__exit__",
];

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
    "rindex",
    "partition",
    "rpartition",
    "isnumeric",
    "isdecimal",
    "isidentifier",
    "istitle",
    "isprintable",
    "expandtabs",
    "translate",
    "format_map",
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
    "fromkeys",
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
    "isdisjoint",
    "update",
    "intersection_update",
    "difference_update",
    "symmetric_difference_update",
    "copy",
    "symmetric_difference",
];
const TUPLE_METHODS: &[&str] = &["count", "index"];
const NUM_METHODS: &[&str] = &["bit_length", "is_integer", "conjugate"];
const COMPLEX_METHODS: &[&str] = &["conjugate"];

/// Dispatch a method call on a builtin-typed receiver.
pub fn call_type_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    let tn = with_host(|h| h.type_name(recv));
    match tn.as_str() {
        // `.format` needs the kwargs (keyword replacement fields); other str
        // methods don't take keywords.
        "str" if name == "format" => {
            let s = with_host(|h| h.as_str(recv)).unwrap_or_default();
            str_dot_format(&s, &args, &kwargs)
        }
        "str" => str_method(recv, name, &args),
        "bytes" => bytes_method(recv, name, &args),
        "bytearray" => bytearray_method(recv, name, &args),
        "list" => list_method(recv, name, &args, &kwargs),
        "dict" => dict_method(recv, name, &args, &kwargs),
        "Counter" | "defaultdict" | "OrderedDict" => {
            if let Some(r) = collections_dict_method(recv, name, &args, &tn) {
                return r;
            }
            dict_method(recv, name, &args, &kwargs)
        }
        "set" | "frozenset" => set_method(recv, name, &args),
        "tuple" => tuple_method(recv, name, &args),
        "range" => range_method(recv, name, &args),
        "deque" => deque_method(recv, name, &args),
        "TextIOWrapper" => file_method(recv, name, &args),
        "functools._lru_cache_wrapper" => lru_wrapper_method(recv, name),
        "int" | "float" | "bool" => num_method(recv, name, &args),
        "complex" => complex_method(recv, name),
        "property" => property_method(recv, name, &args),
        "generator" => generator_method(recv, name, &args),
        "coroutine" => coroutine_method(recv, name, &args),
        "Future" | "Task" => crate::async_rt::future_method(recv, name, args),
        "_UnixSelectorEventLoop" => crate::async_rt::loop_method(name, args),
        "Event" | "Lock" | "Queue" => crate::async_rt::async_obj_method(recv, name, args),
        other => Err(format!(
            "AttributeError: '{other}' object has no attribute '{name}'"
        )),
    }
}

/// `coro.send/throw/close/__await__` — a coroutine's method protocol (shares the
/// generator machinery; `__await__` returns the coroutine itself to be driven).
fn coroutine_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "__await__" => Ok(recv.clone()),
        _ => generator_method(recv, name, args),
    }
}

/// The abort/error string for an exception object (`Class` or `Class: message`).
fn exc_error_string(exc: &Value) -> String {
    with_host(|h| match h.get(exc) {
        Some(PyObj::Exception { class, args }) => {
            let msg = h.exc_message(args);
            if msg.is_empty() {
                class.clone()
            } else {
                format!("{class}: {msg}")
            }
        }
        _ => "StopIteration".into(),
    })
}

/// `gen.send/throw/close/__next__/__iter__` — the generator method protocol.
fn generator_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "__iter__" => Ok(recv.clone()),
        "send" | "__next__" => {
            let v = if name == "send" {
                args.first().cloned().unwrap_or(Value::Undef)
            } else {
                Value::Undef
            };
            // A just-started generator only accepts `send(None)`.
            if name == "send" && !host::gen_started(recv) && !matches!(v, Value::Undef) {
                return Err(host::type_error(
                    "can't send non-None value to a just-started generator",
                ));
            }
            match host::gen_resume(recv, v)? {
                Some(y) => Ok(y),
                None => {
                    let e = host::gen_stop_iteration(recv);
                    Err(exc_error_string(&e))
                }
            }
        }
        "throw" => {
            // `throw(ExcType)` / `throw(exc_instance)`.
            let a0 = args.first().cloned().unwrap_or(Value::Undef);
            let exc = with_host(|h| match h.get(&a0) {
                // A class/builtin name → instantiate with the remaining args.
                Some(PyObj::Builtin(n)) if is_exception_class(n) => {
                    let n = n.clone();
                    let rest = args.get(1..).unwrap_or(&[]).to_vec();
                    h.alloc(PyObj::Exception {
                        class: n,
                        args: rest,
                    })
                }
                _ => a0.clone(),
            });
            match host::gen_throw(recv, exc)? {
                Some(y) => Ok(y),
                None => {
                    let e = host::gen_stop_iteration(recv);
                    Err(exc_error_string(&e))
                }
            }
        }
        "close" => {
            let ge = with_host(|h| {
                h.alloc(PyObj::Exception {
                    class: "GeneratorExit".into(),
                    args: vec![],
                })
            });
            match host::gen_throw(recv, ge) {
                // The generator yielded again instead of finishing → error.
                Ok(Some(_)) => Err("RuntimeError: generator ignored GeneratorExit".to_string()),
                Ok(None) => Ok(Value::Undef),
                // GeneratorExit (or a clean StopIteration) propagating out is the
                // normal, expected outcome of close(); swallow it.
                Err(e) if e.contains("GeneratorExit") || e.contains("StopIteration") => {
                    with_host(|h| {
                        h.error = None;
                        h.exc = None;
                    });
                    Ok(Value::Undef)
                }
                Err(e) => Err(e),
            }
        }
        _ => Err(format!(
            "AttributeError: 'generator' object has no attribute '{name}'"
        )),
    }
}

/// `property.getter/setter/deleter(func)` — return a copy of the property with
/// the corresponding accessor replaced (the `@x.setter` decorator form).
fn property_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let (fget, fset, fdel) = with_host(|h| match h.get(recv) {
        Some(PyObj::Property { fget, fset, fdel }) => (fget.clone(), fset.clone(), fdel.clone()),
        _ => (Value::Undef, Value::Undef, Value::Undef),
    });
    let f = args.first().cloned().unwrap_or(Value::Undef);
    let (fget, fset, fdel) = match name {
        "getter" => (f, fset, fdel),
        "setter" => (fget, f, fdel),
        "deleter" => (fget, fset, f),
        _ => {
            return Err(format!(
                "AttributeError: 'property' object has no attribute '{name}'"
            ))
        }
    };
    Ok(with_host(|h| h.alloc(PyObj::Property { fget, fset, fdel })))
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
            let items = host::iter_vec(&args[0])?;
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
        "rindex" => match s.rfind(&sarg(0)) {
            Some(b) => Ok(Value::Int(s[..b].chars().count() as i64)),
            None => Err("ValueError: substring not found".into()),
        },
        "partition" => {
            let sep = sarg(0);
            let (a, b, c) = match s.find(&sep) {
                Some(p) => (
                    s[..p].to_string(),
                    sep.clone(),
                    s[p + sep.len()..].to_string(),
                ),
                None => (s.clone(), String::new(), String::new()),
            };
            Ok(with_host(|h| {
                let t = vec![h.new_str(a), h.new_str(b), h.new_str(c)];
                h.new_tuple(t)
            }))
        }
        "rpartition" => {
            let sep = sarg(0);
            let (a, b, c) = match s.rfind(&sep) {
                Some(p) => (
                    s[..p].to_string(),
                    sep.clone(),
                    s[p + sep.len()..].to_string(),
                ),
                None => (String::new(), String::new(), s.clone()),
            };
            Ok(with_host(|h| {
                let t = vec![h.new_str(a), h.new_str(b), h.new_str(c)];
                h.new_tuple(t)
            }))
        }
        "isnumeric" => Ok(Value::Bool(!s.is_empty() && s.chars().all(is_numeric_char))),
        "isdecimal" => Ok(Value::Bool(
            !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()),
        )),
        "isidentifier" => Ok(Value::Bool(is_identifier(&s))),
        "istitle" => Ok(Value::Bool(is_titlecased(&s))),
        "isprintable" => Ok(Value::Bool(
            s.chars().all(|c| !c.is_control() && c != '\u{85}'),
        )),
        "expandtabs" => {
            let tabsize = args
                .first()
                .and_then(|v| with_host(|h| h.as_int(v)))
                .unwrap_or(8)
                .max(0) as usize;
            Ok(new_str(expand_tabs(&s, tabsize)))
        }
        "translate" => {
            let table = arg0(args)?;
            Ok(new_str(str_translate(&s, &table)?))
        }
        "format_map" => {
            let mapping = arg0(args)?;
            str_format_map(&s, &mapping)
        }
        "encode" => Ok(with_host(|h| h.alloc(PyObj::Bytes(s.into_bytes())))),
        // `.format` with keyword fields comes through `call_type_method`, which
        // has the kwargs; a bare `str_method` call has none.
        "format" => str_dot_format(&s, args, &[]),
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
/// CPython `str.maketrans`: build a translation table (a dict of ordinal→
/// int/str/None). Either a single mapping arg, or two equal-length strings
/// (`x`→`y`), with an optional third string whose chars map to `None`.
/// `type(name, bases, namespace)` — dynamic class creation. `bases` is a tuple
/// of class objects; `namespace` a dict of the class body. Registers the class
/// and returns it.
fn type_new(name: &Value, bases: &Value, ns: &Value) -> Result<Value, String> {
    type_new_meta(name, bases, ns, "type")
}

/// `type.__new__(mcls, name, bases, namespace)` — like [`type_new`] but tags the
/// new class's metaclass as `metaclass` (so `type(cls) is mcls`). Used by the
/// metaclass construction path.
pub fn type_new_meta(
    name: &Value,
    bases: &Value,
    ns: &Value,
    metaclass: &str,
) -> Result<Value, String> {
    let cname = with_host(|h| h.as_str(name))
        .ok_or_else(|| host::type_error("type() argument 1 must be str"))?;
    // Base class names from the bases tuple (a bare `object` base is implicit).
    let base_names: Vec<String> = with_host(|h| match h.get(bases) {
        Some(PyObj::Tuple(items)) | Some(PyObj::List(items)) => items
            .iter()
            .filter_map(|b| match h.get(b) {
                Some(PyObj::Class(n)) => Some(n.clone()),
                Some(PyObj::Builtin(n)) if n != "object" => Some(n.clone()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    });
    // The class-body namespace (string keys → values).
    let namespace: IndexMap<String, Value> = with_host(|h| match h.get(ns) {
        Some(PyObj::Dict(d)) => d
            .values()
            .filter_map(|(k, v)| h.as_str(k).map(|s| (s, v.clone())))
            .collect(),
        _ => IndexMap::new(),
    });
    Ok(with_host(|h| {
        h.register_class_meta(&cname, base_names, namespace, metaclass)
    }))
}

fn str_maketrans(args: &[Value]) -> Result<Value, String> {
    let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
    if args.len() == 1 {
        // A mapping: string-of-length-1 keys become ordinals; int keys stay.
        let pairs = with_host(|h| match h.get(&args[0]) {
            Some(PyObj::Dict(m)) => m
                .values()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Vec<_>>(),
            _ => vec![],
        });
        for (k, v) in pairs {
            let ord = with_host(|h| {
                if let Some(n) = h.as_int(&k) {
                    Some(n)
                } else {
                    h.as_str(&k)
                        .filter(|s| s.chars().count() == 1)
                        .map(|s| s.chars().next().unwrap() as i64)
                }
            });
            match ord {
                Some(n) => {
                    host::dict_put(&mut d, PKey::Int(n), Value::Int(n), v);
                }
                None => {
                    return Err(host::type_error(
                        "keys in translate table must be strings of length 1",
                    ))
                }
            }
        }
        return Ok(with_host(|h| h.new_dict(d)));
    }
    // Two (or three) string args.
    let x = with_host(|h| h.as_str(&args[0])).unwrap_or_default();
    let y = with_host(|h| h.as_str(args.get(1).unwrap_or(&Value::Undef))).unwrap_or_default();
    let xs: Vec<char> = x.chars().collect();
    let ys: Vec<char> = y.chars().collect();
    if xs.len() != ys.len() {
        return Err(host::type_error(
            "the first two maketrans arguments must have equal length",
        ));
    }
    for (a, b) in xs.iter().zip(ys.iter()) {
        let ord = *a as i64;
        host::dict_put(
            &mut d,
            PKey::Int(ord),
            Value::Int(ord),
            Value::Int(*b as i64),
        );
    }
    if let Some(z) = args.get(2) {
        let zs = with_host(|h| h.as_str(z)).unwrap_or_default();
        for c in zs.chars() {
            let ord = c as i64;
            host::dict_put(&mut d, PKey::Int(ord), Value::Int(ord), Value::Undef);
        }
    }
    Ok(with_host(|h| h.new_dict(d)))
}

/// CPython `str.isnumeric`: any Unicode numeric character (Nd/Nl/No).
fn is_numeric_char(c: char) -> bool {
    c.is_numeric()
}

/// CPython `str.isidentifier` (approximated with Rust's Unicode categories).
fn is_identifier(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        Some(c) if c == '_' || c.is_alphabetic() => {}
        _ => return false,
    }
    it.all(|c| c == '_' || c.is_alphanumeric())
}

/// CPython `str.istitle`: every cased run starts upper/title-cased and continues
/// lower-cased; at least one cased character must be present.
fn is_titlecased(s: &str) -> bool {
    let mut cased = false;
    let mut prev_cased = false;
    for c in s.chars() {
        if c.is_uppercase() {
            if prev_cased {
                return false;
            }
            prev_cased = true;
            cased = true;
        } else if c.is_lowercase() {
            if !prev_cased {
                return false;
            }
            prev_cased = true;
            cased = true;
        } else {
            prev_cased = false;
        }
    }
    cased
}

/// CPython `str.expandtabs`: replace tabs with spaces to the next tab stop,
/// resetting the column at each newline/carriage-return.
fn expand_tabs(s: &str, tabsize: usize) -> String {
    let mut out = String::new();
    let mut col = 0;
    for c in s.chars() {
        match c {
            '\t' => {
                if tabsize == 0 {
                    continue;
                }
                let n = tabsize - (col % tabsize);
                for _ in 0..n {
                    out.push(' ');
                }
                col += n;
            }
            '\n' | '\r' => {
                out.push(c);
                col = 0;
            }
            other => {
                out.push(other);
                col += 1;
            }
        }
    }
    out
}

/// CPython `str.translate(table)`: `table` maps ordinals to a replacement
/// ordinal (int), string, or `None` (delete). Absent keys pass through.
fn str_translate(s: &str, table: &Value) -> Result<String, String> {
    let mut out = String::new();
    for c in s.chars() {
        let key = with_host(|h| h.to_key(&Value::Int(c as i64)))?;
        let mapped = with_host(|h| match h.get(table) {
            Some(PyObj::Dict(d)) => d.get(&key).map(|(_, v)| v.clone()),
            _ => None,
        });
        match mapped {
            None => out.push(c),
            Some(Value::Undef) => {} // None → delete
            Some(v) => {
                if let Some(n) = with_host(|h| h.as_int(&v)) {
                    if let Some(ch) = char::from_u32(n as u32) {
                        out.push(ch);
                    }
                } else if let Some(rep) = with_host(|h| h.as_str(&v)) {
                    out.push_str(&rep);
                }
            }
        }
    }
    Ok(out)
}

/// CPython `str.format_map(mapping)`: like `.format` but named fields resolve
/// from `mapping` and it is not copied.
fn str_format_map(s: &str, mapping: &Value) -> Result<Value, String> {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
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
                i += 1;
                let (name_conv, spec) = match field.split_once(':') {
                    Some((a, b)) => (a.to_string(), b.to_string()),
                    None => (field, String::new()),
                };
                let (fname, conv) = match name_conv.split_once('!') {
                    Some((n, c)) => (
                        n.to_string(),
                        match c {
                            "s" => 1,
                            "r" => 2,
                            "a" => 3,
                            _ => 0,
                        },
                    ),
                    None => (name_conv, 0),
                };
                let key = PKey::Str(fname.clone());
                let val = with_host(|h| match h.get(mapping) {
                    Some(PyObj::Dict(d)) => d.get(&key).map(|(_, v)| v.clone()),
                    _ => None,
                });
                let val = match val {
                    Some(v) => v,
                    None => return Err(format!("KeyError: '{fname}'")),
                };
                out.push_str(&format_field(&val, conv, &spec)?);
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(new_str(out))
}

/// A conversion code (`!s`/`!r`/`!a` → 1/2/3, 0 = none) from the trailing part
/// of a field-name segment (the part before any `:` format spec).
fn conv_code(c: &str) -> i64 {
    match c {
        "s" => 1,
        "r" => 2,
        "a" => 3,
        _ => 0,
    }
}

/// Split a replacement field's inner text into `(field_name, conv, spec)`. The
/// `:` (format-spec separator) and `!` (conversion) are only recognized at
/// bracket-depth 0, so a subscript key may itself contain them (`{d[a:b]}`).
fn split_format_field(field: &str) -> (String, i64, String) {
    let fchars: Vec<char> = field.chars().collect();
    let mut depth = 0i32;
    let mut colon = None;
    let mut bang = None;
    for (i, &c) in fchars.iter().enumerate() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ':' if depth == 0 => {
                colon = Some(i);
                break;
            }
            '!' if depth == 0 && bang.is_none() => bang = Some(i),
            _ => {}
        }
    }
    let (head, spec) = match colon {
        Some(i) => (
            fchars[..i].iter().collect::<String>(),
            fchars[i + 1..].iter().collect::<String>(),
        ),
        None => (field.to_string(), String::new()),
    };
    // Re-scan the head for a `!conv` (must precede the spec).
    match bang.filter(|&b| colon.map(|c| b < c).unwrap_or(true)) {
        Some(b) => {
            let name: String = fchars[..b].iter().collect();
            let conv: String = fchars[b + 1..colon.unwrap_or(fchars.len())]
                .iter()
                .collect();
            (name, conv_code(&conv), spec)
        }
        None => (head, 0, spec),
    }
}

/// Resolve a `str.format` field name (`arg_name` plus `.attr` / `[index]`
/// accessor chain) against positional `args`, `kwargs`, and the shared
/// automatic-field counter `auto`.
fn resolve_format_arg(
    name: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
    auto: &mut usize,
) -> Result<Value, String> {
    let nchars: Vec<char> = name.chars().collect();
    // Base arg_name ends at the first `.` or `[`.
    let mut i = 0;
    while i < nchars.len() && nchars[i] != '.' && nchars[i] != '[' {
        i += 1;
    }
    let base: String = nchars[..i].iter().collect();
    let mut val = if base.is_empty() {
        let v = args
            .get(*auto)
            .cloned()
            .ok_or_else(|| format!("IndexError: Replacement index {auto} out of range"))?;
        *auto += 1;
        v
    } else if let Ok(n) = base.parse::<usize>() {
        args.get(n)
            .cloned()
            .ok_or_else(|| format!("IndexError: Replacement index {n} out of range"))?
    } else {
        kwargs
            .iter()
            .find(|(k, _)| *k == base)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| format!("KeyError: '{base}'"))?
    };
    // Accessor chain.
    while i < nchars.len() {
        match nchars[i] {
            '.' => {
                i += 1;
                let start = i;
                while i < nchars.len() && nchars[i] != '.' && nchars[i] != '[' {
                    i += 1;
                }
                let attr: String = nchars[start..i].iter().collect();
                val = with_host(|h| h.get_attr(&val, &attr))?;
            }
            '[' => {
                i += 1;
                let start = i;
                while i < nchars.len() && nchars[i] != ']' {
                    i += 1;
                }
                let key: String = nchars[start..i].iter().collect();
                if i < nchars.len() {
                    i += 1; // skip ]
                }
                let keyv = if let Ok(n) = key.parse::<i64>() {
                    Value::Int(n)
                } else {
                    with_host(|h| h.new_str(key))
                };
                val = with_host(|h| h.get_item(&val, &keyv))?;
            }
            _ => break,
        }
    }
    Ok(val)
}

/// Substitute any `{…}` replacement fields inside a format spec (one nesting
/// level, per CPython), formatting each with its default `str()` and consuming
/// the shared automatic-field counter.
fn substitute_nested_spec(
    spec: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
    auto: &mut usize,
) -> Result<String, String> {
    if !spec.contains('{') {
        return Ok(spec.to_string());
    }
    let chars: Vec<char> = spec.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '{' => {
                i += 1;
                let mut inner = String::new();
                while i < chars.len() && chars[i] != '}' {
                    inner.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    i += 1; // skip }
                }
                let (fname, conv, _) = split_format_field(&inner);
                let val = resolve_format_arg(&fname, args, kwargs, auto)?;
                out.push_str(&format_field(&val, conv, "")?);
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

fn str_dot_format(s: &str, args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut auto = 0usize;
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
                // Extract the field, honoring nested `{…}` inside the spec.
                let mut field = String::new();
                let mut depth = 1;
                i += 1;
                while i < chars.len() && depth > 0 {
                    match chars[i] {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    field.push(chars[i]);
                    i += 1;
                }
                i += 1; // skip closing }
                let (fname, conv, spec) = split_format_field(&field);
                let val = resolve_format_arg(&fname, args, kwargs, &mut auto)?;
                let spec = substitute_nested_spec(&spec, args, kwargs, &mut auto)?;
                out.push_str(&format_field(&val, conv, &spec)?);
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
            let items = host::iter_vec(&arg0(args)?)?;
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

fn dict_method(
    recv: &Value,
    name: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value, String> {
    match name {
        "keys" => Ok(with_host(|h| {
            h.alloc(PyObj::DictView {
                dict: recv.clone(),
                kind: 0,
            })
        })),
        "values" => Ok(with_host(|h| {
            h.alloc(PyObj::DictView {
                dict: recv.clone(),
                kind: 1,
            })
        })),
        "items" => Ok(with_host(|h| {
            h.alloc(PyObj::DictView {
                dict: recv.clone(),
                kind: 2,
            })
        })),
        "fromkeys" => {
            // `dict.fromkeys(iterable[, value])` — unbound classmethod form; here
            // `recv` is the dict the method was fetched from (unused).
            let keys = host::iter_vec(&arg0(args)?)?;
            let value = args.get(1).cloned().unwrap_or(Value::Undef);
            let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
            for k in keys {
                let cands = host::dict_local_candidates(&d);
                let key = host::with_instance_key(&k, &cands, || with_host(|h| h.to_key(&k)))?;
                host::dict_put(&mut d, key, k, value.clone());
            }
            Ok(with_host(|h| h.new_dict(d)))
        }
        "get" => {
            let kv = arg0(args)?;
            let cands = host::instance_key_candidates(recv);
            let key = host::with_instance_key(&kv, &cands, || with_host(|h| h.to_key(&kv)))?;
            Ok(with_host(|h| match h.get(recv) {
                Some(PyObj::Dict(d)) => d.get(&key).map(|(_, v)| v.clone()),
                _ => None,
            })
            .unwrap_or_else(|| args.get(1).cloned().unwrap_or(Value::Undef)))
        }
        "pop" => {
            let kv = arg0(args)?;
            let cands = host::instance_key_candidates(recv);
            let key = host::with_instance_key(&kv, &cands, || with_host(|h| h.to_key(&kv)))?;
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
            let cands = host::instance_key_candidates(recv);
            let key = host::with_instance_key(&kv, &cands, || with_host(|h| h.to_key(&kv)))?;
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
            dict_update(recv, args.first(), kwargs)?;
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

/// `dict.update(other?, **kwargs)`: `other` may be a mapping (dict/view) OR an
/// iterable of key/value pairs; keyword args are applied last (they win).
fn dict_update(
    recv: &Value,
    other: Option<&Value>,
    kwargs: &[(String, Value)],
) -> Result<(), String> {
    // Collect (key, key-value, value) triples to apply, in order.
    let mut triples: Vec<(PKey, Value, Value)> = Vec::new();
    if let Some(o) = other {
        let is_dict = with_host(|h| matches!(h.get(o), Some(PyObj::Dict(_))));
        if is_dict {
            let pairs = with_host(|h| match h.get(o) {
                Some(PyObj::Dict(d)) => d
                    .iter()
                    .map(|(k, (kv, v))| (k.clone(), kv.clone(), v.clone()))
                    .collect::<Vec<_>>(),
                _ => vec![],
            });
            triples.extend(pairs);
        } else {
            // An iterable of 2-element pairs.
            for pair in host::iter_vec(o)? {
                let elems = host::iter_vec(&pair)?;
                if elems.len() != 2 {
                    return Err(host::type_error(
                        "dictionary update sequence element has length != 2",
                    ));
                }
                let key = with_host(|h| h.to_key(&elems[0]))?;
                triples.push((key, elems[0].clone(), elems[1].clone()));
            }
        }
    }
    for (k, v) in kwargs {
        let kv = with_host(|h| h.new_str(k.clone()));
        triples.push((PKey::Str(k.clone()), kv, v.clone()));
    }
    with_host(|h| {
        if let Some(PyObj::Dict(d)) = h.get_mut(recv) {
            for (k, kv, v) in triples {
                host::dict_put(d, k, kv, v);
            }
        }
    });
    Ok(())
}

fn set_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    // `frozenset` is immutable: it has no in-place mutators.
    if with_host(|h| h.is_frozenset(recv))
        && matches!(
            name,
            "add"
                | "discard"
                | "remove"
                | "clear"
                | "update"
                | "pop"
                | "intersection_update"
                | "difference_update"
                | "symmetric_difference_update"
        )
    {
        return Err(format!(
            "AttributeError: 'frozenset' object has no attribute '{name}'"
        ));
    }
    match name {
        "add" => {
            let v = arg0(args)?;
            let cands = host::instance_key_candidates(recv);
            let k = host::with_instance_key(&v, &cands, || with_host(|h| h.to_key(&v)))?;
            with_host(|h| {
                if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                    host::set_put(s, k, v);
                }
            });
            Ok(Value::Undef)
        }
        "discard" | "remove" => {
            let v = arg0(args)?;
            let cands = host::instance_key_candidates(recv);
            let k = host::with_instance_key(&v, &cands, || with_host(|h| h.to_key(&v)))?;
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
            let other_set = if with_host(|h| h.setlike(&other).is_some()) {
                other
            } else {
                call_builtin_function("set", vec![other], vec![])?
            };
            with_host(|h| h.arith(NumOp::Sub, recv, &other_set))
        }
        "issubset" => {
            let other = iter_keys(&arg0(args)?)?;
            Ok(Value::Bool(with_host(|h| {
                set_keys(h, recv).iter().all(|k| other.contains(k))
            })))
        }
        "issuperset" => {
            let other = iter_keys(&arg0(args)?)?;
            Ok(Value::Bool(with_host(|h| {
                other.iter().all(|k| set_keys(h, recv).contains(k))
            })))
        }
        "isdisjoint" => {
            let other = iter_keys(&arg0(args)?)?;
            Ok(Value::Bool(with_host(|h| {
                set_keys(h, recv).iter().all(|k| !other.contains(k))
            })))
        }
        "copy" => {
            let (s, frozen) = with_host(|h| match h.get(recv) {
                Some(PyObj::Set(s)) => (s.clone(), false),
                Some(PyObj::Frozenset(s)) => (s.clone(), true),
                _ => (IndexMap::new(), false),
            });
            Ok(with_host(|h| h.new_setlike(s, frozen)))
        }
        "update" => {
            let items = host::iter_vec(&arg0(args)?)?;
            for it in items {
                let k = with_host(|h| h.to_key(&it))?;
                with_host(|h| {
                    if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                        host::set_put(s, k, it);
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
        "intersection_update" => {
            let other = iter_keys(&arg0(args)?)?;
            with_host(|h| {
                if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                    s.retain(|k, _| other.contains(k));
                }
            });
            Ok(Value::Undef)
        }
        "difference_update" => {
            let other = iter_keys(&arg0(args)?)?;
            with_host(|h| {
                if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                    s.retain(|k, _| !other.contains(k));
                }
            });
            Ok(Value::Undef)
        }
        "symmetric_difference_update" => {
            // Toggle membership of each element of the other iterable.
            let items = host::iter_vec(&arg0(args)?)?;
            for it in items {
                let k = with_host(|h| h.to_key(&it))?;
                with_host(|h| {
                    if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                        if s.shift_remove(&k).is_none() {
                            host::set_put(s, k, it);
                        }
                    }
                });
            }
            Ok(Value::Undef)
        }
        _ => Err(format!(
            "AttributeError: 'set' object has no attribute '{name}'"
        )),
    }
}

fn set_keys(h: &host::PyHost, v: &Value) -> Vec<PKey> {
    match h.setlike(v) {
        Some(s) => s.keys().cloned().collect(),
        None => vec![],
    }
}

/// The element keys of any iterable (set/frozenset short-circuit; anything else
/// is materialized and hashed) — for set methods that accept an arbitrary
/// iterable argument (`issubset([...])`, `isdisjoint((...))`, …).
fn iter_keys(v: &Value) -> Result<Vec<PKey>, String> {
    if let Some(ks) = with_host(|h| h.setlike(v).map(|s| s.keys().cloned().collect::<Vec<_>>())) {
        return Ok(ks);
    }
    let items = host::iter_vec(v)?;
    let mut ks = Vec::with_capacity(items.len());
    for it in items {
        ks.push(with_host(|h| h.to_key(&it))?);
    }
    Ok(ks)
}

fn set_binop(recv: &Value, args: &[Value], tag: i64) -> Result<Value, String> {
    let other = arg0(args)?;
    // Coerce a non-set argument to a set first.
    let other_set = if with_host(|h| h.setlike(&other).is_some()) {
        other
    } else {
        call_builtin_function("set", vec![other], vec![])?
    };
    with_host(|h| h.binop(tag, recv, &other_set))
}

fn range_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let (start, stop, step) = with_host(|h| match h.get(recv) {
        Some(PyObj::Range { start, stop, step }) => (*start, *stop, *step),
        _ => (0, 0, 1),
    });
    let len = host::range_len(start, stop, step);
    let arg = arg0(args)?;
    // The integer value queried (`range.index`/`range.count` take an int); a
    // non-int is simply "not in the range".
    let target = with_host(|h| h.as_int(&arg));
    // Position of `target` in the arithmetic progression, if any.
    let pos = target.and_then(|t| {
        if (t - start) % step == 0 {
            let k = (t - start) / step;
            (k >= 0 && k < len).then_some(k)
        } else {
            None
        }
    });
    match name {
        "count" => Ok(Value::Int(if pos.is_some() { 1 } else { 0 })),
        "index" => match pos {
            Some(k) => Ok(Value::Int(k)),
            None => Err(format!(
                "ValueError: {} is not in range",
                with_host(|h| h.repr_of(&arg))
            )),
        },
        _ => Err(format!(
            "AttributeError: 'range' object has no attribute '{name}'"
        )),
    }
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

/// `complex` methods (`conjugate`).
fn complex_method(recv: &Value, name: &str) -> Result<Value, String> {
    match name {
        "conjugate" => with_host(|h| match h.get(recv) {
            Some(PyObj::Complex(r, i)) => {
                let (r, i) = (*r, *i);
                Ok(h.alloc(PyObj::Complex(r, -i)))
            }
            _ => Err(host::type_error(
                "descriptor 'conjugate' requires a 'complex' object",
            )),
        }),
        _ => Err(format!(
            "AttributeError: 'complex' object has no attribute '{name}'"
        )),
    }
}

// ── bytes / bytearray ────────────────────────────────────────────────────────

/// The byte content of a `bytes` / `bytearray` receiver.
fn recv_bytes(recv: &Value) -> Vec<u8> {
    with_host(|h| match h.get(recv) {
        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => b.clone(),
        _ => vec![],
    })
}

/// A bytes-like argument as raw bytes: a `bytes`/`bytearray`, or a single
/// `int` in `0..=255`. `None` for anything else (a `str`, out-of-range int, …).
fn arg_bytes_like(v: &Value) -> Option<Vec<u8>> {
    let obj = with_host(|h| match h.get(v) {
        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Some(b.clone()),
        _ => None,
    });
    if obj.is_some() {
        return obj;
    }
    match v {
        Value::Int(n) if (0..=255).contains(n) => Some(vec![*n as u8]),
        _ => None,
    }
}

/// Only a `bytes`/`bytearray` (not an int) as raw bytes — for `file.write(b'…')`.
fn as_bytes_object(v: &Value) -> Option<Vec<u8>> {
    with_host(|h| match h.get(v) {
        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Some(b.clone()),
        _ => None,
    })
}

/// ASCII whitespace (CPython `Py_ISSPACE`): space, tab, LF, CR, VT, FF.
fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Allocate a `bytes` (when `!is_ba`) or `bytearray` value from raw bytes. Used
/// so a method returns the same type as its receiver.
fn mk_bytes(is_ba: bool, v: Vec<u8>) -> Value {
    with_host(|h| {
        if is_ba {
            h.alloc(PyObj::Bytearray(v))
        } else {
            h.alloc(PyObj::Bytes(v))
        }
    })
}

/// Normalize an optional `start`/`end` pair (CPython slice-index clamping) over
/// a length-`len` buffer, reading from `args[from]` and `args[from + 1]`.
fn resolve_start_end(len: usize, args: &[Value], from: usize) -> (usize, usize) {
    let n = len as i64;
    let norm = |v: &Value, default: i64| -> i64 {
        match with_host(|h| h.as_int(v)) {
            Some(i) => {
                let k = if i < 0 { i + n } else { i };
                k.clamp(0, n)
            }
            None => default,
        }
    };
    let start = args.get(from).map(|v| norm(v, 0)).unwrap_or(0);
    let end = args.get(from + 1).map(|v| norm(v, n)).unwrap_or(n);
    (start as usize, (end as usize).max(start as usize))
}

/// Search `hay[start..end]` for `needle`, returning an absolute index (or None).
/// `reverse` finds the last match. An empty needle matches at `start` (forward)
/// or `end` (reverse).
fn bytes_find(hay: &[u8], needle: &[u8], start: usize, end: usize, reverse: bool) -> Option<usize> {
    let end = end.min(hay.len());
    if start > end {
        return None;
    }
    let region = &hay[start..end];
    if needle.is_empty() {
        return Some(if reverse { end } else { start });
    }
    if needle.len() > region.len() {
        return None;
    }
    if reverse {
        region
            .windows(needle.len())
            .rposition(|w| w == needle)
            .map(|p| p + start)
    } else {
        region
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|p| p + start)
    }
}

/// Non-overlapping count of `needle` in `hay[start..end]`. An empty needle
/// counts every gap (`end - start + 1`).
fn count_range(hay: &[u8], needle: &[u8], start: usize, end: usize) -> usize {
    let end = end.min(hay.len());
    if start > end {
        return 0;
    }
    let region = &hay[start..end];
    if needle.is_empty() {
        return region.len() + 1;
    }
    let mut c = 0;
    let mut i = 0;
    while i + needle.len() <= region.len() {
        if &region[i..i + needle.len()] == needle {
            c += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    c
}

/// Split `hay` on `sep`, keeping empty fields (`b'aXXb'.split(b'X')` →
/// `[b'a', b'', b'b']`). `maxsplit < 0` means unlimited. `reverse` splits from
/// the right (`rsplit`).
fn split_on_sep(hay: &[u8], sep: &[u8], maxsplit: i64, reverse: bool) -> Vec<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    if reverse {
        let mut end = hay.len();
        let mut splits = 0;
        loop {
            if maxsplit >= 0 && splits >= maxsplit {
                break;
            }
            match bytes_find(hay, sep, 0, end, true) {
                Some(p) => {
                    parts.push(hay[p + sep.len()..end].to_vec());
                    end = p;
                    splits += 1;
                }
                None => break,
            }
        }
        parts.push(hay[..end].to_vec());
        parts.reverse();
    } else {
        let mut start = 0;
        let mut splits = 0;
        loop {
            if maxsplit >= 0 && splits >= maxsplit {
                break;
            }
            match bytes_find(hay, sep, start, hay.len(), false) {
                Some(p) => {
                    parts.push(hay[start..p].to_vec());
                    start = p + sep.len();
                    splits += 1;
                }
                None => break,
            }
        }
        parts.push(hay[start..].to_vec());
    }
    parts
}

/// Whitespace split (`sep is None`): runs of ASCII whitespace separate fields,
/// no empty fields, leading/trailing whitespace ignored. On hitting `maxsplit`
/// the remainder (leading whitespace already skipped) is one field with its
/// trailing whitespace preserved.
fn split_ws(hay: &[u8], maxsplit: i64, reverse: bool) -> Vec<Vec<u8>> {
    let n = hay.len();
    let mut parts: Vec<Vec<u8>> = Vec::new();
    if reverse {
        let mut end = n;
        let mut splits = 0;
        loop {
            while end > 0 && is_ascii_ws(hay[end - 1]) {
                end -= 1;
            }
            if end == 0 {
                break;
            }
            if maxsplit >= 0 && splits >= maxsplit {
                parts.push(hay[..end].to_vec());
                break;
            }
            let mut start = end;
            while start > 0 && !is_ascii_ws(hay[start - 1]) {
                start -= 1;
            }
            parts.push(hay[start..end].to_vec());
            end = start;
            splits += 1;
        }
        parts.reverse();
    } else {
        let mut i = 0;
        let mut splits = 0;
        loop {
            while i < n && is_ascii_ws(hay[i]) {
                i += 1;
            }
            if i >= n {
                break;
            }
            if maxsplit >= 0 && splits >= maxsplit {
                parts.push(hay[i..].to_vec());
                break;
            }
            let start = i;
            while i < n && !is_ascii_ws(hay[i]) {
                i += 1;
            }
            parts.push(hay[start..i].to_vec());
            splits += 1;
        }
    }
    parts
}

/// `bytes.fromhex(s)` / `bytearray.fromhex(s)`: parse hex digit pairs, skipping
/// runs of ASCII whitespace between bytes.
fn bytes_fromhex(args: &[Value]) -> Result<Vec<u8>, String> {
    let s = args
        .first()
        .and_then(|v| with_host(|h| h.as_str(v)))
        .ok_or_else(|| host::type_error("fromhex() argument must be str"))?;
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let hexval = |c: char| -> Option<u8> { c.to_digit(16).map(|d| d as u8) };
    while i < chars.len() {
        if chars[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let hi = hexval(chars[i]).ok_or_else(|| {
            format!("ValueError: non-hexadecimal number found in fromhex() arg at position {i}")
        })?;
        if i + 1 >= chars.len() || chars[i + 1].is_ascii_whitespace() {
            return Err(
                "ValueError: fromhex() arg must contain an even number of hexadecimal digits"
                    .into(),
            );
        }
        let lo = hexval(chars[i + 1]).ok_or_else(|| {
            format!(
                "ValueError: non-hexadecimal number found in fromhex() arg at position {}",
                i + 1
            )
        })?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

/// Decode bytes to a `str`. Only `utf-8` (default, strict) and `latin-1` /
/// `ascii` are recognized; the `errors` argument is not yet honored.
fn decode_bytes(bytes: &[u8], args: &[Value]) -> Result<Value, String> {
    let enc = args
        .first()
        .and_then(|v| with_host(|h| h.as_str(v)))
        .unwrap_or_else(|| "utf-8".into());
    let norm = enc.to_lowercase().replace(['-', '_'], "");
    let s = match norm.as_str() {
        "latin1" | "latin" | "iso88591" | "l1" | "cp1252" => {
            bytes.iter().map(|&b| b as char).collect::<String>()
        }
        "ascii" | "usascii" => {
            if bytes.iter().all(|&b| b < 0x80) {
                bytes.iter().map(|&b| b as char).collect::<String>()
            } else {
                return Err("UnicodeDecodeError: 'ascii' codec can't decode byte".into());
            }
        }
        _ => String::from_utf8(bytes.to_vec())
            .map_err(|_| "UnicodeDecodeError: 'utf-8' codec can't decode byte".to_string())?,
    };
    Ok(new_str(s))
}

fn bytes_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    bytes_common_method(recv, false, name, args)
}

fn bytearray_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    // bytearray-only mutators; everything else shares the bytes methods.
    match name {
        "append" => {
            let a0 = arg0(args)?;
            let n = with_host(|h| h.as_int(&a0))
                .ok_or_else(|| host::type_error("an integer is required"))?;
            if !(0..=255).contains(&n) {
                return Err("ValueError: byte must be in range(0, 256)".into());
            }
            with_host(|h| {
                if let Some(PyObj::Bytearray(b)) = h.get_mut(recv) {
                    b.push(n as u8);
                }
            });
            Ok(Value::Undef)
        }
        "extend" => {
            let add = arg0(args)?;
            let extra = collect_bytes(&add)?;
            with_host(|h| {
                if let Some(PyObj::Bytearray(b)) = h.get_mut(recv) {
                    b.extend_from_slice(&extra);
                }
            });
            Ok(Value::Undef)
        }
        "pop" => {
            let got = with_host(|h| match h.get_mut(recv) {
                Some(PyObj::Bytearray(b)) => b.pop(),
                _ => None,
            });
            got.map(|x| Value::Int(x as i64))
                .ok_or_else(|| "IndexError: pop from empty bytearray".into())
        }
        "clear" => {
            with_host(|h| {
                if let Some(PyObj::Bytearray(b)) = h.get_mut(recv) {
                    b.clear();
                }
            });
            Ok(Value::Undef)
        }
        _ => bytes_common_method(recv, true, name, args),
    }
}

/// The str-parallel `bytes`/`bytearray` methods. `is_ba` picks the return type
/// for methods that produce a new buffer (a `bytearray` receiver yields
/// `bytearray` results, mirroring CPython).
fn bytes_common_method(
    recv: &Value,
    is_ba: bool,
    name: &str,
    args: &[Value],
) -> Result<Value, String> {
    let bytes = recv_bytes(recv);
    let tname = if is_ba { "bytearray" } else { "bytes" };
    // A required bytes-like argument (bytes/bytearray, not an int).
    let need_sub = |i: usize| -> Result<Vec<u8>, String> {
        args.get(i)
            .and_then(as_bytes_object)
            .ok_or_else(|| host::type_error("a bytes-like object is required"))
    };
    // `find`/`rfind`/`index`/`count` accept an int (single byte) or bytes-like.
    let find_needle = || -> Result<Vec<u8>, String> {
        args.first()
            .and_then(arg_bytes_like)
            .ok_or_else(|| host::type_error("argument should be integer or bytes-like object"))
    };
    match name {
        "decode" => decode_bytes(&bytes, args),
        "hex" => Ok(new_str(bytes.iter().map(|b| format!("{b:02x}")).collect())),
        // `fromhex` is a classmethod but is also reachable through an instance.
        "fromhex" => Ok(mk_bytes(is_ba, bytes_fromhex(args)?)),
        "upper" => Ok(mk_bytes(is_ba, bytes.to_ascii_uppercase())),
        "lower" => Ok(mk_bytes(is_ba, bytes.to_ascii_lowercase())),
        "find" | "rfind" => {
            let sub = find_needle()?;
            let (start, end) = resolve_start_end(bytes.len(), args, 1);
            let p = bytes_find(&bytes, &sub, start, end, name == "rfind");
            Ok(Value::Int(p.map(|x| x as i64).unwrap_or(-1)))
        }
        "index" | "rindex" => {
            let sub = find_needle()?;
            let (start, end) = resolve_start_end(bytes.len(), args, 1);
            match bytes_find(&bytes, &sub, start, end, name == "rindex") {
                Some(p) => Ok(Value::Int(p as i64)),
                None => Err("ValueError: subsection not found".into()),
            }
        }
        "count" => {
            let sub = find_needle()?;
            let (start, end) = resolve_start_end(bytes.len(), args, 1);
            Ok(Value::Int(count_range(&bytes, &sub, start, end) as i64))
        }
        "startswith" | "endswith" => {
            let (start, end) = resolve_start_end(bytes.len(), args, 1);
            let region = &bytes[start..end.min(bytes.len())];
            let prefixes = match args.first() {
                Some(v) => bytes_prefix_tuple(v)?,
                None => return Err(host::type_error("startswith first arg must be bytes-like")),
            };
            let hit = prefixes.iter().any(|p| {
                if name == "startswith" {
                    region.starts_with(p)
                } else {
                    region.ends_with(p)
                }
            });
            Ok(Value::Bool(hit))
        }
        "split" | "rsplit" => {
            let reverse = name == "rsplit";
            let maxsplit = args
                .get(1)
                .and_then(|v| with_host(|h| h.as_int(v)))
                .unwrap_or(-1);
            let sep_arg = args.first().filter(|v| !matches!(v, Value::Undef));
            let parts = match sep_arg {
                None => split_ws(&bytes, maxsplit, reverse),
                Some(v) => {
                    let sep = as_bytes_object(v)
                        .ok_or_else(|| host::type_error("must be str or None, not int"))?;
                    if sep.is_empty() {
                        return Err("ValueError: empty separator".into());
                    }
                    split_on_sep(&bytes, &sep, maxsplit, reverse)
                }
            };
            let items: Vec<Value> = parts.into_iter().map(|p| mk_bytes(is_ba, p)).collect();
            Ok(with_host(|h| h.new_list(items)))
        }
        "splitlines" => {
            let keepends = args
                .first()
                .map(|v| with_host(|h| h.truthy(v)))
                .unwrap_or(false);
            let items: Vec<Value> = split_lines(&bytes, keepends)
                .into_iter()
                .map(|p| mk_bytes(is_ba, p))
                .collect();
            Ok(with_host(|h| h.new_list(items)))
        }
        "join" => {
            let sep = bytes.clone();
            let items = host::iter_vec(&arg0(args)?)?;
            let mut out = Vec::new();
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(&sep);
                }
                let piece = as_bytes_object(it).ok_or_else(|| {
                    host::type_error(&format!(
                        "sequence item {i}: expected a bytes-like object, {} found",
                        with_host(|h| h.type_name(it))
                    ))
                })?;
                out.extend_from_slice(&piece);
            }
            Ok(mk_bytes(is_ba, out))
        }
        "replace" => {
            let old = need_sub(0)?;
            let new = need_sub(1)?;
            let count = args
                .get(2)
                .and_then(|v| with_host(|h| h.as_int(v)))
                .unwrap_or(-1);
            Ok(mk_bytes(is_ba, replace_bytes(&bytes, &old, &new, count)))
        }
        "strip" | "lstrip" | "rstrip" => {
            let chars = args.first().filter(|v| !matches!(v, Value::Undef));
            let out = strip_bytes(&bytes, chars, name)?;
            Ok(mk_bytes(is_ba, out))
        }
        "partition" | "rpartition" => {
            let sep = need_sub(0)?;
            if sep.is_empty() {
                return Err("ValueError: empty separator".into());
            }
            let reverse = name == "rpartition";
            let (head, mid, tail) = match bytes_find(&bytes, &sep, 0, bytes.len(), reverse) {
                Some(p) => (
                    bytes[..p].to_vec(),
                    sep.clone(),
                    bytes[p + sep.len()..].to_vec(),
                ),
                None if reverse => (Vec::new(), Vec::new(), bytes.clone()),
                None => (bytes.clone(), Vec::new(), Vec::new()),
            };
            let t = vec![
                mk_bytes(is_ba, head),
                mk_bytes(is_ba, mid),
                mk_bytes(is_ba, tail),
            ];
            Ok(with_host(|h| h.new_tuple(t)))
        }
        "removeprefix" => {
            let pre = need_sub(0)?;
            let out = bytes
                .strip_prefix(pre.as_slice())
                .map(|s| s.to_vec())
                .unwrap_or(bytes);
            Ok(mk_bytes(is_ba, out))
        }
        "removesuffix" => {
            let suf = need_sub(0)?;
            let out = if suf.is_empty() {
                bytes
            } else {
                bytes
                    .strip_suffix(suf.as_slice())
                    .map(|s| s.to_vec())
                    .unwrap_or(bytes)
            };
            Ok(mk_bytes(is_ba, out))
        }
        _ => Err(format!(
            "AttributeError: '{tname}' object has no attribute '{name}'"
        )),
    }
}

/// A `startswith`/`endswith` first argument: a bytes-like, or a tuple of them.
fn bytes_prefix_tuple(v: &Value) -> Result<Vec<Vec<u8>>, String> {
    if let Some(b) = as_bytes_object(v) {
        return Ok(vec![b]);
    }
    let is_tuple = with_host(|h| matches!(h.get(v), Some(PyObj::Tuple(_))));
    if is_tuple {
        let items = host::iter_vec(v)?;
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            let b = as_bytes_object(&it)
                .ok_or_else(|| host::type_error("a bytes-like object is required"))?;
            out.push(b);
        }
        return Ok(out);
    }
    Err(host::type_error(
        "startswith first arg must be bytes or a tuple of bytes",
    ))
}

/// `bytes.replace(old, new[, count])` — non-overlapping, left to right. A
/// `count < 0` replaces every occurrence.
fn replace_bytes(hay: &[u8], old: &[u8], new: &[u8], count: i64) -> Vec<u8> {
    if count == 0 {
        return hay.to_vec();
    }
    // An empty `old` inserts `new` at each of the `len + 1` gaps (before every
    // byte and after the last), capped by `count`.
    if old.is_empty() {
        let slots = hay.len() + 1;
        let limit = if count < 0 {
            slots
        } else {
            (count as usize).min(slots)
        };
        let mut out = Vec::new();
        for (i, &b) in hay.iter().enumerate() {
            if i < limit {
                out.extend_from_slice(new);
            }
            out.push(b);
        }
        if hay.len() < limit {
            out.extend_from_slice(new);
        }
        return out;
    }
    let mut out = Vec::new();
    let mut i = 0;
    let mut done = 0i64;
    while i < hay.len() {
        if (count < 0 || done < count) && hay[i..].starts_with(old) {
            out.extend_from_slice(new);
            i += old.len();
            done += 1;
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// `strip`/`lstrip`/`rstrip`. `chars` is an optional set of byte values to
/// remove; `None`/absent strips ASCII whitespace.
fn strip_bytes(bytes: &[u8], chars: Option<&Value>, which: &str) -> Result<Vec<u8>, String> {
    let set: Option<Vec<u8>> = match chars {
        Some(v) => Some(
            as_bytes_object(v)
                .ok_or_else(|| host::type_error("a bytes-like object is required"))?,
        ),
        None => None,
    };
    let strip_c = |b: u8| -> bool {
        match &set {
            Some(s) => s.contains(&b),
            None => is_ascii_ws(b),
        }
    };
    let mut start = 0;
    let mut end = bytes.len();
    if which != "rstrip" {
        while start < end && strip_c(bytes[start]) {
            start += 1;
        }
    }
    if which != "lstrip" {
        while end > start && strip_c(bytes[end - 1]) {
            end -= 1;
        }
    }
    Ok(bytes[start..end].to_vec())
}

/// `bytes.splitlines(keepends=False)` — split on universal line boundaries
/// (`\n`, `\r`, `\r\n`). No trailing empty field for a final newline.
fn split_lines(bytes: &[u8], keepends: bool) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let n = bytes.len();
    let mut i = 0;
    let mut start = 0;
    while i < n {
        let c = bytes[i];
        if c == b'\n' || c == b'\r' {
            let mut brk = i + 1;
            if c == b'\r' && brk < n && bytes[brk] == b'\n' {
                brk += 1;
            }
            let end = if keepends { brk } else { i };
            out.push(bytes[start..end].to_vec());
            i = brk;
            start = brk;
        } else {
            i += 1;
        }
    }
    if start < n {
        out.push(bytes[start..].to_vec());
    }
    out
}

/// Build the byte content for a `bytes()` / `bytearray()` constructor call.
/// Handles `()` → empty, `int` → that many zero bytes, a bytes-like copy, a
/// `str` (with an optional encoding), and an iterable of ints.
fn build_bytes(args: &[Value]) -> Result<Vec<u8>, String> {
    let v = match args.first() {
        None => return Ok(vec![]),
        Some(v) => v,
    };
    if let Value::Int(n) = v {
        if *n < 0 {
            return Err("ValueError: negative count".into());
        }
        return Ok(vec![0u8; *n as usize]);
    }
    if let Some(b) = as_bytes_object(v) {
        return Ok(b);
    }
    if let Some(s) = with_host(|h| h.as_str(v)) {
        let enc = args
            .get(1)
            .and_then(|e| with_host(|h| h.as_str(e)))
            .map(|e| e.to_lowercase().replace(['-', '_'], ""));
        return match enc.as_deref() {
            Some("latin1") | Some("latin") | Some("iso88591") | Some("l1") => {
                Ok(s.chars().map(|c| c as u32 as u8).collect())
            }
            _ => Ok(s.into_bytes()),
        };
    }
    collect_bytes(v)
}

/// Collect a bytes-like / iterable-of-ints argument into raw bytes (for
/// `bytearray.extend`, `bytes(iterable)`, …).
fn collect_bytes(v: &Value) -> Result<Vec<u8>, String> {
    if let Some(b) = as_bytes_object(v) {
        return Ok(b);
    }
    let items = host::iter_vec(v)?;
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        let n = with_host(|h| h.as_int(&it))
            .ok_or_else(|| host::type_error("'int' object is required"))?;
        if !(0..=255).contains(&n) {
            return Err("ValueError: bytes must be in range(0, 256)".into());
        }
        out.push(n as u8);
    }
    Ok(out)
}

// ── collections.deque ────────────────────────────────────────────────────────

fn deque_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "append" => {
            let v = arg0(args)?;
            with_host(|h| {
                if let Some(PyObj::Deque { items, maxlen }) = h.get_mut(recv) {
                    items.push_back(v);
                    if let Some(m) = *maxlen {
                        while items.len() > m {
                            items.pop_front();
                        }
                    }
                }
            });
            Ok(Value::Undef)
        }
        "appendleft" => {
            let v = arg0(args)?;
            with_host(|h| {
                if let Some(PyObj::Deque { items, maxlen }) = h.get_mut(recv) {
                    items.push_front(v);
                    if let Some(m) = *maxlen {
                        while items.len() > m {
                            items.pop_back();
                        }
                    }
                }
            });
            Ok(Value::Undef)
        }
        "pop" => with_host(|h| match h.get_mut(recv) {
            Some(PyObj::Deque { items, .. }) => items.pop_back(),
            _ => None,
        })
        .ok_or_else(|| "IndexError: pop from an empty deque".into()),
        "popleft" => with_host(|h| match h.get_mut(recv) {
            Some(PyObj::Deque { items, .. }) => items.pop_front(),
            _ => None,
        })
        .ok_or_else(|| "IndexError: pop from an empty deque".into()),
        "extend" => {
            let add = host::iter_vec(&arg0(args)?)?;
            with_host(|h| {
                if let Some(PyObj::Deque { items, maxlen }) = h.get_mut(recv) {
                    for v in add {
                        items.push_back(v);
                        if let Some(m) = *maxlen {
                            while items.len() > m {
                                items.pop_front();
                            }
                        }
                    }
                }
            });
            Ok(Value::Undef)
        }
        "extendleft" => {
            let add = host::iter_vec(&arg0(args)?)?;
            with_host(|h| {
                if let Some(PyObj::Deque { items, maxlen }) = h.get_mut(recv) {
                    for v in add {
                        items.push_front(v);
                        if let Some(m) = *maxlen {
                            while items.len() > m {
                                items.pop_back();
                            }
                        }
                    }
                }
            });
            Ok(Value::Undef)
        }
        "rotate" => {
            let n = args
                .first()
                .and_then(|v| with_host(|h| h.as_int(v)))
                .unwrap_or(1);
            with_host(|h| {
                if let Some(PyObj::Deque { items, .. }) = h.get_mut(recv) {
                    if !items.is_empty() {
                        let len = items.len() as i64;
                        let k = ((n % len) + len) % len;
                        for _ in 0..k {
                            if let Some(x) = items.pop_back() {
                                items.push_front(x);
                            }
                        }
                    }
                }
            });
            Ok(Value::Undef)
        }
        "clear" => {
            with_host(|h| {
                if let Some(PyObj::Deque { items, .. }) = h.get_mut(recv) {
                    items.clear();
                }
            });
            Ok(Value::Undef)
        }
        "count" => {
            let v = arg0(args)?;
            Ok(Value::Int(with_host(|h| match h.get(recv) {
                Some(PyObj::Deque { items, .. }) => {
                    items.iter().filter(|x| h.equal(x, &v)).count() as i64
                }
                _ => 0,
            })))
        }
        "index" => {
            let v = arg0(args)?;
            with_host(|h| match h.get(recv) {
                Some(PyObj::Deque { items, .. }) => {
                    match items.iter().position(|x| h.equal(x, &v)) {
                        Some(p) => Ok(Value::Int(p as i64)),
                        None => Err(format!("ValueError: {} is not in deque", h.repr_of(&v))),
                    }
                }
                _ => Err(host::type_error("not a deque")),
            })
        }
        "remove" => {
            let v = arg0(args)?;
            let pos = with_host(|h| match h.get(recv) {
                Some(PyObj::Deque { items, .. }) => items.iter().position(|x| h.equal(x, &v)),
                _ => None,
            });
            match pos {
                Some(p) => {
                    with_host(|h| {
                        if let Some(PyObj::Deque { items, .. }) = h.get_mut(recv) {
                            items.remove(p);
                        }
                    });
                    Ok(Value::Undef)
                }
                None => Err("ValueError: deque.remove(x): x not in deque".into()),
            }
        }
        _ => Err(format!(
            "AttributeError: 'collections.deque' object has no attribute '{name}'"
        )),
    }
}

// ── collections dict subclasses (Counter / defaultdict / OrderedDict) ─────────

/// Methods specific to the `dict` subclasses. Returns `None` when `name` is a
/// plain-dict method (the caller then falls back to `dict_method`).
fn collections_dict_method(
    recv: &Value,
    name: &str,
    args: &[Value],
    tn: &str,
) -> Option<Result<Value, String>> {
    match (tn, name) {
        ("Counter", "most_common") => Some(counter_most_common(recv, args)),
        ("Counter", "elements") => Some(counter_elements(recv)),
        ("Counter", "total") => Some(Ok(Value::Int(with_host(|h| match h.get(recv) {
            Some(PyObj::Dict(d)) => d
                .values()
                .map(|(_, v)| h.as_int(v).unwrap_or(0))
                .sum::<i64>(),
            _ => 0,
        })))),
        ("Counter", "subtract") => Some(counter_add(recv, args, -1)),
        ("Counter", "update") => Some(counter_add(recv, args, 1)),
        ("OrderedDict", "move_to_end") => Some(ordered_move_to_end(recv, args)),
        _ => None,
    }
}

/// `Counter.most_common([n])` — `(element, count)` pairs, highest count first,
/// ties keeping insertion order (CPython uses a stable sort).
fn counter_most_common(recv: &Value, args: &[Value]) -> Result<Value, String> {
    let mut pairs: Vec<(Value, i64)> = with_host(|h| match h.get(recv) {
        Some(PyObj::Dict(d)) => d
            .values()
            .map(|(k, v)| (k.clone(), h.as_int(v).unwrap_or(0)))
            .collect(),
        _ => vec![],
    });
    pairs.sort_by_key(|p| std::cmp::Reverse(p.1)); // stable → ties keep insertion order
    let n = args.first().and_then(|v| with_host(|h| h.as_int(v)));
    if let Some(n) = n {
        pairs.truncate(n.max(0) as usize);
    }
    let tuples: Vec<Value> = with_host(|h| {
        pairs
            .into_iter()
            .map(|(k, c)| h.new_tuple(vec![k, Value::Int(c)]))
            .collect()
    });
    Ok(with_host(|h| h.new_list(tuples)))
}

/// `Counter.elements()` — each element repeated `count` times (counts <= 0 skipped).
fn counter_elements(recv: &Value) -> Result<Value, String> {
    let pairs: Vec<(Value, i64)> = with_host(|h| match h.get(recv) {
        Some(PyObj::Dict(d)) => d
            .values()
            .map(|(k, v)| (k.clone(), h.as_int(v).unwrap_or(0)))
            .collect(),
        _ => vec![],
    });
    let mut out = Vec::new();
    for (k, c) in pairs {
        for _ in 0..c.max(0) {
            out.push(k.clone());
        }
    }
    Ok(with_host(|h| h.new_list(out)))
}

/// `Counter.update(iterable_or_mapping)` / `.subtract(...)` with `sign` +1 / -1.
fn counter_add(recv: &Value, args: &[Value], sign: i64) -> Result<Value, String> {
    let other = match args.first() {
        Some(v) => v.clone(),
        None => return Ok(Value::Undef),
    };
    // A mapping contributes its counts; any other iterable contributes 1 each.
    let is_dict = with_host(|h| matches!(h.get(&other), Some(PyObj::Dict(_))));
    let deltas: Vec<(PKey, Value, i64)> = if is_dict {
        with_host(|h| match h.get(&other) {
            Some(PyObj::Dict(d)) => d
                .values()
                .map(|(k, v)| {
                    let key = h.to_key(k).unwrap_or(PKey::None);
                    (key, k.clone(), h.as_int(v).unwrap_or(0) * sign)
                })
                .collect(),
            _ => vec![],
        })
    } else {
        let items = host::iter_vec(&other)?;
        with_host(|h| {
            items
                .into_iter()
                .map(|it| {
                    let key = h.to_key(&it).unwrap_or(PKey::None);
                    (key, it, sign)
                })
                .collect()
        })
    };
    with_host(|h| {
        if let Some(PyObj::Dict(d)) = h.get_mut(recv) {
            for (key, kv, delta) in deltas {
                let entry = d.entry(key).or_insert_with(|| (kv.clone(), Value::Int(0)));
                let cur = match &entry.1 {
                    Value::Int(n) => *n,
                    _ => 0,
                };
                entry.1 = Value::Int(cur + delta);
            }
        }
    });
    Ok(Value::Undef)
}

/// `OrderedDict.move_to_end(key, last=True)`.
fn ordered_move_to_end(recv: &Value, args: &[Value]) -> Result<Value, String> {
    let kv = arg0(args)?;
    let key = with_host(|h| h.to_key(&kv))?;
    let last = args
        .get(1)
        .map(|v| with_host(|h| h.truthy(v)))
        .unwrap_or(true);
    let found = with_host(|h| {
        if let Some(PyObj::Dict(d)) = h.get_mut(recv) {
            if let Some((k, entry)) = d.shift_remove_entry(&key) {
                if last {
                    d.insert(k, entry);
                } else {
                    d.shift_insert(0, k, entry);
                }
                return true;
            }
        }
        false
    });
    if !found {
        return Err(format!("KeyError: {}", with_host(|h| h.repr_of(&kv))));
    }
    Ok(Value::Undef)
}

// ── collections constructors ─────────────────────────────────────────────────

/// Insert `key: val` (a `str` key) into a dict-backed target in place.
fn dict_insert_str(target: &Value, key: String, val: Value) -> Result<(), String> {
    let kv = new_str(key);
    let k = with_host(|h| h.to_key(&kv))?;
    with_host(|h| {
        if let Some(PyObj::Dict(d)) = h.get_mut(target) {
            d.insert(k, (kv, val));
        }
    });
    Ok(())
}

/// Fill a dict-backed target from a mapping or an iterable of `(key, value)`
/// pairs (`dict()`-style initialization).
fn fill_dict_from(target: &Value, src: &Value) -> Result<(), String> {
    if matches!(src, Value::Undef) {
        return Ok(());
    }
    let is_dict = with_host(|h| matches!(h.get(src), Some(PyObj::Dict(_))));
    if is_dict {
        let pairs = with_host(|h| match h.get(src) {
            Some(PyObj::Dict(d)) => d
                .iter()
                .map(|(k, (kv, v))| (k.clone(), kv.clone(), v.clone()))
                .collect::<Vec<_>>(),
            _ => vec![],
        });
        with_host(|h| {
            if let Some(PyObj::Dict(d)) = h.get_mut(target) {
                for (k, kv, v) in pairs {
                    host::dict_put(d, k, kv, v);
                }
            }
        });
    } else {
        let items = host::iter_vec(src)?;
        for it in items {
            let pair = host::iter_vec(&it)?;
            if pair.len() != 2 {
                return Err(host::type_error(
                    "dictionary update sequence element has length != 2",
                ));
            }
            let k = with_host(|h| h.to_key(&pair[0]))?;
            let (kv, v) = (pair[0].clone(), pair[1].clone());
            with_host(|h| {
                if let Some(PyObj::Dict(d)) = h.get_mut(target) {
                    host::dict_put(d, k, kv, v);
                }
            });
        }
    }
    Ok(())
}

/// Construct a `collections` type: `deque` / `Counter` / `defaultdict` /
/// `OrderedDict` / `namedtuple`.
fn construct_collection(
    kind: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    match kind {
        "deque" => {
            let mut items: std::collections::VecDeque<Value> = match args.first() {
                Some(v) if !matches!(v, Value::Undef) => {
                    std::collections::VecDeque::from(host::iter_vec(v)?)
                }
                _ => std::collections::VecDeque::new(),
            };
            let maxlen = match args.get(1) {
                Some(v) if !matches!(v, Value::Undef) => {
                    with_host(|h| h.as_int(v)).map(|n| n.max(0) as usize)
                }
                _ => None,
            };
            if let Some(m) = maxlen {
                while items.len() > m {
                    items.pop_front();
                }
            }
            Ok(host::alloc_deque(items, maxlen))
        }
        "Counter" => {
            let c = host::alloc_dict_subtype(IndexMap::new(), host::DictKind::Counter, None);
            if let Some(v) = args.first() {
                if !matches!(v, Value::Undef) {
                    counter_add(&c, std::slice::from_ref(v), 1)?;
                }
            }
            for (k, v) in kwargs {
                let cnt = with_host(|h| h.as_int(&v)).unwrap_or(0);
                dict_insert_str(&c, k, Value::Int(cnt))?;
            }
            Ok(c)
        }
        "defaultdict" => {
            // A dict first-arg is initial data, not a factory.
            let factory = match args.first() {
                None => None,
                Some(Value::Undef) => None,
                Some(v) if with_host(|h| matches!(h.get(v), Some(PyObj::Dict(_)))) => None,
                Some(v) => Some(v.clone()),
            };
            let dd =
                host::alloc_dict_subtype(IndexMap::new(), host::DictKind::DefaultDict, factory);
            for v in &args {
                if with_host(|h| matches!(h.get(v), Some(PyObj::Dict(_)))) {
                    fill_dict_from(&dd, v)?;
                }
            }
            for (k, v) in kwargs {
                dict_insert_str(&dd, k, v)?;
            }
            Ok(dd)
        }
        "OrderedDict" => {
            let od = host::alloc_dict_subtype(IndexMap::new(), host::DictKind::OrderedDict, None);
            if let Some(v) = args.first() {
                fill_dict_from(&od, v)?;
            }
            for (k, v) in kwargs {
                dict_insert_str(&od, k, v)?;
            }
            Ok(od)
        }
        "namedtuple" => {
            let tname = args
                .first()
                .and_then(|v| with_host(|h| h.as_str(v)))
                .ok_or_else(|| {
                    host::type_error(
                        "namedtuple() missing 1 required positional argument: 'typename'",
                    )
                })?;
            let fields: Vec<String> = match args.get(1) {
                Some(v) => {
                    if let Some(s) = with_host(|h| h.as_str(v)) {
                        s.replace(',', " ")
                            .split_whitespace()
                            .map(|x| x.to_string())
                            .collect()
                    } else {
                        host::iter_vec(v)?
                            .iter()
                            .filter_map(|it| with_host(|h| h.as_str(it)))
                            .collect()
                    }
                }
                None => vec![],
            };
            Ok(host::make_namedtuple_type(&tname, fields))
        }
        _ => Err(host::name_error(&format!("collections.{kind}"))),
    }
}

// ── functools.lru_cache wrapper ──────────────────────────────────────────────

fn lru_wrapper_method(recv: &Value, name: &str) -> Result<Value, String> {
    match name {
        "cache_info" => {
            let (hits, misses, maxsize, currsize) =
                host::lru_cache_info(recv).unwrap_or((0, 0, None, 0));
            let ms = maxsize
                .map(|n| Value::Int(n as i64))
                .unwrap_or(Value::Undef);
            Ok(with_host(|h| {
                h.new_tuple(vec![
                    Value::Int(hits as i64),
                    Value::Int(misses as i64),
                    ms,
                    Value::Int(currsize as i64),
                ])
            }))
        }
        "cache_clear" => {
            host::lru_cache_clear(recv);
            Ok(Value::Undef)
        }
        _ => Err(format!(
            "AttributeError: 'functools._lru_cache_wrapper' object has no attribute '{name}'"
        )),
    }
}

// ── file objects ─────────────────────────────────────────────────────────────

fn file_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let id = match with_host(|h| match h.get(recv) {
        Some(PyObj::File { id }) => Some(*id),
        _ => None,
    }) {
        Some(id) => id,
        None => return Err(host::type_error("not a file")),
    };
    match name {
        "read" => {
            let s = with_host(|h| h.io_read_all(id))?;
            Ok(new_str(s))
        }
        "readline" => {
            let s = with_host(|h| h.io_readline(id))?;
            Ok(new_str(s))
        }
        "readlines" => {
            let lines = with_host(|h| h.io_read_lines(id))?;
            let vals: Vec<Value> = with_host(|h| lines.into_iter().map(|l| h.new_str(l)).collect());
            Ok(with_host(|h| h.new_list(vals)))
        }
        "write" => {
            let s = arg0(args)?;
            match as_bytes_object(&s) {
                Some(bytes) => with_host(|h| h.io_write_bytes(id, &bytes)),
                None => {
                    let txt = with_host(|h| h.str_of(&s));
                    with_host(|h| h.io_write(id, &txt))
                }
            }
        }
        "writelines" => {
            let items = host::iter_vec(&arg0(args)?)?;
            for it in items {
                match as_bytes_object(&it) {
                    Some(bytes) => {
                        with_host(|h| h.io_write_bytes(id, &bytes))?;
                    }
                    None => {
                        let txt = with_host(|h| h.str_of(&it));
                        with_host(|h| h.io_write(id, &txt))?;
                    }
                }
            }
            Ok(Value::Undef)
        }
        "close" => {
            with_host(|h| h.io_close(id));
            Ok(Value::Undef)
        }
        "flush" => {
            with_host(|h| h.io_flush(id))?;
            Ok(Value::Undef)
        }
        "readable" => Ok(Value::Bool(true)),
        "writable" => Ok(Value::Bool(true)),
        "seekable" => Ok(Value::Bool(true)),
        "__enter__" => Ok(recv.clone()),
        "__exit__" => {
            with_host(|h| h.io_close(id));
            Ok(Value::Bool(false))
        }
        _ => Err(format!(
            "AttributeError: '_io.TextIOWrapper' object has no attribute '{name}'"
        )),
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
    let mut alt = false;
    if i < chars.len() && chars[i] == '#' {
        alt = true;
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
    let mut body =
        match ty {
            'd' => match with_host(|h| h.big_val(v)) {
                Some(n) => n.to_string(),
                None => s.to_string(),
            },
            'f' | 'F' => {
                let f = as_f(v).unwrap_or(0.0);
                format!("{:.*}", prec.unwrap_or(6), f)
            }
            'e' | 'E' => {
                let f = as_f(v).unwrap_or(0.0);
                crate::host::fmt_sci(f, prec.unwrap_or(6), ty == 'E')
            }
            'g' | 'G' => {
                let f = as_f(v).unwrap_or(0.0);
                crate::host::fmt_g(f, prec.unwrap_or(6), ty == 'G', alt)
            }
            'c' => match as_i(v) {
                Some(n) => char::from_u32(n as u32)
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
                None => s.to_string(),
            },
            'x' => fmt_int_radix(v, 16, if alt { "0x" } else { "" }, false)
                .unwrap_or_else(|| s.to_string()),
            'X' => fmt_int_radix(v, 16, if alt { "0X" } else { "" }, true)
                .unwrap_or_else(|| s.to_string()),
            'o' => fmt_int_radix(v, 8, if alt { "0o" } else { "" }, false)
                .unwrap_or_else(|| s.to_string()),
            'b' => fmt_int_radix(v, 2, if alt { "0b" } else { "" }, false)
                .unwrap_or_else(|| s.to_string()),
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
    // A `+` or space sign flag adds a leading `+`/space to a non-negative value
    // (a negative already carries its own `-`).
    if matches!(sign, '+' | ' ') && as_f(v).map(|f| f >= 0.0).unwrap_or(false) {
        body = format!("{sign}{body}");
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
            // Sign-aware pad: the fill goes AFTER the sign (`+`/`-`/space) and any
            // radix prefix (`0x`/`0o`/`0b`), so `+05d` of 5 → `+0005` and `#08x` of
            // -255 → `-0x000ff`.
            let cs: Vec<char> = body.chars().collect();
            let mut k = 0;
            if cs.first().is_some_and(|c| matches!(c, '+' | '-' | ' ')) {
                k = 1;
            }
            if cs.len() >= k + 2
                && cs[k] == '0'
                && matches!(cs[k + 1], 'x' | 'X' | 'o' | 'O' | 'b' | 'B')
            {
                k += 2;
            }
            let head: String = cs[..k].iter().collect();
            let tail: String = cs[k..].iter().collect();
            format!("{head}{}{tail}", fill.to_string().repeat(pad))
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

/// Format an integer (bignum-safe) in `radix` as sign + optional prefix +
/// magnitude, matching CPython's `format(n, 'x'/'o'/'b')` (e.g. `-ff`, `-0b111`)
/// rather than Rust's two's-complement `{:x}`/`{:b}`. Returns None for non-ints.
fn fmt_int_radix(v: &Value, radix: u32, prefix: &str, upper: bool) -> Option<String> {
    if matches!(v, Value::Float(_)) {
        return None;
    }
    let n = with_host(|h| h.big_val(v))?;
    use num_bigint::Sign;
    let sign = if n.sign() == Sign::Minus { "-" } else { "" };
    let mut body = n.magnitude().to_str_radix(radix);
    if upper {
        body = body.to_uppercase();
    }
    Some(format!("{sign}{prefix}{body}"))
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
