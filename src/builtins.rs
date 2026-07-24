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
    vm.register_builtin(ops::EXTEND_LIST, b_extend_list);
    vm.register_builtin(ops::EXTEND_TUPLE, b_extend_tuple);
    vm.register_builtin(ops::EXTEND_SET, b_extend_set);
    vm.register_builtin(ops::EXTEND_DICT, b_extend_dict);
    vm.register_builtin(ops::EXTEND_STR, b_extend_str);
    vm.register_builtin(ops::ELLIPSIS, b_ellipsis);
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
    vm.register_builtin(ops::YIELD_FROM, b_yield_from);
    vm.register_builtin(ops::AWAIT, b_await);
    vm.register_builtin(ops::CONTAINS, b_contains);
    vm.register_builtin(ops::IS, b_is);
    vm.register_builtin(ops::RAISE, b_raise);
    vm.register_builtin(ops::RERAISE, b_reraise);
    vm.register_builtin(ops::SIG_RETURN, b_sig_return);
    vm.register_builtin(ops::SIG_BREAK, b_sig_break);
    vm.register_builtin(ops::SIG_CONTINUE, b_sig_continue);
    vm.register_builtin(ops::LOOP_BODY, b_loop_body);
    vm.register_builtin(ops::IMPORT, b_import);
    vm.register_builtin(ops::IMPORT_FROM, b_import_from);
    vm.register_builtin(ops::IMPORT_STAR, b_import_star);
    vm.register_builtin(ops::UNPACK, b_unpack);
    vm.register_builtin(ops::BINOP, b_binop);
    vm.register_builtin(ops::INPLACE, b_inplace);
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
    vm.register_builtin(ops::DISPLAYHOOK, b_displayhook);
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
pub(crate) fn record_err_line(vm: &VM) {
    let idx = vm.ip.saturating_sub(1);
    if let Some(&line) = vm.chunk.lines.get(idx) {
        if line != 0 {
            // Look the failing op's caret span up in the position table registered
            // for this chunk (keyed by its `op_hash`); `Span::NONE` if none.
            let span = crate::host::lookup_position(vm.chunk.op_hash, idx);
            with_host(|h| h.set_cur_line_span(line, span));
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

fn b_ellipsis(_vm: &mut VM, _: u8) -> Value {
    // `...` — the `Ellipsis` singleton (distinct from `None`).
    with_host(|h| h.alloc(PyObj::Ellipsis))
}

fn b_getlocal(vm: &mut VM, _: u8) -> Value {
    let name = sval(&vm.pop());
    match with_host(|h| h.read_name_checked(&name)) {
        host::NameRead::Value(v) => return v,
        host::NameRead::Unbound => return abort(vm, host::unbound_local_error(&name)),
        host::NameRead::Missing => {}
    }
    if name == "NotImplemented" {
        return with_host(|h| h.alloc(PyObj::NotImplemented));
    }
    if name == "Ellipsis" {
        return with_host(|h| h.alloc(PyObj::Ellipsis));
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
    if name == "Ellipsis" {
        return with_host(|h| h.alloc(PyObj::Ellipsis));
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

/// Whether `recv` is an instance whose class (user code, not `object`'s
/// defaults) defines the dunder `name` — used to detect `__getattribute__` /
/// `__setattr__` / `__delattr__` / `__getattr__` overrides.
fn instance_dunder(recv: &Value, name: &str) -> bool {
    with_host(|h| match h.get(recv) {
        Some(PyObj::Instance(i)) => h.class_has(&i.class, name),
        _ => false,
    })
}

/// Whether an error string is an `AttributeError` (the only error that triggers
/// the `__getattr__` fallback / that `hasattr` swallows).
fn is_attr_err(e: &str) -> bool {
    e.starts_with("AttributeError")
}

/// `recv.name` — the full attribute read protocol. A user `__getattribute__`
/// intercepts every access; otherwise the default descriptor-aware lookup runs.
/// Either way, an `AttributeError` triggers the `__getattr__` fallback if the
/// class defines one. Accessor bodies run user code, so no host borrow is held.
fn get_attr_desc(recv: &Value, name: &str) -> Result<Value, String> {
    let res = if instance_dunder(recv, "__getattribute__") {
        let nm = with_host(|h| h.new_str(name.to_string()));
        host::call_method(recv, "__getattribute__", vec![nm], vec![])
    } else {
        raw_getattr(recv, name)
    };
    match res {
        Ok(v) => Ok(v),
        // `__getattr__` fallback: fires only when the lookup raised
        // AttributeError (a property/descriptor raising it counts, per CPython).
        Err(e) if is_attr_err(&e) && instance_dunder(recv, "__getattr__") => {
            let nm = with_host(|h| h.new_str(name.to_string()));
            host::call_method(recv, "__getattr__", vec![nm], vec![])
        }
        Err(e) => Err(e),
    }
}

/// The default `object.__getattribute__`: descriptor-aware lookup (data
/// descriptor > instance dict > non-data descriptor / class attr). Does NOT
/// consult a user `__getattribute__` override or the `__getattr__` fallback.
pub(crate) fn raw_getattr(recv: &Value, name: &str) -> Result<Value, String> {
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
        host::AttrGet::CachedProperty { func, inst, name } => {
            // Compute `func(inst)` (as a bound method so `self`/`super()` work),
            // cache it in the instance dict, and return it. Runs outside the host
            // borrow so the getter body can re-enter freely.
            let value = match with_host(|h| h.get(&func).cloned()) {
                Some(PyObj::Func(fv)) => {
                    host::run_user_func(&fv, Some(inst.clone()), None, vec![], vec![])?
                }
                _ => host::invoke(&func, vec![inst.clone()], vec![])?,
            };
            // Cache into the instance dict. A `__slots__` instance with no dict
            // can't cache — CPython raises this exact `TypeError`.
            if with_host(|h| h.set_attr(&inst, &name, value.clone())).is_err() {
                let cls = with_host(|h| h.type_name(&inst));
                return Err(host::type_error(&format!(
                    "No '__dict__' attribute on '{cls}' instance to cache '{name}' property."
                )));
            }
            Ok(value)
        }
        host::AttrGet::Plain => with_host(|h| h.get_attr(recv, name)),
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

/// `recv.name = val` — a user `__setattr__` intercepts every store; otherwise
/// the default descriptor-aware assignment runs.
fn set_attr_desc(recv: &Value, name: &str, val: Value) -> Result<(), String> {
    if instance_dunder(recv, "__setattr__") {
        let nm = with_host(|h| h.new_str(name.to_string()));
        return host::call_method(recv, "__setattr__", vec![nm, val], vec![]).map(|_| ());
    }
    raw_setattr(recv, name, val)
}

/// The default `object.__setattr__`: the data-descriptor protocol
/// (`property.fset`, user `__set__`) then a plain instance-dict store
/// (honoring `__slots__`). Does not consult a user `__setattr__`.
pub(crate) fn raw_setattr(recv: &Value, name: &str, val: Value) -> Result<(), String> {
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
    let r = del_attr_desc(&recv, &name);
    match r {
        Ok(()) => Value::Undef,
        Err(e) => abort(vm, e),
    }
}

/// `del recv.name` — a user `__delattr__` intercepts every deletion; otherwise
/// the default descriptor-aware deletion runs.
fn del_attr_desc(recv: &Value, name: &str) -> Result<(), String> {
    if instance_dunder(recv, "__delattr__") {
        let nm = with_host(|h| h.new_str(name.to_string()));
        return host::call_method(recv, "__delattr__", vec![nm], vec![]).map(|_| ());
    }
    raw_delattr(recv, name)
}

/// The default `object.__delattr__`: the data-descriptor protocol
/// (`property.fdel`, user `__delete__`) then a plain instance-dict deletion.
/// Does not consult a user `__delattr__`.
pub(crate) fn raw_delattr(recv: &Value, name: &str) -> Result<(), String> {
    match with_host(|h| h.plan_attr_del(recv, name)) {
        host::AttrDel::Property { fdel, inst, owner } => {
            if matches!(fdel, Value::Undef) {
                let cls = with_host(|h| h.type_name(&inst));
                return Err(format!(
                    "AttributeError: property '{name}' of '{cls}' object has no deleter"
                ));
            }
            match with_host(|h| h.get(&fdel).cloned()) {
                Some(PyObj::Func(fv)) => {
                    host::run_user_func(&fv, Some(inst), owner, vec![], vec![]).map(|_| ())
                }
                _ => host::invoke(&fdel, vec![inst], vec![]).map(|_| ()),
            }
        }
        host::AttrDel::Descriptor {
            desc,
            inst,
            has_delete,
        } => {
            if !has_delete {
                return Err("AttributeError: __delete__".to_string());
            }
            host::call_method(&desc, "__delete__", vec![inst], vec![]).map(|_| ())
        }
        host::AttrDel::Plain => with_host(|h| h.del_attr(recv, name)),
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
    // `Cls[item]` on a class with `__class_getitem__` (e.g. generic aliases).
    if let Some(r) = host::class_getitem(&recv, idx.clone()) {
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
    // A non-dict index that is an instance with `__index__` stands in for an int
    // (`seq[obj]`). Resolve it before indexing; dict lookups key on the object.
    // A `slice` bound with `__index__` is resolved the same way (`a[Idx():Idx()]`).
    let idx = if with_host(|h| !matches!(h.get(&recv), Some(PyObj::Dict(_)))) {
        if with_host(|h| matches!(h.get(&idx), Some(PyObj::Slice { .. }))) {
            match normalize_slice_bounds(&idx) {
                Ok(v) => v,
                Err(e) => return abort(vm, e),
            }
        } else {
            match index_dunder(&idx) {
                Ok(Some(v)) => v,
                Ok(None) => idx,
                Err(e) => return abort(vm, e),
            }
        }
    } else {
        idx
    };
    // A CPython `Foreign` object (stdlib-ffi): run `recv[idx]` (its `__getitem__`)
    // OUTSIDE the borrow so a `@dataclass` with a user `__getitem__` re-enters.
    #[cfg(feature = "stdlib-ffi")]
    if let Some(id) = with_host(|h| h.foreign_id(&recv)) {
        return finish(vm, crate::ffi::get_item_cb(id, &idx));
    }
    // Subscripting a TYPE object is generic parameterization, not indexing:
    // `list[int]`, `dict[str, int]`, `Cls[T]` -> a `types.GenericAlias`. Handle it
    // here (outside the `get_item` borrow) because it can re-enter the VM.
    if host::is_generic_subscriptable(&recv) {
        return finish(vm, host::generic_alias(&recv, &idx));
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
    // of any host borrow (it may be a generator), then splice. Slice bounds with
    // `__index__` are resolved to ints first (`a[Idx():Idx()] = it`).
    if with_host(|h| matches!(h.get(&idx), Some(PyObj::Slice { .. }))) {
        let idx = match normalize_slice_bounds(&idx) {
            Ok(v) => v,
            Err(e) => return abort(vm, e),
        };
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
    // `del obj[i]` on an instance dispatches to `__delitem__` (raw index/slice).
    if with_host(|h| matches!(h.get(&recv), Some(PyObj::Instance(_)))) {
        let r = host::call_method(&recv, "__delitem__", vec![idx], vec![]).map(|_| Value::Undef);
        return finish(vm, r);
    }
    // `del seq[Idx():Idx()]` — resolve `__index__` slice bounds (recv is a
    // builtin sequence here; instances were dispatched to `__delitem__` above).
    let idx = if with_host(|h| matches!(h.get(&idx), Some(PyObj::Slice { .. }))) {
        match normalize_slice_bounds(&idx) {
            Ok(v) => v,
            Err(e) => return abort(vm, e),
        }
    } else {
        idx
    };
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
    // `host::str_of` (free fn) prefetches any foreign `__str__` outside the borrow,
    // so a `Foreign` with a user-defined dunder can't re-enter mid-format.
    for p in &parts {
        s.push_str(&host::str_of(p));
    }
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

// ── chunked-build extends ────────────────────────────────────────────────────
// A collection literal with more stack slots than a u8 argc can name is built in
// ≤255-slot chunks: `MK*` makes the accumulator from the first chunk, then each
// `EXTEND_*` op below folds the next chunk into it. Stack on entry is
// [acc, items...] (bottom-to-top), so `pop_n` yields the accumulator first.

fn b_extend_list(vm: &mut VM, argc: u8) -> Value {
    let mut items = pop_n(vm, argc as usize);
    let acc = items.remove(0);
    with_host(|h| {
        if let Some(PyObj::List(l)) = h.get_mut(&acc) {
            l.extend(items);
        }
    });
    acc
}

fn b_extend_tuple(vm: &mut VM, argc: u8) -> Value {
    let mut items = pop_n(vm, argc as usize);
    let acc = items.remove(0);
    let mut all: Vec<Value> = with_host(|h| match h.get(&acc) {
        Some(PyObj::Tuple(t)) => t.clone(),
        _ => Vec::new(),
    });
    all.extend(items);
    with_host(|h| h.new_tuple(all))
}

fn b_extend_set(vm: &mut VM, argc: u8) -> Value {
    let mut items = pop_n(vm, argc as usize);
    let acc = items.remove(0);
    let mut set: IndexMap<PKey, Value> = with_host(|h| match h.get(&acc) {
        Some(PyObj::Set(s)) => s.clone(),
        _ => IndexMap::new(),
    });
    for it in items {
        let cands = host::set_local_candidates(&set);
        let key = host::with_instance_key(&it, &cands, || with_host(|h| h.to_key(&it)));
        match key {
            Ok(k) => host::set_put(&mut set, k, it),
            Err(e) => return abort(vm, e),
        }
    }
    with_host(|h| {
        if let Some(PyObj::Set(s)) = h.get_mut(&acc) {
            *s = set;
        }
    });
    acc
}

fn b_extend_dict(vm: &mut VM, argc: u8) -> Value {
    let flat = pop_n(vm, argc as usize);
    let acc = flat[0].clone();
    let mut d: IndexMap<PKey, (Value, Value)> = with_host(|h| match h.get(&acc) {
        Some(PyObj::Dict(m)) => m.clone(),
        _ => IndexMap::new(),
    });
    let mut i = 1;
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
    with_host(|h| {
        if let Some(PyObj::Dict(m)) = h.get_mut(&acc) {
            *m = d;
        }
    });
    acc
}

fn b_extend_str(vm: &mut VM, argc: u8) -> Value {
    let mut parts = pop_n(vm, argc as usize);
    let acc = parts.remove(0);
    let mut s = with_host(|h| h.str_of(&acc));
    with_host(|h| {
        for p in &parts {
            s.push_str(&h.str_of(p));
        }
    });
    with_host(|h| h.new_str(s))
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

/// `yield from E` (PEP 380). Resolve `E` to an iterator (an instance `__iter__`
/// may return a lazy generator), then delegate: re-yield each value, forwarding
/// sent values / thrown exceptions / close into the sub-iterator, and leave the
/// sub-iterator's return value (its `StopIteration.value`) on the stack.
fn b_yield_from(vm: &mut VM, _: u8) -> Value {
    let v = vm.pop();
    let it = if with_host(|h| matches!(h.get(&v), Some(PyObj::Instance(_)))) {
        match iter_instance(&v) {
            Ok(it) => it,
            Err(e) => return abort(vm, e),
        }
    } else {
        match with_host(|h| h.make_iter(&v)) {
            Ok(it) => it,
            Err(e) => return abort(vm, e),
        }
    };
    match host::run_yield_from(it) {
        Ok(ret) => ret,
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
    if let Some(a) = check_kw_dup(vm, &with_host(|h| h.call_display_name(&name))) {
        return a;
    }
    let r = host::call_named(&name, list_args(&argl), kw_pairs(&kwd));
    finish(vm, r)
}

fn b_call_value_ex(vm: &mut VM, _: u8) -> Value {
    let kwd = vm.pop();
    let argl = vm.pop();
    let callable = vm.pop();
    if let Some(a) = check_kw_dup(vm, &host::callable_display_name(&callable)) {
        return a;
    }
    let r = host::invoke(&callable, list_args(&argl), kw_pairs(&kwd));
    finish(vm, r)
}

fn b_call_method_ex(vm: &mut VM, _: u8) -> Value {
    let kwd = vm.pop();
    let argl = vm.pop();
    let name = sval(&vm.pop());
    let recv = vm.pop();
    if let Some(a) = check_kw_dup(vm, &name) {
        return a;
    }
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
///
/// Unlike a `{**a, **b}` dict display, a call's keyword merge rejects a repeated
/// key (`f(**a, **b)` / `f(k=v, **{'k': ...})`). We can't name the callable yet,
/// so the first collision is stashed in `pending_kw_dup` for the following
/// `CALL_*_EX` handler to raise with the correct `<callable>() got multiple
/// values for keyword argument '<k>'` message.
fn b_build_kwargs(vm: &mut VM, argc: u8) -> Value {
    let flat = pop_n(vm, argc as usize);
    let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
    let mut dup: Option<String> = None;
    let mut note_dup = |k: &PKey, seen: &IndexMap<PKey, (Value, Value)>| {
        if dup.is_none() && seen.contains_key(k) {
            if let PKey::Str(s) = k {
                dup = Some(s.clone());
            }
        }
    };
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
                note_dup(&k, &d);
                host::dict_put(&mut d, k, kv, v);
            }
        } else {
            let kstr = sval(&key);
            let kv = with_host(|h| h.new_str(kstr.clone()));
            let pk = PKey::Str(kstr);
            note_dup(&pk, &d);
            d.insert(pk, (kv, val));
        }
        i += 2;
    }
    if let Some(k) = dup {
        with_host(|h| h.pending_kw_dup = Some(k));
    }
    with_host(|h| h.new_dict(d))
}

/// If the just-built kwargs carried a duplicate key, abort with CPython's
/// `<callable>() got multiple values for keyword argument '<k>'`. `disp` is the
/// already-formatted callable name (`__main__.f`, `dict`, …). Returns the abort
/// sentinel when a duplicate was pending, else `None` to continue the call.
fn check_kw_dup(vm: &mut VM, disp: &str) -> Option<Value> {
    let dup = with_host(|h| h.pending_kw_dup.take());
    dup.map(|k| {
        abort(
            vm,
            host::type_error(&format!(
                "{disp}() got multiple values for keyword argument '{k}'"
            )),
        )
    })
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
    // A CPython `Foreign` object (stdlib-ffi): run its `__bool__`/`__len__` in
    // CPython OUTSIDE the borrow so a `@dataclass` with a user dunder can re-enter.
    #[cfg(feature = "stdlib-ffi")]
    if let Some(fid) = with_host(|h| h.foreign_id(v)) {
        return Ok(crate::ffi::truthy(fid));
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
    if h.class_lookup(&i.class, name).is_some() {
        return true;
    }
    // A builtin-type subclass responds to the protocol dunders its base
    // provides (`__len__`/`__getitem__`/`__iter__`/`__repr__`/…) via delegation.
    if !matches!(i.payload, Value::Undef) {
        if let Some(base) = h.builtin_base_of(&i.class) {
            return host::base_provides(base, name);
        }
    }
    false
}

/// Whether `r` is an integer value (`int`/`bool`/bignum) — the required return
/// type of `__index__`/`__int__`.
fn is_int_value(r: &Value) -> bool {
    with_host(|h| match r {
        Value::Int(_) | Value::Bool(_) => true,
        Value::Obj(_) => matches!(h.get(r), Some(PyObj::BigInt(_))),
        _ => false,
    })
}

/// CPython's `__index__` protocol: if `v` is an instance defining `__index__`,
/// call it and require an integer result. Returns `Ok(Some(int))` when resolved,
/// `Ok(None)` when `v` is not an instance with `__index__`, `Err(..)` when the
/// method raised or returned a non-int. Used by `bin`/`hex`/`oct` and sequence
/// indexing, where any object may stand in for an integer.
fn index_dunder(v: &Value) -> Result<Option<Value>, String> {
    let has = with_host(
        |h| matches!(h.get(v), Some(PyObj::Instance(i)) if instance_has(h, i, "__index__")),
    );
    if !has {
        return Ok(None);
    }
    let r = host::call_method(v, "__index__", vec![], vec![])?;
    if !is_int_value(&r) {
        let t = with_host(|h| h.type_name(&r));
        return Err(host::type_error(&format!(
            "__index__ returned non-int (type {t})"
        )));
    }
    Ok(Some(r))
}

fn b_tostr(vm: &mut VM, _: u8) -> Value {
    let v = vm.pop();
    stringify(vm, &v, false)
}

/// CPython `sys.displayhook`: the interactive REPL feeds each top-level
/// expression-statement value here. `None` is neither printed nor bound; any
/// other value is `repr`-printed to stdout and bound to the module global `_`.
/// Only emitted for interactive compiles (`compile_interactive`), never scripts.
fn b_displayhook(vm: &mut VM, _: u8) -> Value {
    let v = vm.pop();
    if matches!(v, Value::Undef) {
        return Value::Undef;
    }
    let s = match py_repr(&v) {
        Ok(s) => s,
        Err(e) => return abort(vm, e),
    };
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(s.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
    with_host(|h| h.set_global("_", v));
    Value::Undef
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
    let s = if repr { host::repr_of(v) } else { host::str_of(v) };
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
        return apply_format_spec(&s, &sv, spec);
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
    apply_format_spec(&s, v, spec)
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
    // Stack layout (bottom→top): annotations_dict, pos_defaults…, kw_defaults…,
    // kw_count, func_id.
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
    // The `__annotations__` dict is the deepest arg (always present — the compiler
    // emits an empty dict for an unannotated func); the rest are positional
    // defaults, in order.
    let annotations = if args.is_empty() {
        Value::Undef
    } else {
        args.remove(0)
    };
    let defaults = args;
    let env = with_host(|h| h.current_env_capture());
    with_host(|h| {
        h.alloc(PyObj::Func(host::FuncVal {
            def_id,
            module: h.cur_module(),
            env: Some(env),
            defaults,
            kwonly_defaults,
            bound: None,
            owner: None,
            annotations,
        }))
    })
}

fn b_build_class(vm: &mut VM, _: u8) -> Value {
    let kwargs_val = vm.pop();
    let body_func = vm.pop();
    let name = sval(&vm.pop());
    let bases_val = vm.pop();
    let metaclass = vm.pop();
    // A foreign (CPython) base — `class C(enum.Enum)`, `class T(NamedTuple)` —
    // means the class must be built by that base's metaclass; route creation
    // through CPython so members/fields are produced the real way.
    #[cfg(feature = "stdlib-ffi")]
    if with_host(|h| match h.get(&bases_val) {
        Some(PyObj::List(l)) => l.iter().any(|b| h.foreign_id(b).is_some()),
        _ => false,
    }) {
        let bases: Vec<Value> = with_host(|h| match h.get(&bases_val) {
            Some(PyObj::List(l)) => l.clone(),
            _ => Vec::new(),
        });
        let r = host::build_class_foreign(&name, bases, &body_func);
        return finish(vm, r);
    }
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
    // A class whose metaclass defines `__iter__` (Enum subclasses) iterates via
    // `type(cls).__iter__(cls)`; drive whatever iterable that returns.
    if let Some(m) = with_host(|h| h.metaclass_method(&v, "__iter__")) {
        let r = host::invoke(&m, vec![v.clone()], vec![]).and_then(|it| iter_drive(&it));
        return finish(vm, r);
    }
    let r = with_host(|h| h.make_iter(&v));
    finish(vm, r)
}

/// Turn an already-obtained iterable (e.g. the result of a metaclass `__iter__`)
/// into an iterator: a user instance goes through its own iteration protocol; a
/// native iterator/generator/sequence is driven by `make_iter`.
fn iter_drive(it: &Value) -> Result<Value, String> {
    if with_host(|h| matches!(h.get(it), Some(PyObj::Instance(_)))) {
        return iter_instance(it);
    }
    with_host(|h| h.make_iter(it))
}

/// Drive a user iterable into a concrete seq iterator. A `__iter__` that returns
/// a native iterator is used directly (stays lazy); everything else materializes
/// through the shared `__iter__`/`__next__`/`__getitem__` protocol.
fn iter_instance(v: &Value) -> Result<Value, String> {
    // A builtin-type subclass without an `__iter__` override iterates its native
    // payload (`for x in Stack([...])`).
    if let Some(payload) = host::subclass_payload(v, "__iter__") {
        return with_host(|h| h.make_iter(&payload));
    }
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
        // A builtin-type subclass without `__contains__` tests membership on its
        // native payload (`x in Stack([...])`).
        if let Some(payload) = host::subclass_payload(&container, "__contains__") {
            return match with_host(|h| h.contains(&item, &payload)) {
                Ok(b) => Value::Bool(b),
                Err(e) => abort(vm, e),
            };
        }
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
    // A list/tuple whose membership may hit a user `__eq__` (the searched item or
    // any element is an instance) compares element-by-element via the rich `==`
    // dunder outside the host borrow. Scalar-only sequences use the fast path.
    if with_host(|h| {
        matches!(
            h.get(&container),
            Some(PyObj::List(_)) | Some(PyObj::Tuple(_))
        )
    }) {
        let elems = with_host(|h| match h.get(&container) {
            Some(PyObj::List(l)) | Some(PyObj::Tuple(l)) => l.clone(),
            _ => Vec::new(),
        });
        let any_instance = with_host(|h| {
            matches!(h.get(&item), Some(PyObj::Instance(_)))
                || elems
                    .iter()
                    .any(|e| matches!(h.get(e), Some(PyObj::Instance(_))))
        });
        if any_instance {
            for e in &elems {
                match elem_equal(e, &item) {
                    Ok(true) => return Value::Bool(true),
                    Ok(false) => {}
                    Err(err) => return abort(vm, err),
                }
            }
            return Value::Bool(false);
        }
    }
    // A CPython `Foreign` container (stdlib-ffi): run `in` (its `__contains__`)
    // OUTSIDE the borrow so a `@dataclass` with a user `__contains__` re-enters.
    #[cfg(feature = "stdlib-ffi")]
    if let Some(id) = with_host(|h| h.foreign_id(&container)) {
        return match crate::ffi::contains_cb(id, &item) {
            Ok(b) => Value::Bool(b),
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
                    // `Ellipsis`/`NotImplemented` are singletons: any two
                    // allocations of `...` satisfy `... is ...`.
                    (Some(PyObj::Ellipsis), Some(PyObj::Ellipsis))
                    | (Some(PyObj::NotImplemented), Some(PyObj::NotImplemented)) => true,
                    // Two `Foreign` handles are the same object when they point at
                    // the same CPython object — so `Color.RED is Color.RED`
                    // (a singleton enum member fetched twice) holds.
                    #[cfg(feature = "stdlib-ffi")]
                    (Some(PyObj::Foreign(m)), Some(PyObj::Foreign(n))) => {
                        crate::ffi::same_object(*m, *n)
                    }
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

/// `break` that must cross a `try`/`with` chunk boundary: set the Break signal
/// and stop the current chunk (so a `finally` on the way out still runs, driven
/// by `b_try`), letting the signal bubble to the enclosing `LOOP_BODY`.
fn b_sig_break(vm: &mut VM, _: u8) -> Value {
    with_host(|h| h.signal = Some(host::Signal::Break));
    vm.ip = vm.chunk.ops.len();
    Value::Undef
}

/// `continue` counterpart of `b_sig_break`.
fn b_sig_continue(vm: &mut VM, _: u8) -> Value {
    with_host(|h| h.signal = Some(host::Signal::Continue));
    vm.ip = vm.chunk.ops.len();
    Value::Undef
}

/// Run one iteration's body chunk for a loop whose `break`/`continue` cross a
/// `try`/`with` boundary (so they can't be plain jumps). Returns `Int(0)` to
/// continue to the next iteration (normal fall-through OR `continue`), `Int(1)`
/// to break. A `Return` signal is left intact and the enclosing loop chunk is
/// halted so the return propagates to the function frame.
fn b_loop_body(vm: &mut VM, _: u8) -> Value {
    let id = match vm.pop() {
        Value::Int(n) => n as usize,
        _ => return abort(vm, "internal: LOOP_BODY id".into()),
    };
    let body = match with_host(|h| h.try_def(id)) {
        Some(t) => t.body,
        None => return abort(vm, "internal: unknown loop-body id".into()),
    };
    if let Err(e) = host::run_chunk_on(body) {
        return abort(vm, e);
    }
    let code = with_host(|h| match &h.signal {
        Some(host::Signal::Break) => {
            h.signal = None;
            1
        }
        Some(host::Signal::Continue) => {
            h.signal = None;
            0
        }
        // Return: keep the signal for the function frame to consume.
        Some(host::Signal::Return(_)) => 2,
        None => 0,
    });
    if code == 2 {
        // Propagate the pending return: stop the loop chunk immediately.
        vm.ip = vm.chunk.ops.len();
    }
    Value::Int(code)
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
                    // Any explicit `from` clause (including `from None`) sets
                    // `__suppress_context__`, hiding the implicit context.
                    if argc >= 2 {
                        if let Value::Obj(id) = new_exc {
                            h.suppress_context.insert(id);
                        }
                    }
                }
            });
            abort(vm, msg)
        }
        Err(e) => abort(vm, e),
    }
}

fn b_reraise(vm: &mut VM, _: u8) -> Value {
    // Re-raise the active exception preserving its *class* (not just its
    // message): use the "Class: msg" / "Class" form so a driver that recovers
    // the exception from the abort string alone (e.g. an asyncio Task settling a
    // re-raised `CancelledError`) still sees the right type. `exc_error_string`
    // itself borrows the host, so pull `h.exc` out before calling it.
    let active = with_host(|h| h.exc.clone());
    let msg = match active {
        Some(ref v) => Some(exc_error_string(v)),
        None => with_host(|h| h.error.clone()),
    };
    match msg {
        Some(m) => abort(vm, m),
        None => abort(vm, "RuntimeError: No active exception to re-raise".into()),
    }
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
/// Element equality for `in` / `list.index` / `list.count` / `list.remove` on a
/// list or tuple — CPython's `PyObject_RichCompareBool`: identity first (so
/// `nan in [nan]` with the same object is `True`), then the `==` dunder when a
/// user instance is involved, else native value equality. `elem` is the sequence
/// element (forward operand), `target` the searched value, matching CPython's
/// `RichCompareBool(item, value, Py_EQ)`.
fn elem_equal(elem: &Value, target: &Value) -> Result<bool, String> {
    if identity_eq(elem, target) {
        return Ok(true);
    }
    let needs_dunder = with_host(|h| {
        matches!(h.get(elem), Some(PyObj::Instance(_)))
            || matches!(h.get(target), Some(PyObj::Instance(_)))
    });
    if needs_dunder {
        return match dispatch_binop(elem, target, "__eq__", "__eq__") {
            Dunder::Value(v) => Ok(with_host(|h| h.truthy(&v))),
            Dunder::Err(e) => Err(e),
            Dunder::NotImplemented => Ok(with_host(|h| h.equal(elem, target))),
        };
    }
    Ok(with_host(|h| h.equal(elem, target)))
}

fn try_binop_dunder(
    a: &Value,
    b: &Value,
    lname: &str,
    rname: &str,
) -> Option<Result<Value, String>> {
    // `str % obj` / `bytes % obj` / `bytearray % obj` are native formatting
    // (`__mod__`), which is authoritative — CPython never returns
    // `NotImplemented` from it, so the right operand's `__rmod__` is never
    // consulted (even when the RHS is an instance with `__bytes__`). Route
    // straight to native.
    if lname == "__mod__"
        && with_host(|h| {
            matches!(a, Value::Str(_))
                || matches!(
                    h.get(a),
                    Some(PyObj::Str(_)) | Some(PyObj::Bytes(_)) | Some(PyObj::Bytearray(_))
                )
        })
    {
        return None;
    }
    // A builtin-type subclass operand with no override delegates to its native
    // payload (`S1 | S2` of set subclasses runs native set union, etc.).
    let a_owned = host::subclass_operand(a, lname);
    let b_owned = host::subclass_operand(b, rname);
    let a = &a_owned;
    let b = &b_owned;
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
    // A builtin-type subclass operand with no override delegates to its native
    // payload, so the native op runs on the base value (`S1 | S2`, `C(5) // 2`).
    let (a, b) = match binop_tag_dunders(tag) {
        Some((l, r)) => (host::subclass_operand(&a, l), host::subclass_operand(&b, r)),
        None => (a, b),
    };
    // Counter `&` (intersection) / `|` (union) — multiset ops keeping positive
    // counts; only when both operands are Counters.
    if tag == host::binop::BITAND || tag == host::binop::BITOR {
        let op = if tag == host::binop::BITAND { '&' } else { '|' };
        if let Some(res) = counter_binop(&a, &b, op) {
            return finish(vm, res);
        }
    }
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
    // `bytes % args` / `bytearray % args` (PEP 461). The result keeps the
    // receiver's type.
    if tag == host::binop::MOD {
        let kind = with_host(|h| match h.get(&a) {
            Some(PyObj::Bytes(_)) => Some(false),
            Some(PyObj::Bytearray(_)) => Some(true),
            _ => None,
        });
        if let Some(is_ba) = kind {
            let r = bytes_percent_format(&a, &b, is_ba);
            return finish(vm, r);
        }
    }
    let r = with_host(|h| h.binop(tag, &a, &b));
    finish(vm, r)
}

/// The in-place dunder name for an `ops::iop` tag (`+=` → `__iadd__`, …).
fn iop_dunder(tag: i64) -> Option<&'static str> {
    use host::iop::*;
    Some(match tag {
        ADD => "__iadd__",
        SUB => "__isub__",
        MUL => "__imul__",
        DIV => "__itruediv__",
        FLOORDIV => "__ifloordiv__",
        MOD => "__imod__",
        POW => "__ipow__",
        MATMUL => "__imatmul__",
        BITAND => "__iand__",
        BITOR => "__ior__",
        BITXOR => "__ixor__",
        SHL => "__ilshift__",
        SHR => "__irshift__",
        _ => return None,
    })
}

/// The augmented-assignment operator glyph for an in-place `TypeError` message.
fn iop_symbol(tag: i64) -> &'static str {
    use host::iop::*;
    match tag {
        ADD => "+=",
        SUB => "-=",
        MUL => "*=",
        DIV => "/=",
        FLOORDIV => "//=",
        MOD => "%=",
        POW => "**=",
        MATMUL => "@=",
        BITAND => "&=",
        BITOR => "|=",
        BITXOR => "^=",
        SHL => "<<=",
        SHR => ">>=",
        _ => "?",
    }
}

/// The `x op= y` binary fallback: `x = x op y`. `+`/`-`/`*` route through the
/// numeric hook (so a user `__add__`/`__radd__` still fires); the rest mirror
/// `b_binop`'s non-native dispatch (instance dunder, `str %`, then the host op).
fn inplace_binary_fallback(tag: i64, a: &Value, b: &Value) -> Result<Value, String> {
    use host::iop;
    let btag = match tag {
        iop::ADD => return numeric_hook(NumOp::Add, a, b),
        iop::SUB => return numeric_hook(NumOp::Sub, a, b),
        iop::MUL => return numeric_hook(NumOp::Mul, a, b),
        iop::DIV => host::binop::DIV,
        iop::FLOORDIV => host::binop::FLOORDIV,
        iop::MOD => host::binop::MOD,
        iop::POW => host::binop::POW,
        iop::MATMUL => host::binop::MATMUL,
        iop::BITAND => host::binop::BITAND,
        iop::BITOR => host::binop::BITOR,
        iop::BITXOR => host::binop::BITXOR,
        iop::SHL => host::binop::SHL,
        iop::SHR => host::binop::SHR,
        _ => return Err(host::type_error("internal: INPLACE tag")),
    };
    if let Some((l, r)) = binop_tag_dunders(btag) {
        if let Some(res) = try_binop_dunder(a, b, l, r) {
            return res;
        }
    }
    if btag == host::binop::MOD && with_host(|h| matches!(h.get(a), Some(PyObj::Str(_)))) {
        return str_percent_format(a, b);
    }
    with_host(|h| h.binop(btag, a, b))
}

/// The identity-preserving in-place fast paths for the mutable built-ins
/// (`list`/`set`/`dict`/`bytearray`). `Some(Ok(a))` after mutating `a` in place;
/// `Some(Err(..))` for an in-place-specific `TypeError`; `None` when `a` has no
/// in-place fast path for this op and the caller should use the binary fallback.
fn inplace_builtin(tag: i64, a: &Value, b: &Value) -> Option<Result<Value, String>> {
    use host::iop;
    // A mutable builtin-subclass instance (`class L(list)`, `class S(set)`) keeps
    // its native storage in its payload. Mutate the payload in place, then return
    // the *instance* so `x += ...` preserves x's subclass type (CPython: inherited
    // `__iadd__` mutates and returns self). An immutable payload (int/str/tuple
    // subclass) yields `None` here, so the binary fallback rebinds to the base
    // type — which is what CPython does for immutables too.
    let payload = with_host(|h| match h.get(a) {
        Some(PyObj::Instance(inst)) if !matches!(inst.payload, Value::Undef) => {
            Some(inst.payload.clone())
        }
        _ => None,
    });
    if let Some(p) = payload {
        return match inplace_builtin(tag, &p, b) {
            Some(Ok(_)) => Some(Ok(a.clone())),
            other => other,
        };
    }
    // list: `+=` extends with any iterable; `*=` repeats in place.
    if with_host(|h| matches!(h.get(a), Some(PyObj::List(_)))) {
        match tag {
            iop::ADD => {
                let items = match host::iter_vec(b) {
                    Ok(v) => v,
                    Err(_) => {
                        return Some(Err(with_host(|h| {
                            host::type_error(&format!(
                                "'{}' object is not iterable",
                                h.type_name(b)
                            ))
                        })));
                    }
                };
                with_host(|h| {
                    if let Some(PyObj::List(l)) = h.get_mut(a) {
                        l.extend(items);
                    }
                });
                return Some(Ok(a.clone()));
            }
            iop::MUL => {
                let n = with_host(|h| h.as_int(b))?.max(0) as usize; // non-int → binary fallback
                with_host(|h| {
                    if let Some(PyObj::List(l)) = h.get_mut(a) {
                        let base = l.clone();
                        l.clear();
                        for _ in 0..n {
                            l.extend(base.clone());
                        }
                    }
                });
                return Some(Ok(a.clone()));
            }
            _ => return None,
        }
    }
    // bytearray: `+=` extends with a bytes-like; `*=` repeats in place.
    if with_host(|h| matches!(h.get(a), Some(PyObj::Bytearray(_)))) {
        match tag {
            iop::ADD => match as_bytes_object(b) {
                Some(bytes) => {
                    with_host(|h| {
                        if let Some(PyObj::Bytearray(v)) = h.get_mut(a) {
                            v.extend_from_slice(&bytes);
                        }
                    });
                    return Some(Ok(a.clone()));
                }
                None => {
                    return Some(Err(with_host(|h| {
                        host::type_error(&format!("can't concat {} to bytearray", h.type_name(b)))
                    })));
                }
            },
            iop::MUL => {
                let n = with_host(|h| h.as_int(b))?.max(0) as usize;
                with_host(|h| {
                    if let Some(PyObj::Bytearray(v)) = h.get_mut(a) {
                        let base = v.clone();
                        v.clear();
                        for _ in 0..n {
                            v.extend_from_slice(&base);
                        }
                    }
                });
                return Some(Ok(a.clone()));
            }
            _ => return None,
        }
    }
    // dict: `|=` updates in place with a mapping or an iterable of pairs.
    if with_host(|h| matches!(h.get(a), Some(PyObj::Dict(_)))) && tag == iop::BITOR {
        return Some(dict_update(a, Some(b), &[]).map(|()| a.clone()));
    }
    // set (mutable only): `|= &= -= ^=` mutate in place; the operand must be a
    // set-like (mirrors the binary set operators). A frozenset has no in-place
    // form and falls through to the binary op (rebinding a new frozenset).
    let is_set = with_host(|h| matches!(h.get(a), Some(PyObj::Set(_))));
    if is_set && matches!(tag, iop::BITOR | iop::BITAND | iop::SUB | iop::BITXOR) {
        let y = match with_host(|h| h.setmap_of(b)) {
            Some(y) => y,
            None => return Some(Err(unsupported_operand(iop_symbol(tag), a, b))),
        };
        with_host(|h| {
            if let Some(PyObj::Set(x)) = h.get_mut(a) {
                match tag {
                    iop::BITOR => {
                        for (k, v) in y {
                            x.entry(k).or_insert(v);
                        }
                    }
                    iop::BITAND => {
                        x.retain(|k, _| y.contains_key(k));
                    }
                    iop::SUB => {
                        x.retain(|k, _| !y.contains_key(k));
                    }
                    _ => {
                        // symmetric difference: drop shared keys, add b-only keys
                        // (x-only keys stay untouched).
                        for (k, v) in y {
                            if x.contains_key(&k) {
                                x.shift_remove(&k);
                            } else {
                                x.insert(k, v);
                            }
                        }
                    }
                }
            }
        });
        return Some(Ok(a.clone()));
    }
    None
}

/// `x op= y` (augmented assignment): try `type(x).__i<op>__(x, y)` — mutating `x`
/// in place and preserving identity for the mutable built-ins and any user
/// in-place dunder — then fall back to `x = x op y`.
fn b_inplace(vm: &mut VM, _: u8) -> Value {
    let b = vm.pop();
    let a = vm.pop();
    let tag = match vm.pop() {
        Value::Int(n) => n,
        _ => return abort(vm, "internal: INPLACE tag".into()),
    };
    // 1. A user instance's in-place dunder wins; `NotImplemented` falls back.
    if let Some(iname) = iop_dunder(tag) {
        if with_host(|h| is_instance_with(h, &a, iname)) {
            match host::call_method(&a, iname, vec![b.clone()], vec![]) {
                Ok(v) if is_not_implemented(&v) => {
                    return finish(vm, inplace_binary_fallback(tag, &a, &b));
                }
                r => return finish(vm, r),
            }
        }
    }
    // 2. Mutable built-in in-place fast path (identity preserved).
    if let Some(r) = inplace_builtin(tag, &a, &b) {
        return finish(vm, r);
    }
    // 3. Binary fallback (immutables rebind a new value; instance `__add__`/… fire).
    finish(vm, inplace_binary_fallback(tag, &a, &b))
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

/// `bytes % args` / `bytearray % args` (PEP 461): pre-resolve any user
/// instance's `__bytes__` OUTSIDE the host borrow (the host `%` formatter runs
/// inside the borrow and cannot call back into `__bytes__`), keyed by heap id,
/// then hand the pre-resolved table to the host formatter.
fn bytes_percent_format(fmt_val: &Value, args: &Value, is_ba: bool) -> Result<Value, String> {
    let items: Vec<Value> = with_host(|h| match h.get(args) {
        Some(PyObj::Tuple(t)) => t.clone(),
        Some(PyObj::Dict(d)) => d.values().map(|(_, v)| v.clone()).collect(),
        _ => vec![args.clone()],
    });
    let mut premap: std::collections::HashMap<u32, Vec<u8>> = std::collections::HashMap::new();
    for it in &items {
        let Value::Obj(id) = it else { continue };
        if premap.contains_key(id) {
            continue;
        }
        // Only instances that define `__bytes__` need the dispatching path.
        let has = with_host(
            |h| matches!(h.get(it), Some(PyObj::Instance(i)) if instance_has(h, i, "__bytes__")),
        );
        if !has {
            continue;
        }
        let res = host::call_method(it, "__bytes__", vec![], vec![])?;
        let bytes = with_host(|h| match h.get(&res) {
            Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Some(b.clone()),
            _ => None,
        });
        match bytes {
            Some(b) => {
                premap.insert(*id, b);
            }
            None => {
                return Err(host::type_error(&format!(
                    "__bytes__ returned non-bytes (type {})",
                    with_host(|h| h.type_name(&res))
                )))
            }
        }
    }
    let fmt = with_host(|h| match h.get(fmt_val) {
        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => b.clone(),
        _ => vec![],
    });
    with_host(|h| h.bytes_format_percent(&fmt, args, is_ba, &premap))
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
    // A CPython `Foreign` operand (stdlib-ffi): dispatch `~`/unary `+` OUTSIDE the
    // borrow so a `@dataclass` with a user `__invert__`/`__pos__` can re-enter.
    #[cfg(feature = "stdlib-ffi")]
    {
        let func = match tag {
            host::unop::INVERT => "invert",
            host::unop::POS => "pos",
            _ => "",
        };
        if !func.is_empty() && with_host(|h| h.foreign_id(&v).is_some()) {
            return finish(vm, crate::ffi::unary_op_cb(func, &v));
        }
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
    let is_module = with_host(|h| matches!(h.get(&module), Some(PyObj::Module { .. })));
    let r = with_host(|h| h.get_attr(&module, &name)).map_err(|e| {
        // `from mod import missing` raises ImportError (not AttributeError) so the
        // stdlib's `try: from posix import X / except ImportError:` fallbacks fire.
        if is_module && e.starts_with("AttributeError") {
            let mname = with_host(|h| match h.get(&module) {
                Some(PyObj::Module { name, .. }) => name.clone(),
                _ => String::new(),
            });
            format!("ImportError: cannot import name '{name}' from '{mname}'")
        } else {
            e
        }
    });
    finish(vm, r)
}

/// `from <module> import *` — bind the module's public names (its `__all__`, or
/// every non-underscore name) into the current namespace. Leaves an `Undef`
/// sentinel the compiler pops.
fn b_import_star(vm: &mut VM, _: u8) -> Value {
    let module = vm.pop();
    let bindings = match with_host(|h| h.import_star_bindings(&module)) {
        Ok(b) => b,
        Err(e) => return abort(vm, e),
    };
    with_host(|h| {
        for (name, val) in bindings {
            h.set_name(&name, val);
        }
    });
    Value::Undef
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
            // Resolve the object for the raised error. `h.exc` is authoritative
            // only when it matches the error string just produced (an explicit
            // `raise` or a builtin that installed its own exception, e.g.
            // `KeyError`). Otherwise `h.exc` is a stale still-being-handled
            // exception (set when this handler's enclosing `except` fired): a
            // native error like `[..][5]` never updates it, so matching against
            // that stale class would pick the wrong handler. Synthesize a fresh
            // exception from the string and wire the stale one as `__context__`.
            let exc = with_host(|h| {
                let consistent = h
                    .exc
                    .clone()
                    .and_then(|x| h.exc_line_of(&x))
                    .map(|line| line == e)
                    .unwrap_or(false);
                if consistent {
                    return h.exc.clone().unwrap();
                }
                let context = h.exc.clone().unwrap_or(Value::Undef);
                let new = synth_exc(h, &e);
                let ctx = match &context {
                    Value::Obj(_) if new != context => context,
                    _ => Value::Undef,
                };
                h.set_exc_link(&new, Value::Undef, ctx);
                h.exc = Some(new.clone());
                new
            });
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
                    // The exception is caught: snapshot its frames as `__traceback__`
                    // (for a later chained render), then discard the live trace.
                    with_host(|h| {
                        h.error = None;
                        h.exc = Some(exc.clone());
                        h.capture_exc_tb(&exc);
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
/// CPython's message for a class pattern with too many positional sub-patterns:
/// "NAME() accepts N positional sub-pattern(s) (M given)".
fn match_pos_msg(cname: &str, accepts: usize, given: usize) -> String {
    let plural = if accepts == 1 { "" } else { "s" };
    format!("{cname}() accepts {accepts} positional sub-pattern{plural} ({given} given)")
}

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
    // `isinstance_dispatch` (not the raw helper) so a foreign class — a
    // `@dataclass`/`enum` mirror — matches via CPython's `isinstance`.
    match isinstance_dispatch(&subject, &class) {
        Ok(true) => {}
        Ok(false) => return Value::Bool(false),
        Err(e) => return abort(vm, e),
    }
    let cname = with_host(|h| callable_name(h, &class)).unwrap_or_default();
    let mut vals: Vec<Value> = Vec::new();
    if npos > 0 {
        if is_builtin_type(&cname) {
            // Builtin types (int, str, …) allow exactly one positional self-match.
            if npos != 1 {
                return abort(vm, host::type_error(&match_pos_msg(&cname, 1, npos)));
            }
            vals.push(subject.clone());
        } else {
            // `__match_args__` via `get_attr` so a foreign class (dataclass mirror,
            // whose name isn't in `h.classes`) is read over the bridge.
            let margs = with_host(|h| h.get_attr(&class, "__match_args__")).ok();
            let names: Vec<String> = match margs {
                Some(v) => match host::iter_vec(&v) {
                    Ok(items) => items.iter().map(sval).collect(),
                    Err(e) => return abort(vm, e),
                },
                None => Vec::new(),
            };
            if npos > names.len() {
                return abort(
                    vm,
                    host::type_error(&match_pos_msg(&cname, names.len(), npos)),
                );
            }
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
        // A foreign exception class (`except json.JSONDecodeError`) resolves by
        // its CPython `__name__`, matched against the raised exception's recorded
        // base chain by `exception_isa`.
        #[cfg(feature = "stdlib-ffi")]
        None => match h.foreign_id(typ).and_then(crate::ffi::class_name) {
            Some(n) => n,
            None => return false,
        },
        #[cfg(not(feature = "stdlib-ffi"))]
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
/// The `operator`-module attribute name for a `NumOp` binary op (`Add` → `add`,
/// `Lt` → `lt`, …), or `None` for the unary `Neg`. Used to run an op on a
/// `Foreign` operand in CPython over the FFI bridge.
#[cfg(feature = "stdlib-ffi")]
fn foreign_binop_func(op: NumOp) -> Option<&'static str> {
    use NumOp::*;
    Some(match op {
        Add => "add",
        Sub => "sub",
        Mul => "mul",
        Div => "truediv",
        Mod => "mod",
        Pow => "pow",
        Eq => "eq",
        Ne => "ne",
        Lt => "lt",
        Le => "le",
        Gt => "gt",
        Ge => "ge",
        Neg => return None,
    })
}

pub fn numeric_hook(op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
    use NumOp::*;
    // A builtin-type subclass operand with no operator override delegates to its
    // native payload, so `C(5) + 3` runs int arithmetic (yielding a plain `int`)
    // and `Stack([1]) + [2]` runs list concatenation (a plain `list`).
    let (fwd, refl) = match op {
        Neg => ("__neg__", ""),
        _ => numop_dunders(op).unwrap_or(("", "")),
    };
    let a_owned = if fwd.is_empty() {
        a.clone()
    } else {
        host::subclass_operand(a, fwd)
    };
    let b_owned = if refl.is_empty() {
        b.clone()
    } else {
        host::subclass_operand(b, refl)
    };
    let a = &a_owned;
    let b = &b_owned;
    // Counter `+` / `-` — multiset ops keeping positive counts, only when both
    // operands are Counters (else int/float arithmetic falls through).
    match op {
        Add | Sub => {
            let sym = if matches!(op, Add) { '+' } else { '-' };
            if let Some(res) = counter_binop(a, b, sym) {
                return res;
            }
        }
        _ => {}
    }
    // A CPython (Foreign) operand runs the op in CPython with the host borrow
    // released, so an operator that re-enters pythonrs (a `cmp_to_key` wrapper's
    // `__lt__` calling the user cmp function during `sorted`) doesn't double-
    // borrow the host. `h.arith`'s foreign branch would hold the borrow.
    #[cfg(feature = "stdlib-ffi")]
    if let Some(func) = foreign_binop_func(op) {
        if with_host(|h| h.foreign_id(a).is_some() || h.foreign_id(b).is_some()) {
            return crate::ffi::binary_op_cb(func, a, b);
        }
    }
    // Unary negation on a `Foreign` operand: `foreign_binop_func(Neg)` is `None`
    // (it is not a binary op), so dispatch it borrow-free here before the native
    // fallthrough would hand it to `h.arith` (which holds the borrow across ffi).
    #[cfg(feature = "stdlib-ffi")]
    if matches!(op, Neg) && with_host(|h| h.foreign_id(a).is_some()) {
        return crate::ffi::unary_op_cb("neg", a);
    }
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
                    // `@functools.total_ordering`: derive the missing op from the
                    // class's defined ordering dunder — forward (`a op b`) first,
                    // then reflected (`b <reflected-op> a`).
                    if let Some(res) = total_ordering_derive(op, a, b) {
                        if !matches!(&res, Ok(v) if is_not_implemented(v)) {
                            return res;
                        }
                    }
                    let refl = match op {
                        Lt => Gt,
                        Le => Ge,
                        Gt => Lt,
                        _ => Le,
                    };
                    if let Some(res) = total_ordering_derive(refl, b, a) {
                        if !matches!(&res, Ok(v) if is_not_implemented(v)) {
                            return res;
                        }
                    }
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
                // A CPython `Foreign` operand: dispatch `-x` OUTSIDE the borrow so
                // a `@dataclass` with a user `__neg__` can re-enter the host.
                #[cfg(feature = "stdlib-ffi")]
                if with_host(|h| h.foreign_id(a).is_some()) {
                    return crate::ffi::unary_op_cb("neg", a);
                }
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
/// `True` if `v` is an `int` (or `bool`/bigint) — CPython `PyLong_Check` — used
/// by `sum`'s exact integer prefix and its float-loop int handling.
fn sum_is_long(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::Bool(_))
        || with_host(|h| matches!(h.get(v), Some(PyObj::BigInt(_))))
}

/// The trailing half of CPython's "sum() can't sum X" message if `start` is a
/// forbidden start value (str/bytes/bytearray), else `None`.
fn sum_bad_start(start: &Value) -> Option<&'static str> {
    if matches!(start, Value::Str(_)) {
        return Some("strings [use ''.join(seq) instead]");
    }
    with_host(|h| match h.get(start) {
        Some(PyObj::Str(_)) => Some("strings [use ''.join(seq) instead]"),
        Some(PyObj::Bytes(_)) => Some("bytes [use b''.join(seq) instead]"),
        Some(PyObj::Bytearray(_)) => Some("bytearray [use b''.join(seq) instead]"),
        _ => None,
    })
}

/// Neumaier "improved Kahan–Babuška" step: fold `x` into the compensated sum
/// `(hi, lo)`. Verbatim port of CPython `cs_add` (`Python/bltinmodule.c`).
#[inline]
fn cs_add(hi: f64, lo: f64, x: f64) -> (f64, f64) {
    let t = hi + x;
    let lo = if hi.abs() >= x.abs() {
        lo + ((hi - t) + x)
    } else {
        lo + ((x - t) + hi)
    };
    (t, lo)
}

/// Collapse a compensated sum to a `double` (CPython `cs_to_double`): add the
/// low-order compensation back unless it is non-finite.
#[inline]
fn cs_to_double(hi: f64, lo: f64) -> f64 {
    if lo != 0.0 && lo.is_finite() {
        hi + lo
    } else {
        hi
    }
}

/// `True` if `name` names a builtin *type* (constructor) or exception class —
/// i.e. `type(<that>)` is `type`. Used by `PyHost::type_name` to distinguish a
/// builtin type object (`int`, `ValueError`) from a builtin function (`len`).
pub fn is_type_like_builtin(name: &str) -> bool {
    is_builtin_type(name) || is_exception_class(name)
}

/// The universal object dunders that any builtin value exposes as a bound method
/// (`d.__len__`, `d.__getitem__`, `x.__eq__`), dispatched by `call_type_method`.
/// The stdlib reaches these directly (e.g. functools.lru_cache uses
/// `cache.__len__`).
pub fn is_object_dunder_method(name: &str) -> bool {
    // Note: __eq__/__ne__/__hash__ are NOT here — those have type-specific
    // NotImplemented semantics (e.g. int.__eq__(str) is NotImplemented) handled
    // elsewhere; a generic override would break them.
    matches!(
        name,
        "__len__"
            | "__getitem__"
            | "__setitem__"
            | "__delitem__"
            | "__iter__"
            | "__contains__"
            | "__str__"
            | "__repr__"
            | "__bool__"
    )
}

/// True if `n` names a builtin TYPE object (for `isinstance(x, type)`, `__mro__`,
/// `__dict__`). Covers the builtin types/exceptions, the type-constructor
/// builtins that are also callable (`zip`/`map`/`filter`/`enumerate`/…), and any
/// dotless type-name builtin produced by `type(x)` (coroutine/generator/…). A
/// dotted name is an unbound method; a plain BUILTIN_FUNCS name is a function.
pub fn is_type_object_name(n: &str) -> bool {
    is_type_like_builtin(n)
        || matches!(
            n,
            "zip" | "map"
                | "filter"
                | "enumerate"
                | "reversed"
                | "slice"
                | "super"
                | "property"
                | "classmethod"
                | "staticmethod"
        )
        || (!n.contains('.') && !is_builtin_function(n))
}

/// The method-resolution order of a builtin type object, as type names from the
/// type up to `object`. Exceptions follow their class chain; `bool` subclasses
/// `int`; everything else is `[name, object]`.
pub fn builtin_mro(name: &str) -> Vec<String> {
    let mut chain = vec![name.to_string()];
    if name == "bool" {
        chain.push("int".to_string());
    } else if is_exception_class(name) {
        let mut cur = name;
        while let Some((_, parent)) = EXC_PARENTS.iter().find(|(c, _)| *c == cur) {
            chain.push((*parent).to_string());
            cur = parent;
        }
    }
    if name != "object" {
        chain.push("object".to_string());
    }
    chain
}

/// The classmethod names of a builtin type — the members reached as
/// `<type>.__dict__[name]` that are C classmethod descriptors (`dict.fromkeys`,
/// `int.from_bytes`, …). Used to populate a type object's `__dict__` proxy.
pub fn type_classmethods(name: &str) -> &'static [&'static str] {
    match name {
        "dict" => &["fromkeys"],
        "int" => &["from_bytes"],
        "bytes" | "bytearray" => &["fromhex"],
        "float" => &["fromhex"],
        _ => &[],
    }
}

/// The builtin *type* names (constructors), as a slice so the `builtins` module
/// namespace can enumerate them without duplicating the list.
pub const BUILTIN_TYPES: &[&str] = &[
    "int",
    "float",
    "str",
    "bool",
    "list",
    "tuple",
    "dict",
    "set",
    "frozenset",
    "bytes",
    "bytearray",
    "memoryview",
    "complex",
    "object",
    "type",
    "range",
];

fn is_builtin_type(name: &str) -> bool {
    BUILTIN_TYPES.contains(&name)
}

/// Every builtin name exposed by the `builtins` module: functions, type
/// constructors, and exception classes (each resolves to a `PyObj::Builtin`).
/// Singletons (`None`/`True`/`False`/`NotImplemented`/`Ellipsis`) are added by
/// the module builder, not here.
pub fn builtin_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = BUILTIN_FUNCS.to_vec();
    names.extend_from_slice(BUILTIN_TYPES);
    names.push("BaseException");
    names.extend(EXC_PARENTS.iter().map(|(c, _)| *c));
    names
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
    "__import__",
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
    "slice",
    "eval",
    "exec",
    "globals",
    "locals",
];

// ── builtin functions ────────────────────────────────────────────────────────

/// str()/repr() with instance dunder dispatch (free-function form).
pub fn py_str(v: &Value) -> Result<String, String> {
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
    // `host::str_of` prefetches a `Foreign` object's CPython `str` outside the
    // borrow, so a foreign value with a user `__str__`/`__repr__` cannot re-enter.
    Ok(host::str_of(v))
}

pub fn py_repr(v: &Value) -> Result<String, String> {
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
        // A dict, plus its subtype tag (Counter / defaultdict / OrderedDict) and
        // the defaultdict factory, so the repr keeps the CPython wrapper.
        Dict(Vec<(Value, Value)>, Option<(host::DictKind, Option<Value>)>),
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
        Some(PyObj::Dict(d)) => {
            let meta = match v {
                Value::Obj(i) => h.dict_meta.get(i).map(|m| (m.kind, m.factory.clone())),
                _ => None,
            };
            Some(Cont::Dict(d.values().cloned().collect(), meta))
        }
        _ => None,
    });
    let reprs =
        |elems: &[Value]| -> Result<Vec<String>, String> { elems.iter().map(py_repr).collect() };
    if let Some(cont) = cont {
        // Reference-cycle guard: a container that (transitively) contains itself
        // must emit CPython's recursion marker instead of recursing forever.
        let id = if let Value::Obj(i) = v { *i } else { 0 };
        let marker = match &cont {
            Cont::List(_) => "[...]",
            Cont::Tuple(..) => "(...)",
            Cont::Set(_) | Cont::Dict(..) => "{...}",
            Cont::Frozenset(_) => "frozenset(...)",
        };
        if host::repr_guard_enter(id) {
            return Ok(marker.to_string());
        }
        let build = || -> Result<String, String> {
            Ok(match cont {
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
                Cont::Dict(pairs, meta) => {
                    let mut p = Vec::with_capacity(pairs.len());
                    for (k, val) in &pairs {
                        p.push(format!("{}: {}", py_repr(k)?, py_repr(val)?));
                    }
                    let body = format!("{{{}}}", p.join(", "));
                    // CPython 3.12+ formats: Counter({…}), OrderedDict({…}),
                    // defaultdict(<factory>, {…}); a plain dict is just {…}.
                    // Empty Counter/OrderedDict use the bare `Counter()` form.
                    let empty = pairs.is_empty();
                    match meta.as_ref().map(|(k, _)| *k) {
                        Some(host::DictKind::Counter) if empty => "Counter()".into(),
                        Some(host::DictKind::Counter) => format!("Counter({body})"),
                        Some(host::DictKind::OrderedDict) if empty => "OrderedDict()".into(),
                        Some(host::DictKind::OrderedDict) => format!("OrderedDict({body})"),
                        Some(host::DictKind::DefaultDict) => {
                            let f = match meta.and_then(|(_, f)| f) {
                                Some(fv) => py_repr(&fv)?,
                                None => "None".into(),
                            };
                            format!("defaultdict({f}, {body})")
                        }
                        None => body,
                    }
                }
            })
        };
        let result = build();
        host::repr_guard_leave(id);
        return result;
    }
    // See `py_str`: prefetch a `Foreign` object's CPython `repr` outside the borrow.
    Ok(host::repr_of(v))
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
    // copy.copy / copy.deepcopy (native — see call_copy).
    if let Some(f) = name.strip_prefix("copy.") {
        return call_copy(f, &args);
    }
    // functools.total_ordering (native — keeps the decorated class a native
    // pythonrs class; see call_total_ordering).
    if name == "functools.total_ordering" {
        return call_total_ordering(&args);
    }
    // contextlib.redirect_stdout(target) / redirect_stderr(target) — native
    // context managers that retarget pythonrs's own print stream.
    if name == "contextlib.redirect_stdout" || name == "contextlib.redirect_stderr" {
        let target = arg0(&args)?;
        let stderr = name.ends_with("stderr");
        return Ok(with_host(|h| {
            h.alloc(PyObj::Redirect {
                stderr,
                target,
                saved: None,
            })
        }));
    }
    // functools.cached_property(func) — a native non-data descriptor. Its `name`
    // is filled from the class-namespace key at class-build time.
    if name == "functools.cached_property" {
        let func = arg0(&args)?;
        return Ok(with_host(|h| {
            h.alloc(PyObj::CachedProperty {
                func,
                name: String::new(),
            })
        }));
    }
    // sys.* module functions.
    if let Some(f) = name.strip_prefix("sys.") {
        return call_sys(f, args);
    }
    // asyncio.* module functions (native event loop / futures).
    if let Some(f) = name.strip_prefix("asyncio.") {
        return call_asyncio(f, args, kwargs);
    }
    // `posix.*` — the Unix syscall surface.
    if let Some(f) = name.strip_prefix("posix.") {
        return call_posix(f, args, kwargs);
    }
    // `_thread.*` — lock/thread primitives.
    if let Some(f) = name.strip_prefix("_thread.") {
        return match f {
            "allocate_lock" => Ok(with_host(|h| {
                h.alloc(PyObj::Lock {
                    count: 0,
                    reentrant: false,
                })
            })),
            "RLock" => Ok(with_host(|h| {
                h.alloc(PyObj::Lock {
                    count: 0,
                    reentrant: true,
                })
            })),
            // A single, stable identity for the one user thread.
            "get_ident" | "get_native_id" => Ok(Value::Int(1)),
            "start_new_thread" => {
                // Run the target synchronously (no real threads for user code).
                let func = arg0(&args)?;
                let a = args.get(1).cloned().unwrap_or(Value::Undef);
                let call_args = host::iter_vec(&a).unwrap_or_default();
                host::invoke(&func, call_args, vec![])?;
                Ok(Value::Int(1))
            }
            _ => Err(format!("AttributeError: module '_thread' has no attribute '{f}'")),
        };
    }
    // `itertools.*` iterators.
    if let Some(f) = name.strip_prefix("itertools.") {
        return call_itertools(f, args, kwargs);
    }
    // `_string` — the C helpers behind `string.Formatter`.
    if name == "_string.formatter_parser" {
        return string_formatter_parser(&args);
    }
    if name == "_string.formatter_field_name_split" {
        return string_formatter_field_name_split(&args);
    }
    // `<base>.__new__(cls, *args)` — a data type's constructor invoked on a
    // subclass (enum's `_new_member_`, an explicit `int.__new__(MyInt, 5)`).
    // Build a payload-carrying hybrid instance for the subclass; on the base
    // type itself it yields the plain builtin value.
    if let Some(base) = name.strip_suffix(".__new__").filter(|b| {
        matches!(
            *b,
            "int" | "str" | "float" | "tuple" | "frozenset" | "list" | "dict" | "set"
        )
    }) {
        let cls = arg0(&args)?;
        let rest: Vec<Value> = args.iter().skip(1).cloned().collect();
        let cname = with_host(|h| match h.get(&cls) {
            Some(PyObj::Class(n)) => Some(n.clone()),
            _ => None,
        });
        return match cname {
            Some(c) => {
                let payload = call_builtin_function(base, rest, kwargs)?;
                Ok(with_host(|h| h.new_instance_payload(c, payload)))
            }
            // `str.__new__(str, 'x')` on the base type itself → the plain value.
            None => call_builtin_function(base, rest, kwargs),
        };
    }
    // `object.__new__(cls, *args)` — build a bare instance of the class argument
    // (the args beyond `cls` are consumed by `__init__`, per CPython).
    if name == "object.__new__" {
        let cls = arg0(&args)?;
        let cname = with_host(|h| match h.get(&cls) {
            Some(PyObj::Class(n)) => Some(n.clone()),
            _ => None,
        });
        return match cname {
            Some(c) => Ok(with_host(|h| h.new_instance(c, IndexMap::new()))),
            None => Err(host::type_error("object.__new__(X): X is not a type object")),
        };
    }
    // `dict.fromkeys(iterable[, value])` reached via the `dict` type object.
    if name == "dict.fromkeys" {
        return dict_method(&Value::Undef, "fromkeys", &args, &[]);
    }
    // `str.maketrans(...)` reached via the `str` type object.
    if name == "str.maketrans" {
        return str_maketrans(&args);
    }
    // `bytes.maketrans(...)` / `bytearray.maketrans(...)` via the type object.
    if name == "bytes.maketrans" || name == "bytearray.maketrans" {
        return bytes_maketrans(&args);
    }
    // `int.from_bytes(bytes, byteorder='big', *, signed=False)` via the type.
    if name == "int.from_bytes" {
        return int_from_bytes(&args, &kwargs);
    }
    // `float.fromhex(str)` via the type object.
    if name == "float.fromhex" {
        let s = with_host(|h| args.first().and_then(|v| h.as_str(v)))
            .ok_or_else(|| host::type_error("float.fromhex() argument must be str"))?;
        return Ok(Value::Float(float_fromhex(&s)?));
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
    // Unbound builtin instance method reached via the type object
    // (`str.lower(s)`, `list.append(lst, x)`, `dict.get(d, k)`): the first
    // argument is the receiver. Gated by `is_builtin_type` + `type_has_method`
    // so only genuine `type.method` names route here (getattr already rejected a
    // bad name), never a module function like `math.sqrt`.
    if let Some((tp, meth)) = name.split_once('.') {
        if is_builtin_type(tp) && type_has_method(tp, meth) {
            let Some(recv) = args.first().cloned() else {
                return Err(host::type_error(&format!(
                    "descriptor '{meth}' of '{tp}' object needs an argument"
                )));
            };
            return call_type_method(&recv, meth, args[1..].to_vec(), kwargs);
        }
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
            // An explicit `file=` writes to that stream (a native `File` such as
            // `sys.stderr`, or any object with `write` — a `StringIO`); with no
            // `file=`, write to the current `sys.stdout` (honoring a redirect).
            let file = kw_get(&kwargs, "file").filter(|f| !matches!(f, Value::Undef));
            match file {
                Some(f) => host::write_to_stream(&f, &out)?,
                None => host::write_stdout(&out)?,
            }
            Ok(Value::Undef)
        }
        "len" => {
            let v = arg0(&args)?;
            let n = py_len(&v)?;
            Ok(Value::Int(n as i64))
        }
        "range" => make_range(&args),
        // `slice(stop)` / `slice(start, stop[, step])`. Bounds are stored RAW —
        // CPython keeps the objects (`slice(x).start is x`); `__index__` is only
        // applied when the slice is *used* (indexing) or `.indices()` is called.
        "slice" => {
            let (lo, hi, step) = match args.len() {
                1 => (Value::Undef, args[0].clone(), Value::Undef),
                2 => (args[0].clone(), args[1].clone(), Value::Undef),
                3 => (args[0].clone(), args[1].clone(), args[2].clone()),
                0 => {
                    return Err(host::type_error(
                        "slice expected at least 1 argument, got 0",
                    ))
                }
                n => {
                    return Err(host::type_error(&format!(
                        "slice expected at most 3 arguments, got {n}"
                    )))
                }
            };
            Ok(with_host(|h| h.alloc(PyObj::Slice { lo, hi, step })))
        }
        "abs" => {
            let v = arg0(&args)?;
            // Instance overloading: `abs(x)` → `x.__abs__()`.
            if with_host(
                |h| matches!(h.get(&v), Some(PyObj::Instance(i)) if instance_has(h, i, "__abs__")),
            ) {
                return host::call_method(&v, "__abs__", vec![], vec![]);
            }
            // An `int`/`float` subclass with no `__abs__` override: `abs` on the
            // native payload (a plain `int`/`float`).
            let v = host::subclass_operand(&v, "__abs__");
            // A CPython `Foreign` object (stdlib-ffi): `abs(Decimal(...))`,
            // `abs(timedelta(...))`, or a `@dataclass` with a user `__abs__`.
            // Dispatch OUTSIDE the borrow — the real `__abs__` may be a pythonrs
            // method that re-enters the host.
            #[cfg(feature = "stdlib-ffi")]
            if with_host(|h| h.foreign_id(&v).is_some()) {
                return crate::ffi::unary_op_cb("abs", &v);
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
                _ => Err(host::type_error(&format!(
                    "bad operand type for abs(): '{}'",
                    h.type_name(&v)
                ))),
            })
        }
        "min" => reduce_minmax(&args, &kwargs, false),
        "max" => reduce_minmax(&args, &kwargs, true),
        "sum" => {
            // Faithful port of CPython 3.14 `builtin_sum_impl`: an exact integer
            // prefix, then Neumaier compensated summation once the accumulator is
            // a float (so `sum([0.1]*10) == 1.0`), then generic `+` for the tail.
            let seq = arg0(&args)?;
            let start = args.get(1).cloned().unwrap_or(Value::Int(0));
            // CPython rejects str/bytes/bytearray start values up front.
            if let Some(msg) = sum_bad_start(&start) {
                return Err(host::type_error(&format!("sum() can't sum {msg}")));
            }
            let items = host::iter_vec(&seq)?;
            let mut acc = start;
            let mut idx = 0;
            // Exact integer prefix. pythonrs int add auto-promotes to bigint, so
            // this stays exact like CPython's `i_result` fast path; the first
            // non-int item is added generically (int+float -> float), which may
            // enter the float loop below.
            while idx < items.len() && sum_is_long(&acc) {
                let item = &items[idx];
                // A non-int item is added generically (int+float -> float, or
                // int+instance -> `__radd__`), which may enter the float loop;
                // two ints stay on the exact `arith` fast path.
                acc = if sum_is_long(item) {
                    with_host(|h| h.arith(NumOp::Add, &acc, item))?
                } else {
                    numeric_hook(NumOp::Add, &acc, item)?
                };
                idx += 1;
                if !sum_is_long(item) {
                    break;
                }
            }
            // Neumaier compensated float summation.
            if let Value::Float(mut hi) = acc {
                let mut lo = 0.0f64;
                while idx < items.len() {
                    let item = &items[idx];
                    let x = if let Value::Float(x) = *item {
                        x
                    } else if sum_is_long(item) {
                        with_host(|h| h.num_val(item)).unwrap()
                    } else {
                        // Non-float non-int: finalize and fall to the generic tail.
                        break;
                    };
                    let (nh, nl) = cs_add(hi, lo, x);
                    hi = nh;
                    lo = nl;
                    idx += 1;
                }
                acc = Value::Float(cs_to_double(hi, lo));
            }
            // Generic tail (mixed types, complex, Decimal, instances via their
            // `__add__`/`__radd__`, …).
            while idx < items.len() {
                acc = numeric_hook(NumOp::Add, &acc, &items[idx])?;
                idx += 1;
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
                _ => {
                    // An instance / foreign object (`Decimal`, `Fraction`, a user
                    // class) delegates to `x.__round__([n])`.
                    let has_round = with_host(|h| match h.get(&v) {
                        Some(PyObj::Instance(i)) => h.class_lookup(&i.class, "__round__").is_some(),
                        _ => h.foreign_id(&v).is_some(),
                    });
                    if has_round {
                        let margs = if has_nd {
                            vec![args[1].clone()]
                        } else {
                            vec![]
                        };
                        host::call_method(&v, "__round__", margs, vec![])
                    } else {
                        Err(host::type_error(&format!(
                            "type {} doesn't define __round__ method",
                            with_host(|h| h.type_name(&v))
                        )))
                    }
                }
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
            // 3-arg `type(name, bases, ns, **kw)`: dynamic class creation. The
            // most-derived metaclass inherited from the bases wins (CPython's
            // rule) — so `type(name, (StrEnum,), body, boundary=…, _simple=True)`
            // from enum's `_simple_enum` actually invokes `EnumType`, carrying its
            // keywords, rather than registering a plain `type`-metaclass class.
            if args.len() == 3 {
                let base_names: Vec<String> = with_host(|h| match h.get(&args[1]) {
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
                let meta = with_host(|h| host::default_metaclass(h, &base_names));
                if meta != "type" {
                    let metaobj = with_host(|h| h.alloc(PyObj::Class(meta)));
                    return host::invoke(
                        &metaobj,
                        vec![args[0].clone(), args[1].clone(), args[2].clone()],
                        kwargs,
                    );
                }
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
        // `types.SimpleNamespace(**kwargs)` — a mutable attribute bag. CPython
        // takes keyword arguments only (a single positional mapping is also
        // accepted in 3.13+, but the keyword form is what the stdlib uses).
        "SimpleNamespace" => {
            if !args.is_empty() {
                return Err(host::type_error(
                    "no positional arguments expected",
                ));
            }
            Ok(with_host(|h| {
                let mut attrs = indexmap::IndexMap::new();
                for (k, v) in kwargs {
                    attrs.insert(k, v);
                }
                h.alloc(PyObj::Namespace { attrs })
            }))
        }
        // `types.GenericAlias(origin, args)` — the type object (from
        // `type(list[int])`) is callable to build an alias, which is how the
        // stdlib's `__class_getitem__ = classmethod(GenericAlias)` works.
        "GenericAlias" => {
            let origin = arg0(&args)?;
            let idx = args.get(1).cloned().unwrap_or(Value::Undef);
            Ok(with_host(|h| {
                let alias_args = match h.get(&idx) {
                    Some(PyObj::Tuple(items)) => items.clone(),
                    _ => vec![idx.clone()],
                };
                h.alloc(PyObj::GenericAlias {
                    origin,
                    args: alias_args,
                })
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
            isinstance_dispatch(&v, &cls).map(Value::Bool)
        }
        "issubclass" => {
            let a0 = arg0(&args)?;
            let a1 = args.get(1).cloned().unwrap_or(Value::Undef);
            issubclass_dispatch(&a0, &a1).map(Value::Bool)
        }
        "__import__" => {
            // `__import__(name, globals=None, locals=None, fromlist=(), level=0)`.
            // With an empty `fromlist` and a dotted name CPython returns the
            // top-level package; otherwise the named (sub)module.
            let name_v = arg0(&args)?;
            let name = with_host(|h| h.str_of(&name_v));
            let fromlist_empty = match args.get(3) {
                None | Some(Value::Undef) => true,
                Some(v) => with_host(|h| !h.truthy(v)),
            };
            let target = if fromlist_empty {
                name.split('.').next().unwrap_or(&name).to_string()
            } else {
                name
            };
            host::import_module(&target)
        }
        "hasattr" => {
            let v = arg0(&args)?;
            let n = with_host(|h| h.str_of(&args.get(1).cloned().unwrap_or(Value::Undef)));
            // Only AttributeError becomes `False`; any other exception propagates.
            match get_attr_desc(&v, &n) {
                Ok(_) => Ok(Value::Bool(true)),
                Err(e) if is_attr_err(&e) => Ok(Value::Bool(false)),
                Err(e) => Err(e),
            }
        }
        "getattr" => {
            let v = arg0(&args)?;
            let n = with_host(|h| h.str_of(&args.get(1).cloned().unwrap_or(Value::Undef)));
            match get_attr_desc(&v, &n) {
                Ok(x) => Ok(x),
                // The default substitutes only for AttributeError; a getter
                // raising anything else propagates even when a default is given.
                Err(e) => match args.get(2) {
                    Some(d) if is_attr_err(&e) => Ok(d.clone()),
                    _ => Err(e),
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
        "delattr" => {
            let v = arg0(&args)?;
            let n = with_host(|h| h.str_of(&args.get(1).cloned().unwrap_or(Value::Undef)));
            del_attr_desc(&v, &n)?;
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
            // A `str` of exactly one character, or a `bytes`/`bytearray` of length
            // one; any other length is a TypeError naming the actual length.
            let n = with_host(|h| -> Result<i64, String> {
                if let Some(s) = h.as_str(&a0) {
                    let len = s.chars().count();
                    return if len == 1 {
                        Ok(s.chars().next().unwrap() as i64)
                    } else {
                        Err(host::type_error(&format!(
                            "ord() expected a character, but string of length {len} found"
                        )))
                    };
                }
                match h.get(&a0) {
                    Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) if b.len() == 1 => {
                        Ok(b[0] as i64)
                    }
                    Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => {
                        Err(host::type_error(&format!(
                            "ord() expected a character, but string of length {} found",
                            b.len()
                        )))
                    }
                    _ => Err(host::type_error(&format!(
                        "ord() expected string of length 1, but {} found",
                        h.type_name(&a0)
                    ))),
                }
            })?;
            Ok(Value::Int(n))
        }
        "chr" => {
            let a0 = arg0(&args)?;
            let n = with_host(|h| h.as_int(&a0)).unwrap_or(0);
            match char::from_u32(n as u32) {
                Some(c) => Ok(with_host(|h| h.new_str(c.to_string()))),
                // Rust `char` can't hold a lone surrogate (U+D800..U+DFFF), so
                // `chr(surrogate)` errors here where CPython would return a
                // surrogate-bearing `str`; that gap needs a surrogate-aware
                // string type. Both the surrogate and out-of-range cases share
                // CPython's message text.
                None => Err("ValueError: chr() arg not in range(0x110000)".to_string()),
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
            // Two-argument form: `iter(callable, sentinel)` calls `callable()`
            // repeatedly, yielding results until one equals `sentinel`.
            if let Some(sentinel) = args.get(1) {
                let sentinel = sentinel.clone();
                return Ok(with_host(|h| {
                    h.alloc(PyObj::CallIter {
                        func: v,
                        sentinel,
                        done: false,
                    })
                }));
            }
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
        "dir" => Ok(with_host(|h| {
            let names = match args.first() {
                Some(v) => h.dir_names(v),
                // Bare `dir()` (module scope) is not modeled; return empty.
                None => Vec::new(),
            };
            let items: Vec<Value> = names.into_iter().map(|n| h.new_str(n)).collect();
            h.new_list(items)
        })),
        // `globals()` / `locals()` as a dict. A snapshot (not a live view): reads
        // and passing to `eval`/`exec` work; in-place mutation is not reflected
        // back into the namespace. `locals()` at module scope is `globals()`.
        "globals" => Ok(str_keyed_dict(with_host(|h| h.globals_pairs()))),
        "locals" => Ok(str_keyed_dict(with_host(|h| {
            if h.frame_depth() > 1 {
                h.caller_locals().into_iter().collect()
            } else {
                h.globals_pairs()
            }
        }))),
        // `eval(expr[, globals[, locals]])` evaluates a single expression string
        // and returns its value; `exec(code[, globals[, locals]])` runs statements
        // and returns None. Both compile the source on the fly and re-enter the VM
        // on the current host (so names resolve against — and assignments land in —
        // the live module globals), exactly as the REPL runs a line.
        "eval" | "exec" => run_pysource(name == "eval", &args),
        // Type constructors.
        "int" => {
            // Fold an `int(x, base=B)` keyword into the positional base slot.
            let mut a = args.clone();
            if let Some(base) = kw_get(&kwargs, "base") {
                if a.len() >= 2 {
                    a[1] = base;
                } else {
                    if a.is_empty() {
                        a.push(Value::str(""));
                    }
                    a.push(base);
                }
            }
            construct_int(&a)
        }
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
        "memoryview" => {
            let v = arg0(&args)?;
            with_host(|h| {
                let (len, readonly) = match h.get(&v) {
                    Some(PyObj::Bytes(b)) => (b.len(), true),
                    Some(PyObj::Bytearray(b)) => (b.len(), false),
                    // A memoryview of a memoryview shares the same window.
                    Some(PyObj::Memoryview {
                        obj,
                        start,
                        len,
                        readonly,
                    }) => {
                        let (obj, start, len, readonly) = (obj.clone(), *start, *len, *readonly);
                        return Ok(h.alloc(PyObj::Memoryview {
                            obj,
                            start,
                            len,
                            readonly,
                        }));
                    }
                    _ => {
                        return Err(host::type_error(&format!(
                            "memoryview: a bytes-like object is required, not '{}'",
                            h.type_name(&v)
                        )))
                    }
                };
                Ok(h.alloc(PyObj::Memoryview {
                    obj: v.clone(),
                    start: 0,
                    len,
                    readonly,
                }))
            })
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
            h.new_instance("object".into(), IndexMap::new())
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
    // Negative exponent → modular inverse of `base` (CPython 3.8+):
    // `pow(base, -k, m) == pow(modinv(base, m), k, m)`.
    let (base, exp) = if exp < zero {
        use num_integer::Integer;
        let m_abs = if modulus < zero {
            -&modulus
        } else {
            modulus.clone()
        };
        let base_r = base.mod_floor(&m_abs);
        let egcd = base_r.extended_gcd(&m_abs);
        if egcd.gcd != BigInt::from(1) {
            return Err("ValueError: base is not invertible for the given modulus".into());
        }
        (egcd.x.mod_floor(&m_abs), -exp)
    } else {
        (base, exp)
    };
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

/// Shared implementation of `eval`/`exec`. `want_value` is true for `eval` (the
/// source is a single expression whose value is returned); false for `exec`
/// (statements, returning `None`). `args` is `[source, globals?, locals?]`.
///
/// The source is compiled on the fly and run by re-entering the VM on the current
/// host — a fresh VM instance sharing the live module globals and heap (the same
/// mechanism the REPL uses per line). With no namespace argument, names resolve
/// against and assignments land in the real module globals. A `globals` dict
/// replaces the module globals for the run (saved and restored afterward, with
/// post-run bindings copied back into the dict); a `locals` dict is overlaid on
/// top. Builtins resolve through a separate registry, so they stay available.
fn run_pysource(want_value: bool, args: &[Value]) -> Result<Value, String> {
    let fname = if want_value { "eval" } else { "exec" };
    let src = match args.first() {
        // A pre-compiled code object is unsupported (pythonrs has no code
        // objects); a string source is required.
        Some(v) => with_host(|h| h.as_str(v)).ok_or_else(|| {
            host::type_error(&format!(
                "{fname}() arg 1 must be a string, bytes or code object"
            ))
        })?,
        None => {
            return Err(host::type_error(&format!(
                "{fname}() missing required argument: 'source' (pos 1)"
            )))
        }
    };

    let ns_arg = |i: usize| match args.get(i) {
        Some(v) if !matches!(v, Value::Undef) => Some(v.clone()),
        _ => None,
    };
    let globals_arg = ns_arg(1);
    let locals_arg = ns_arg(2);
    let explicit_ns = globals_arg.is_some() || locals_arg.is_some();
    // With no explicit namespace, a call from inside a function runs against the
    // module globals with the caller's locals overlaid, and its writes are
    // DISCARDED afterward — CPython's rule that exec/eval in a function reads
    // locals but cannot persist to them or to globals. At module scope, writes go
    // straight to the real module globals and persist.
    let in_function = with_host(|h| h.frame_depth() > 1);
    let sandboxed = explicit_ns || in_function;

    // `eval` requires exactly one expression. Validate the RAW (leading-whitespace
    // stripped, as CPython does) source first, so a statement, a semicolon/newline
    // series, or a bare newline after an operator is a SyntaxError — the wrapper's
    // parens below would otherwise make some of those parse.
    if want_value {
        let stmts = crate::parser::parse(src.trim())?;
        if stmts.len() != 1 || !matches!(stmts[0].kind, crate::ast::StmtKind::Expr(_)) {
            return Err("SyntaxError: invalid syntax".to_string());
        }
    }
    // Bind the (validated) expression to a temporary so its value can be read back.
    // The surrounding newlines let a multi-line expression (one whose newlines sit
    // inside its own brackets) parse and stop a trailing `# comment` from swallowing
    // the closing paren.
    const TMP: &str = "__pyrs_eval_result__";
    let to_compile = if want_value {
        format!("{TMP} = (\n{src}\n)")
    } else {
        src
    };

    let saved = if sandboxed {
        let snap = with_host(|h| h.snapshot_globals());
        let mut ns: IndexMap<String, Value> = if explicit_ns {
            // The provided globals/locals dicts ARE the namespace.
            IndexMap::new()
        } else {
            // In-function, no explicit namespace: module globals + caller locals.
            let mut base = snap.clone();
            for (k, v) in with_host(|h| h.caller_locals()) {
                base.insert(k, v);
            }
            base
        };
        for d in [globals_arg.as_ref(), locals_arg.as_ref()]
            .into_iter()
            .flatten()
        {
            for (k, v) in dict_str_pairs(d)? {
                ns.insert(k, v);
            }
        }
        with_host(|h| h.replace_globals(ns));
        Some(snap)
    } else {
        None
    };

    // Park any active function frames so the nested chunk runs at module scope
    // (its globals reach the real module namespace, not the caller's locals).
    let parked = with_host(|h| h.enter_module_scope());
    let result = (|| -> Result<Value, String> {
        let prog = crate::compile(&to_compile)?;
        let chunk = crate::load_merged(prog);
        crate::host::run_chunk_on(chunk)?;
        Ok(if want_value {
            with_host(|h| h.del_global(TMP)).unwrap_or(Value::Undef)
        } else {
            Value::Undef
        })
    })();
    with_host(|h| h.restore_scope(parked));

    if let Some(saved) = saved {
        // With an explicit `globals` dict, copy the post-run namespace back into it
        // (so `exec("x=1", d)` leaves `d["x"] == 1`). The in-function overlay case
        // writes nothing back — its assignments are discarded. Either way, restore
        // the interpreter's real module globals.
        if explicit_ns {
            if let Some(g) = &globals_arg {
                for (k, v) in with_host(|h| h.globals_pairs()) {
                    if k == TMP {
                        continue;
                    }
                    let key = with_host(|h| h.new_str(k));
                    with_host(|h| h.set_item(g, &key, v))?;
                }
            }
        }
        with_host(|h| h.replace_globals(saved));
    }
    result
}

/// Build a `dict` from string-keyed `(name, value)` pairs — the shape returned by
/// `globals()`/`locals()`, insertion order preserved.
fn str_keyed_dict(pairs: Vec<(String, Value)>) -> Value {
    with_host(|h| {
        let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::with_capacity(pairs.len());
        for (k, v) in pairs {
            let key = h.new_str(k.clone());
            d.insert(PKey::Str(k), (key, v));
        }
        h.new_dict(d)
    })
}

/// A dict's string-keyed `(name, value)` entries, for an `eval`/`exec` namespace
/// argument. Non-string keys are skipped (a namespace is keyed by identifier).
fn dict_str_pairs(d: &Value) -> Result<Vec<(String, Value)>, String> {
    with_host(|h| match h.get(d) {
        Some(PyObj::Dict(m)) => Ok(m
            .iter()
            .filter_map(|(_, (kv, v))| h.as_str(kv).map(|s| (s, v.clone())))
            .collect()),
        _ => Err(host::type_error("globals must be a real dictionary")),
    })
}

fn arg0(args: &[Value]) -> Result<Value, String> {
    args.first()
        .cloned()
        .ok_or_else(|| host::type_error("missing required argument"))
}

pub fn hash_key(k: &PKey) -> i64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    // A Foreign key's user-visible `hash()` is CPython's own `hash(obj)` (the
    // `hash` field), independent of the handle's heap id — so value-equal objects
    // (`Decimal('1.5')` fetched twice) report equal hashes. The `id` only
    // discriminates map slots after `prepare_key`'s value collapse.
    if let PKey::Foreign { hash, .. } = k {
        return *hash;
    }
    let mut h = DefaultHasher::new();
    k.hash(&mut h);
    h.finish() as i64
}

pub fn py_len(v: &Value) -> Result<usize, String> {
    // A CPython `Foreign` object (stdlib-ffi): `len()` runs its `__len__` in
    // CPython OUTSIDE the borrow, so a `@dataclass` with a user `__len__` can
    // re-enter the host without a double borrow.
    #[cfg(feature = "stdlib-ffi")]
    if let Some(fid) = with_host(|h| h.foreign_id(v)) {
        return crate::ffi::len(fid);
    }
    with_host(|h| match h.get(v) {
        Some(PyObj::Str(s)) => Ok(s.chars().count()),
        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Ok(b.len()),
        Some(PyObj::Memoryview { len, .. }) => Ok(*len),
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
        Some(PyObj::BigRange { start, stop, step }) => {
            use num_traits::ToPrimitive;
            host::big_range_len(start, stop, step).to_usize().ok_or_else(|| {
                "OverflowError: cannot fit 'int' into an index-sized integer".to_string()
            })
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
    if !matches!(args.len(), 1..=3) {
        return Err(host::type_error("range expected 1 to 3 arguments"));
    }
    // Fast path: every argument fits `i64`.
    let small: Option<Vec<i64>> = args.iter().map(|v| with_host(|h| h.as_int(v))).collect();
    if let Some(ints) = small {
        let (start, stop, step) = match ints.len() {
            1 => (0, ints[0], 1),
            2 => (ints[0], ints[1], 1),
            _ => (ints[0], ints[1], ints[2]),
        };
        if step == 0 {
            return Err("ValueError: range() arg 3 must not be zero".into());
        }
        return Ok(with_host(|h| h.alloc(PyObj::Range { start, stop, step })));
    }
    // A bound overflows `i64`: promote to a bignum range (`range(1 << 1000)`).
    let bigs: Vec<num_bigint::BigInt> = args
        .iter()
        .map(|v| {
            with_host(|h| h.big_val(v))
                .ok_or_else(|| host::type_error("'range' requires integer arguments"))
        })
        .collect::<Result<_, _>>()?;
    use num_traits::{One, Zero};
    let (start, stop, step) = match bigs.len() {
        1 => (num_bigint::BigInt::zero(), bigs[0].clone(), num_bigint::BigInt::one()),
        2 => (bigs[0].clone(), bigs[1].clone(), num_bigint::BigInt::one()),
        _ => (bigs[0].clone(), bigs[1].clone(), bigs[2].clone()),
    };
    if step.is_zero() {
        return Err("ValueError: range() arg 3 must not be zero".into());
    }
    Ok(with_host(|h| h.alloc(PyObj::BigRange { start, stop, step })))
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
            "ValueError: {}() iterable argument is empty",
            if want_max { "max" } else { "min" }
        ));
    }
    let key = kw_get(kwargs, "key");
    let mut best = items[0].clone();
    let mut best_k = eval_key(&key, &best)?;
    for it in &items[1..] {
        let k = eval_key(&key, it)?;
        // Strict replacement so ties keep the FIRST element (CPython: `max`
        // returns the first maximal, `min` the first minimal). A non-strict test
        // (`!(k > best_k)` for min) would overwrite on equal keys.
        let cmp = if want_max { NumOp::Gt } else { NumOp::Lt };
        let take = numeric_hook(cmp, &k, &best_k)?;
        if with_host(|h| h.truthy(&take)) {
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
    // Stable sort using host ordering. A tie must map to `Ordering::Equal` so the
    // stable sort keeps the original relative order (CPython's guaranteed
    // stability). `reverse` flips the compared operands rather than reversing the
    // output list, so equal elements retain their ORIGINAL order in both
    // directions — CPython documents `reverse` "as if each comparison were
    // reversed", which is a stable descending sort, not `sorted(...)[::-1]`.
    let mut err: Option<String> = None;
    keyed.sort_by(|a, b| {
        if err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        let (lo, hi) = if reverse { (&b.0, &a.0) } else { (&a.0, &b.0) };
        // lo < hi ?  ->  Less
        match numeric_hook(NumOp::Lt, lo, hi) {
            Ok(v) if with_host(|h| h.truthy(&v)) => std::cmp::Ordering::Less,
            Ok(_) => {
                // Not less: distinguish Greater (hi < lo) from Equal (tie).
                match numeric_hook(NumOp::Lt, hi, lo) {
                    Ok(v2) if with_host(|h| h.truthy(&v2)) => std::cmp::Ordering::Greater,
                    Ok(_) => std::cmp::Ordering::Equal,
                    Err(e) => {
                        err = Some(e);
                        std::cmp::Ordering::Equal
                    }
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
    let out: Vec<Value> = keyed.into_iter().map(|(_, v)| v).collect();
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
    // `bin`/`hex`/`oct` accept anything with `__index__`.
    let a0 = match index_dunder(&a0)? {
        Some(v) => v,
        None => a0,
    };
    let n = with_host(|h| h.big_val(&a0)).ok_or_else(|| {
        let t = with_host(|h| h.type_name(&a0));
        host::type_error(&format!("'{t}' object cannot be interpreted as an integer"))
    })?;
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
    // `int(x)` with no explicit base honors `__int__`, then `__index__`.
    if args.len() < 2 {
        // A live CPython object (an `IntEnum` member, `Fraction`, `numpy` scalar)
        // converts via CPython's own `int()`.
        #[cfg(feature = "stdlib-ffi")]
        if let Some(id) = with_host(|h| h.foreign_id(&v)) {
            // Borrow-free: a `@dataclass` with a user `__int__` re-enters the host.
            return crate::ffi::to_int_cb(id);
        }
        let has_int = with_host(
            |h| matches!(h.get(&v), Some(PyObj::Instance(i)) if instance_has(h, i, "__int__")),
        );
        if has_int {
            let r = host::call_method(&v, "__int__", vec![], vec![])?;
            if !is_int_value(&r) {
                let t = with_host(|h| h.type_name(&r));
                return Err(host::type_error(&format!(
                    "__int__ returned non-int (type {t})"
                )));
            }
            return Ok(r);
        }
        if let Some(r) = index_dunder(&v)? {
            return Ok(r);
        }
    }
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
            let s = h.as_str(&v).ok_or_else(|| {
                host::type_error(&format!(
                    "int() argument must be a string, a bytes-like object or a real number, not '{}'",
                    h.type_name(&v)
                ))
            })?;
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
    // A foreign (CPython) object runs through CPython's own `float()` so its
    // `__float__`/`__index__` (`Fraction`, `Decimal`, …) are honored.
    #[cfg(feature = "stdlib-ffi")]
    if let Some(id) = with_host(|h| h.foreign_id(&v)) {
        return crate::ffi::to_float(id).map(Value::Float);
    }
    // `float(x)` honors `__float__`, then `__index__`.
    let has_float = with_host(
        |h| matches!(h.get(&v), Some(PyObj::Instance(i)) if instance_has(h, i, "__float__")),
    );
    if has_float {
        let r = host::call_method(&v, "__float__", vec![], vec![])?;
        let ok = matches!(r, Value::Float(_));
        if !ok {
            let t = with_host(|h| h.type_name(&r));
            return Err(host::type_error(&format!(
                "__float__ returned non-float (type {t})"
            )));
        }
        return Ok(r);
    }
    if let Some(r) = index_dunder(&v)? {
        // An `__index__` int converts to float.
        let f = with_host(|h| {
            use num_traits::ToPrimitive;
            match &r {
                Value::Int(n) => *n as f64,
                Value::Bool(b) => *b as i64 as f64,
                _ => match h.big_val(&r) {
                    Some(b) => b.to_f64().unwrap_or(f64::INFINITY),
                    None => 0.0,
                },
            }
        });
        return Ok(Value::Float(f));
    }
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
            let s = h.as_str(&v).ok_or_else(|| {
                host::type_error(&format!(
                    "float() argument must be a string or a real number, not '{}'",
                    h.type_name(&v)
                ))
            })?;
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
        // dict(mapping): a real dict OR a dict-subclass instance (`class D(dict)`),
        // whose native dict lives in its payload — unwrap it so `dict(d)` copies
        // the pairs instead of iterating the instance as bare keys.
        let dict_map = with_host(|h| {
            let src = match h.get(v) {
                Some(PyObj::Dict(_)) => Some(v.clone()),
                Some(PyObj::Instance(inst)) if !matches!(inst.payload, Value::Undef) => {
                    Some(inst.payload.clone())
                }
                _ => None,
            };
            src.and_then(|s| match h.get(&s) {
                Some(PyObj::Dict(m)) => Some(m.clone()),
                _ => None,
            })
        });
        // A foreign (CPython) mapping (ChainMap, a custom Mapping, …) exposes
        // `keys()`; `dict(mapping)` copies via keys()+subscript, matching CPython,
        // instead of iterating it as bare keys.
        let foreign_keys = if dict_map.is_none() && with_host(|h| h.foreign_id(v)).is_some() {
            host::call_method(v, "keys", vec![], vec![]).ok()
        } else {
            None
        };
        if let Some(m) = dict_map {
            d = m;
        } else if let Some(keys) = foreign_keys {
            for k in host::iter_vec(&keys)? {
                let val = with_host(|h| h.get_item(v, &k))?;
                let key = with_host(|h| h.to_key(&k))?;
                host::dict_put(&mut d, key, k, val);
            }
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
/// `_string.formatter_parser(fmt)` — the `str.format` field parser. Yields
/// `(literal_text, field_name, format_spec, conversion)` per replacement field;
/// a trailing literal is `(text, None, None, None)`.
fn string_formatter_parser(args: &[Value]) -> Result<Value, String> {
    let s = with_host(|h| args.first().and_then(|v| h.as_str(v)))
        .ok_or_else(|| host::type_error("formatter_parser() argument must be str"))?;
    let cs: Vec<char> = s.chars().collect();
    let n = cs.len();
    let mut i = 0;
    type Field = (String, Option<String>, Option<String>, Option<char>);
    let mut out: Vec<Field> = Vec::new();
    while i < n {
        let mut lit = String::new();
        let mut has_field = false;
        while i < n {
            let c = cs[i];
            if c == '{' || c == '}' {
                if i + 1 < n && cs[i + 1] == c {
                    lit.push(c);
                    i += 2;
                    continue;
                }
                if c == '}' {
                    return Err("ValueError: Single '}' encountered in format string".into());
                }
                has_field = true;
                i += 1; // skip '{'
                break;
            }
            lit.push(c);
            i += 1;
        }
        if !has_field {
            if !lit.is_empty() {
                out.push((lit, None, None, None));
            }
            break;
        }
        let mut field_name = String::new();
        let mut conversion: Option<char> = None;
        let mut format_spec = String::new();
        while i < n && cs[i] != '!' && cs[i] != ':' && cs[i] != '}' {
            field_name.push(cs[i]);
            i += 1;
        }
        if i < n && cs[i] == '!' {
            i += 1;
            if i >= n || cs[i] == '}' || cs[i] == ':' {
                return Err(
                    "ValueError: end of string while looking for conversion specifier".into(),
                );
            }
            conversion = Some(cs[i]);
            i += 1;
        }
        if i < n && cs[i] == ':' {
            i += 1;
            let mut depth = 1;
            while i < n {
                let c = cs[i];
                if c == '{' {
                    depth += 1;
                } else if c == '}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                format_spec.push(c);
                i += 1;
            }
        }
        if i >= n || cs[i] != '}' {
            return Err("ValueError: expected '}' before end of string".into());
        }
        i += 1; // skip closing '}'
        out.push((lit, Some(field_name), Some(format_spec), conversion));
    }
    Ok(with_host(|h| {
        let mut tuples: Vec<Value> = Vec::with_capacity(out.len());
        for (lit, fname, fspec, conv) in out {
            let lit_v = h.new_str(lit);
            let fname_v = fname.map(|s| h.new_str(s)).unwrap_or(Value::Undef);
            let fspec_v = fspec.map(|s| h.new_str(s)).unwrap_or(Value::Undef);
            let conv_v = conv.map(|c| h.new_str(c.to_string())).unwrap_or(Value::Undef);
            tuples.push(h.new_tuple(vec![lit_v, fname_v, fspec_v, conv_v]));
        }
        h.new_list(tuples)
    }))
}

/// `_string.formatter_field_name_split(name)` — `(first, rest)` where `first` is
/// the leading arg name (str) or index (int), and `rest` iterates
/// `(is_attr, key)` for each `.attr` / `[item]` access.
fn string_formatter_field_name_split(args: &[Value]) -> Result<Value, String> {
    let s = with_host(|h| args.first().and_then(|v| h.as_str(v)))
        .ok_or_else(|| host::type_error("formatter_field_name_split() argument must be str"))?;
    let cs: Vec<char> = s.chars().collect();
    let n = cs.len();
    let mut i = 0;
    let mut first = String::new();
    while i < n && cs[i] != '.' && cs[i] != '[' {
        first.push(cs[i]);
        i += 1;
    }
    // (is_attr, key, key_is_int)
    let mut rest: Vec<(bool, String, bool)> = Vec::new();
    while i < n {
        if cs[i] == '.' {
            i += 1;
            let mut nm = String::new();
            while i < n && cs[i] != '.' && cs[i] != '[' {
                nm.push(cs[i]);
                i += 1;
            }
            rest.push((true, nm, false));
        } else if cs[i] == '[' {
            i += 1;
            let mut key = String::new();
            while i < n && cs[i] != ']' {
                key.push(cs[i]);
                i += 1;
            }
            if i >= n {
                return Err("ValueError: Missing ']' in format string".into());
            }
            i += 1; // skip ']'
            let is_int = !key.is_empty() && key.chars().all(|c| c.is_ascii_digit());
            rest.push((false, key, is_int));
        } else {
            return Err(
                "ValueError: Only '.' or '[' may follow ']' in format field specifier".into(),
            );
        }
    }
    let first_is_int = !first.is_empty() && first.chars().all(|c| c.is_ascii_digit());
    Ok(with_host(|h| {
        let first_v = if first_is_int {
            Value::Int(first.parse().unwrap_or(0))
        } else {
            h.new_str(first)
        };
        let mut items: Vec<Value> = Vec::with_capacity(rest.len());
        for (is_attr, key, key_is_int) in rest {
            let key_v = if !is_attr && key_is_int {
                Value::Int(key.parse().unwrap_or(0))
            } else {
                h.new_str(key)
            };
            items.push(h.new_tuple(vec![Value::Bool(is_attr), key_v]));
        }
        let rest_list = h.new_list(items);
        h.new_tuple(vec![first_v, rest_list])
    }))
}

/// Construct an `itertools` iterator. Lazy ones build an `ItertoolsIter`; the
/// combinatorics build the full tuple list and return its iterator.
fn call_itertools(name: &str, args: Vec<Value>, kwargs: Vec<(String, Value)>) -> Result<Value, String> {
    use host::{ItKind, PyObj};
    let mk = |kind: ItKind, sources: Vec<Value>, func: Value, nums: Vec<i64>, buf: Vec<Value>, flag: bool| {
        with_host(|h| {
            h.alloc(PyObj::ItertoolsIter {
                kind,
                sources,
                func,
                nums,
                buf,
                flag,
                done: false,
            })
        })
    };
    let iter_of = |v: &Value| -> Result<Value, String> { with_host(|h| h.make_iter(v)) };
    let as_i = |v: &Value| with_host(|h| h.as_int(v));
    let kw = |k: &str| kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    match name {
        "count" => {
            let start = args.first().and_then(|v| as_i(v)).unwrap_or(0);
            let step = args.get(1).and_then(|v| as_i(v)).unwrap_or(1);
            Ok(mk(ItKind::Count, vec![], Value::Undef, vec![start, step], vec![], false))
        }
        "repeat" => {
            let obj = args.first().cloned().unwrap_or(Value::Undef);
            let times = args
                .get(1)
                .cloned()
                .or_else(|| kw("times"))
                .and_then(|v| as_i(&v))
                .unwrap_or(-1);
            Ok(mk(ItKind::Repeat, vec![], Value::Undef, vec![times], vec![obj], false))
        }
        "cycle" => {
            let src = iter_of(&arg0(&args)?)?;
            Ok(mk(ItKind::Cycle, vec![src], Value::Undef, vec![0], vec![], false))
        }
        "chain" => {
            let mut sources = Vec::with_capacity(args.len());
            for a in &args {
                sources.push(iter_of(a)?);
            }
            Ok(mk(ItKind::Chain, sources, Value::Undef, vec![0], vec![], false))
        }
        "chain.from_iterable" => {
            let outer = host::iter_vec(&arg0(&args)?)?;
            let mut sources = Vec::with_capacity(outer.len());
            for a in &outer {
                sources.push(iter_of(a)?);
            }
            Ok(mk(ItKind::Chain, sources, Value::Undef, vec![0], vec![], false))
        }
        "accumulate" => {
            let src = iter_of(&arg0(&args)?)?;
            let func = args.get(1).cloned().or_else(|| kw("func")).unwrap_or(Value::Undef);
            Ok(mk(ItKind::Accumulate, vec![src], func, vec![], vec![], false))
        }
        "starmap" => {
            let func = arg0(&args)?;
            let src = iter_of(args.get(1).ok_or_else(|| host::type_error("starmap expected 2 arguments"))?)?;
            Ok(mk(ItKind::StarMap, vec![src], func, vec![], vec![], false))
        }
        "compress" => {
            let data = iter_of(&arg0(&args)?)?;
            let sel = iter_of(args.get(1).ok_or_else(|| host::type_error("compress expected 2 arguments"))?)?;
            Ok(mk(ItKind::Compress, vec![data, sel], Value::Undef, vec![], vec![], false))
        }
        "dropwhile" => {
            let func = arg0(&args)?;
            let src = iter_of(args.get(1).ok_or_else(|| host::type_error("dropwhile expected 2 arguments"))?)?;
            Ok(mk(ItKind::DropWhile, vec![src], func, vec![], vec![], true))
        }
        "takewhile" => {
            let func = arg0(&args)?;
            let src = iter_of(args.get(1).ok_or_else(|| host::type_error("takewhile expected 2 arguments"))?)?;
            Ok(mk(ItKind::TakeWhile, vec![src], func, vec![], vec![], false))
        }
        "filterfalse" => {
            let func = args.first().cloned().unwrap_or(Value::Undef);
            // filterfalse(None, it): None is already Value::Undef (the identity sentinel).
            let func = func;
            let src = iter_of(args.get(1).ok_or_else(|| host::type_error("filterfalse expected 2 arguments"))?)?;
            Ok(mk(ItKind::FilterFalse, vec![src], func, vec![], vec![], false))
        }
        "islice" => {
            let src = iter_of(&arg0(&args)?)?;
            // islice(it, stop) | islice(it, start, stop[, step])
            let (start, stop, step) = if args.len() <= 2 {
                (0, args.get(1).and_then(|v| as_i(v)).unwrap_or(-1), 1)
            } else {
                (
                    args.get(1).and_then(|v| as_i(v)).unwrap_or(0),
                    args.get(2).and_then(|v| as_i(v)).unwrap_or(-1),
                    args.get(3).and_then(|v| as_i(v)).unwrap_or(1),
                )
            };
            // nums = [next_yield_index=start, stop, step, cursor=0]
            Ok(mk(ItKind::ISlice, vec![src], Value::Undef, vec![start, stop, step, 0], vec![], false))
        }
        "zip_longest" => {
            let fill = kw("fillvalue").unwrap_or(Value::Undef);
            let mut sources = Vec::with_capacity(args.len());
            for a in &args {
                sources.push(iter_of(a)?);
            }
            Ok(mk(ItKind::ZipLongest, sources, fill, vec![], vec![], false))
        }
        "pairwise" => {
            let src = iter_of(&arg0(&args)?)?;
            Ok(mk(ItKind::Pairwise, vec![src], Value::Undef, vec![], vec![], false))
        }
        "product" => itertools_product(&args, &kwargs),
        "permutations" => itertools_permutations(&args),
        "combinations" => itertools_combinations(&args, false),
        "combinations_with_replacement" => itertools_combinations(&args, true),
        "tee" => itertools_tee(&args),
        "groupby" => itertools_groupby(&args, &kwargs),
        _ => Err(format!("AttributeError: module 'itertools' has no attribute '{name}'")),
    }
}

/// A list-iterator over `items` (for the eager combinatoric itertools functions).
fn list_iter(items: Vec<Value>) -> Value {
    with_host(|h| {
        let l = h.new_list(items);
        h.make_iter(&l).unwrap_or(l)
    })
}

fn itertools_product(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let repeat = kwargs
        .iter()
        .find(|(n, _)| n == "repeat")
        .and_then(|(_, v)| with_host(|h| h.as_int(v)))
        .unwrap_or(1)
        .max(0) as usize;
    let mut pools: Vec<Vec<Value>> = Vec::new();
    for a in args {
        pools.push(host::iter_vec(a)?);
    }
    let base = pools.clone();
    for _ in 1..repeat {
        pools.extend(base.iter().cloned());
    }
    if repeat == 0 {
        pools.clear();
    }
    // Cartesian product.
    let mut result: Vec<Vec<Value>> = vec![vec![]];
    for pool in &pools {
        let mut next = Vec::new();
        for prefix in &result {
            for item in pool {
                let mut row = prefix.clone();
                row.push(item.clone());
                next.push(row);
            }
        }
        result = next;
    }
    let tuples: Vec<Value> = result.into_iter().map(|r| with_host(|h| h.new_tuple(r))).collect();
    Ok(list_iter(tuples))
}

fn itertools_permutations(args: &[Value]) -> Result<Value, String> {
    let pool = host::iter_vec(&arg0(args)?)?;
    let n = pool.len();
    let r = args
        .get(1)
        .filter(|v| !matches!(v, Value::Undef))
        .and_then(|v| with_host(|h| h.as_int(v)))
        .map(|x| x as usize)
        .unwrap_or(n);
    let mut out: Vec<Value> = Vec::new();
    if r <= n {
        let mut indices: Vec<usize> = (0..n).collect();
        // Standard CPython permutations via a cycles array.
        let mut cycles: Vec<usize> = (n - r + 1..=n).rev().collect();
        let emit = |indices: &[usize]| {
            let row: Vec<Value> = indices[..r].iter().map(|&i| pool[i].clone()).collect();
            with_host(|h| h.new_tuple(row))
        };
        out.push(emit(&indices));
        'outer: loop {
            let mut i = r;
            loop {
                if i == 0 {
                    break 'outer;
                }
                i -= 1;
                cycles[i] -= 1;
                if cycles[i] == 0 {
                    let first = indices[i];
                    for j in i..n - 1 {
                        indices[j] = indices[j + 1];
                    }
                    indices[n - 1] = first;
                    cycles[i] = n - i;
                } else {
                    let j = n - cycles[i];
                    indices.swap(i, j);
                    out.push(emit(&indices));
                    break;
                }
            }
        }
    }
    Ok(list_iter(out))
}

fn itertools_combinations(args: &[Value], with_repl: bool) -> Result<Value, String> {
    let pool = host::iter_vec(&arg0(args)?)?;
    let n = pool.len();
    let r = args
        .get(1)
        .and_then(|v| with_host(|h| h.as_int(v)))
        .ok_or_else(|| host::type_error("combinations() missing r"))?
        .max(0) as usize;
    let mut out: Vec<Value> = Vec::new();
    let emit = |indices: &[usize]| {
        let row: Vec<Value> = indices.iter().map(|&i| pool[i].clone()).collect();
        with_host(|h| h.new_tuple(row))
    };
    if with_repl {
        if n == 0 && r > 0 {
            return Ok(list_iter(out));
        }
        let mut indices = vec![0usize; r];
        out.push(emit(&indices));
        loop {
            let mut i = r;
            loop {
                if i == 0 {
                    return Ok(list_iter(out));
                }
                i -= 1;
                if indices[i] != n - 1 {
                    break;
                }
            }
            let v = indices[i] + 1;
            for j in i..r {
                indices[j] = v;
            }
            out.push(emit(&indices));
        }
    } else {
        if r > n {
            return Ok(list_iter(out));
        }
        let mut indices: Vec<usize> = (0..r).collect();
        out.push(emit(&indices));
        loop {
            let mut i = r;
            loop {
                if i == 0 {
                    return Ok(list_iter(out));
                }
                i -= 1;
                if indices[i] != i + n - r {
                    break;
                }
            }
            indices[i] += 1;
            for j in i + 1..r {
                indices[j] = indices[j - 1] + 1;
            }
            out.push(emit(&indices));
        }
    }
}

fn itertools_tee(args: &[Value]) -> Result<Value, String> {
    let items = host::iter_vec(&arg0(args)?)?;
    let n = args.get(1).and_then(|v| with_host(|h| h.as_int(v))).unwrap_or(2).max(0) as usize;
    let iters: Vec<Value> = (0..n).map(|_| list_iter(items.clone())).collect();
    Ok(with_host(|h| h.new_tuple(iters)))
}

fn itertools_groupby(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value, String> {
    let items = host::iter_vec(&arg0(args)?)?;
    let key = args.get(1).filter(|v| !matches!(v, Value::Undef)).cloned();
    // Materialize consecutive groups as (key, list) tuples (the group is a list,
    // which is iterable like CPython's grouper — eager, not lazily invalidated).
    let mut out: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let k = match &key {
            Some(f) => host::invoke(f, vec![items[i].clone()], vec![])?,
            None => items[i].clone(),
        };
        let mut group = vec![items[i].clone()];
        let mut j = i + 1;
        while j < items.len() {
            let kj = match &key {
                Some(f) => host::invoke(f, vec![items[j].clone()], vec![])?,
                None => items[j].clone(),
            };
            if with_host(|h| h.equal(&kj, &k)) {
                group.push(items[j].clone());
                j += 1;
            } else {
                break;
            }
        }
        let glist = with_host(|h| h.new_list(group));
        let tup = with_host(|h| h.new_tuple(vec![k, glist]));
        out.push(tup);
        i = j;
    }
    Ok(list_iter(out))
}

/// A `_random.Random` instance method, dispatched against the instance's MT
/// state (keyed by heap id `id`).
pub fn random_method(id: u32, name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "seed" => {
            let key: Vec<u32> = match args.first() {
                None | Some(Value::Undef) => {
                    let mut buf = [0u8; 32];
                    use std::io::Read;
                    let _ = std::fs::File::open("/dev/urandom")
                        .and_then(|mut f| f.read_exact(&mut buf));
                    buf.chunks(4)
                        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect()
                }
                Some(a) => {
                    let big = with_host(|h| h.big_val(a))
                        .ok_or_else(|| host::type_error("seed must be an integer"))?;
                    let (_, digits) = big.to_u32_digits();
                    if digits.is_empty() {
                        vec![0]
                    } else {
                        digits
                    }
                }
            };
            with_host(|h| h.mt_states.entry(id).or_default().init_by_array(&key));
            Ok(Value::Undef)
        }
        "random" => Ok(Value::Float(with_host(|h| {
            h.mt_states.entry(id).or_default().random()
        }))),
        "getrandbits" => {
            let k = args.first().and_then(|v| with_host(|h| h.as_int(v))).unwrap_or(0);
            if k <= 0 {
                return Err("ValueError: number of bits must be greater than zero".into());
            }
            Ok(with_host(|h| {
                let st = h.mt_states.entry(id).or_default();
                if k <= 32 {
                    return Value::Int((st.next_u32() >> (32 - k)) as i64);
                }
                let words = ((k - 1) / 32 + 1) as usize;
                let mut result = num_bigint::BigInt::from(0);
                let mut kk = k;
                for i in 0..words {
                    let mut r = st.next_u32();
                    if kk < 32 {
                        r >>= 32 - kk;
                    }
                    result |= num_bigint::BigInt::from(r) << (32 * i as u32);
                    kk -= 32;
                }
                h.norm_big(result)
            }))
        }
        "getstate" => Ok(with_host(|h| {
            let s = h.mt_states.entry(id).or_default().state();
            let vals: Vec<Value> = s.into_iter().map(|w| Value::Int(w as i64)).collect();
            h.new_tuple(vals)
        })),
        "setstate" => {
            let tup = arg0(args)?;
            let items = host::iter_vec(&tup)?;
            let s: Vec<u32> = items
                .iter()
                .map(|v| with_host(|h| h.as_int(v)).unwrap_or(0) as u32)
                .collect();
            with_host(|h| h.mt_states.entry(id).or_default().set_state(&s));
            Ok(Value::Undef)
        }
        _ => Err(format!("AttributeError: '_random.Random' object has no method '{name}'")),
    }
}

/// The `posix` syscall surface, backed by std::fs/std::env/libc.
fn call_posix(name: &str, args: Vec<Value>, kwargs: Vec<(String, Value)>) -> Result<Value, String> {
    let path_arg = |i: usize| -> Option<String> {
        args.get(i).and_then(|v| with_host(|h| h.as_str(v)))
    };
    let str_v = |s: String| with_host(|h| h.new_str(s));
    let io_err = |e: std::io::Error| -> String {
        let code = e.raw_os_error().unwrap_or(0);
        format!("OSError: [Errno {code}] {e}")
    };
    match name {
        "getcwd" => std::env::current_dir()
            .map(|p| str_v(p.to_string_lossy().into_owned()))
            .map_err(io_err),
        "getcwdb" => std::env::current_dir()
            .map(|p| {
                with_host(|h| h.alloc(PyObj::Bytes(p.to_string_lossy().as_bytes().to_vec())))
            })
            .map_err(io_err),
        "chdir" => {
            let p = path_arg(0).ok_or_else(|| host::type_error("chdir: path required"))?;
            std::env::set_current_dir(&p).map_err(io_err)?;
            Ok(Value::Undef)
        }
        "listdir" => {
            let p = path_arg(0).unwrap_or_else(|| ".".to_string());
            let mut names = Vec::new();
            for entry in std::fs::read_dir(&p).map_err(io_err)? {
                let e = entry.map_err(io_err)?;
                names.push(str_v(e.file_name().to_string_lossy().into_owned()));
            }
            Ok(with_host(|h| h.new_list(names)))
        }
        "stat" | "lstat" => {
            let p = path_arg(0).ok_or_else(|| host::type_error("stat: path required"))?;
            let md = if name == "lstat" {
                std::fs::symlink_metadata(&p)
            } else {
                std::fs::metadata(&p)
            }
            .map_err(io_err)?;
            Ok(build_stat_result(&md))
        }
        "mkdir" => {
            let p = path_arg(0).ok_or_else(|| host::type_error("mkdir: path required"))?;
            std::fs::create_dir(&p).map_err(io_err)?;
            Ok(Value::Undef)
        }
        "makedirs" => {
            let p = path_arg(0).ok_or_else(|| host::type_error("makedirs: path required"))?;
            std::fs::create_dir_all(&p).map_err(io_err)?;
            Ok(Value::Undef)
        }
        "rmdir" => {
            let p = path_arg(0).ok_or_else(|| host::type_error("rmdir: path required"))?;
            std::fs::remove_dir(&p).map_err(io_err)?;
            Ok(Value::Undef)
        }
        "remove" | "unlink" => {
            let p = path_arg(0).ok_or_else(|| host::type_error("remove: path required"))?;
            std::fs::remove_file(&p).map_err(io_err)?;
            Ok(Value::Undef)
        }
        "rename" | "replace" => {
            let src = path_arg(0).ok_or_else(|| host::type_error("rename: src required"))?;
            let dst = path_arg(1).ok_or_else(|| host::type_error("rename: dst required"))?;
            std::fs::rename(&src, &dst).map_err(io_err)?;
            Ok(Value::Undef)
        }
        "getpid" => Ok(Value::Int(std::process::id() as i64)),
        "getppid" => Ok(Value::Int(unsafe { libc::getppid() } as i64)),
        "getuid" | "geteuid" => Ok(Value::Int(unsafe { libc::getuid() } as i64)),
        "getgid" | "getegid" => Ok(Value::Int(unsafe { libc::getgid() } as i64)),
        "getpgrp" => Ok(Value::Int(unsafe { libc::getpgrp() } as i64)),
        "umask" => {
            let m = args.first().and_then(|v| with_host(|h| h.as_int(v))).unwrap_or(0);
            let prev = unsafe { libc::umask(m as libc::mode_t) };
            Ok(Value::Int(prev as i64))
        }
        "urandom" => {
            let n = args.first().and_then(|v| with_host(|h| h.as_int(v))).unwrap_or(0).max(0) as usize;
            let mut buf = vec![0u8; n];
            use std::io::Read;
            std::fs::File::open("/dev/urandom")
                .and_then(|mut f| f.read_exact(&mut buf))
                .map_err(io_err)?;
            Ok(with_host(|h| h.alloc(PyObj::Bytes(buf))))
        }
        "getenv" => {
            let key = path_arg(0).ok_or_else(|| host::type_error("getenv: key required"))?;
            match std::env::var(&key) {
                Ok(val) => Ok(str_v(val)),
                Err(_) => Ok(args.get(1).cloned().unwrap_or(Value::Undef)),
            }
        }
        "putenv" => {
            let key = path_arg(0).ok_or_else(|| host::type_error("putenv: key required"))?;
            let val = path_arg(1).unwrap_or_default();
            std::env::set_var(&key, &val);
            Ok(Value::Undef)
        }
        "unsetenv" => {
            let key = path_arg(0).ok_or_else(|| host::type_error("unsetenv: key required"))?;
            std::env::remove_var(&key);
            Ok(Value::Undef)
        }
        "access" => {
            let p = path_arg(0).ok_or_else(|| host::type_error("access: path required"))?;
            let mode = args.get(1).and_then(|v| with_host(|h| h.as_int(v))).unwrap_or(0);
            let cp = std::ffi::CString::new(p).map_err(|_| "ValueError: embedded null".to_string())?;
            Ok(Value::Bool(unsafe { libc::access(cp.as_ptr(), mode as i32) } == 0))
        }
        "fspath" => {
            // Return str/bytes unchanged; call __fspath__ on a path-like object.
            let a = arg0(&args)?;
            if with_host(|h| matches!(h.get(&a), Some(PyObj::Str(_)) | Some(PyObj::Bytes(_))))
                || matches!(a, Value::Str(_))
            {
                Ok(a)
            } else {
                host::call_method(&a, "__fspath__", vec![], vec![])
            }
        }
        "strerror" => {
            let code = args.first().and_then(|v| with_host(|h| h.as_int(v))).unwrap_or(0);
            let msg = std::io::Error::from_raw_os_error(code as i32).to_string();
            Ok(str_v(msg))
        }
        "system" => {
            let cmd = path_arg(0).ok_or_else(|| host::type_error("system: command required"))?;
            let status = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&cmd)
                .status()
                .map_err(io_err)?;
            Ok(Value::Int((status.code().unwrap_or(0) as i64) << 8))
        }
        "cpu_count" => Ok(std::thread::available_parallelism()
            .map(|n| Value::Int(n.get() as i64))
            .unwrap_or(Value::Undef)),
        "isatty" => {
            let fd = args.first().and_then(|v| with_host(|h| h.as_int(v))).unwrap_or(-1);
            Ok(Value::Bool(unsafe { libc::isatty(fd as i32) } == 1))
        }
        "_exit" => {
            let code = args.first().and_then(|v| with_host(|h| h.as_int(v))).unwrap_or(0);
            std::process::exit(code as i32);
        }
        "abort" => std::process::exit(134),
        "sync" | "sched_yield" => Ok(Value::Undef),
        "_create_environ" => Ok(with_host(|h| {
            let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
            for (k, val) in std::env::vars() {
                let kb = k.into_bytes();
                let kv = h.alloc(PyObj::Bytes(kb.clone()));
                let vv = h.alloc(PyObj::Bytes(val.into_bytes()));
                d.insert(PKey::Bytes(kb), (kv, vv));
            }
            h.alloc(PyObj::Dict(d))
        })),
        // File-descriptor level ops.
        "close" => {
            let fd = args.first().and_then(|v| with_host(|h| h.as_int(v))).unwrap_or(-1);
            unsafe { libc::close(fd as i32) };
            Ok(Value::Undef)
        }
        "get_inheritable" => Ok(Value::Bool(false)),
        "set_inheritable" | "fsync" | "utime" | "chmod" | "truncate" | "ftruncate" => {
            Ok(Value::Undef)
        }
        "readlink" => {
            let p = path_arg(0).ok_or_else(|| host::type_error("readlink: path required"))?;
            std::fs::read_link(&p)
                .map(|t| str_v(t.to_string_lossy().into_owned()))
                .map_err(io_err)
        }
        _ => Err(format!("AttributeError: module 'posix' has no attribute '{name}'")),
    }
    .map_err(|e| {
        let _ = &kwargs;
        e
    })
}

/// A `posix.stat` result — the 10-field `stat_result` sequence (st_mode, st_ino,
/// st_dev, st_nlink, st_uid, st_gid, st_size, st_atime, st_mtime, st_ctime),
/// tagged so `.st_mode` etc. resolve.
fn build_stat_result(md: &std::fs::Metadata) -> Value {
    use std::os::unix::fs::MetadataExt;
    let fields = [
        ("st_mode", md.mode() as i64),
        ("st_ino", md.ino() as i64),
        ("st_dev", md.dev() as i64),
        ("st_nlink", md.nlink() as i64),
        ("st_uid", md.uid() as i64),
        ("st_gid", md.gid() as i64),
        ("st_size", md.size() as i64),
        ("st_atime", md.atime()),
        ("st_mtime", md.mtime()),
        ("st_ctime", md.ctime()),
    ];
    with_host(|h| {
        let vals: Vec<Value> = fields.iter().map(|(_, v)| Value::Int(*v)).collect();
        let tup = h.new_tuple(vals);
        if let Value::Obj(i) = tup {
            h.nt_meta.insert(
                i,
                host::NtMeta {
                    type_name: "os.stat_result".to_string(),
                    fields: fields.iter().map(|(k, _)| (*k).to_string()).collect(),
                },
            );
        }
        tup
    })
}

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
        "getfilesystemencoding" | "getdefaultencoding" => {
            Ok(with_host(|h| h.new_str("utf-8".to_string())))
        }
        "getfilesystemencodeerrors" => Ok(with_host(|h| h.new_str("surrogateescape".to_string()))),
        // `sys.intern(s)` returns the string (no interning table needed here).
        "intern" => Ok(args.into_iter().next().unwrap_or(Value::Undef)),
        "audit" => Ok(Value::Undef),
        "is_finalizing" => Ok(Value::Bool(false)),
        // `sys._getframe([depth])` — the frame `depth` levels up from the caller.
        "_getframe" => {
            let depth = args
                .first()
                .and_then(|v| with_host(|h| h.as_int(v)))
                .unwrap_or(0)
                .max(0) as usize;
            Ok(with_host(|h| h.current_frame_object(depth)))
        }
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
            // `timeout` is positional-or-keyword; `timeout=None` means "no timeout".
            let timeout = it
                .next()
                .or_else(|| kw_get(&kwargs, "timeout"))
                .and_then(num_f);
            async_rt::wait_for(aw, timeout)
        }
        "get_event_loop" | "get_running_loop" | "new_event_loop" => Ok(async_rt::event_loop()),
        "Future" => Ok(async_rt::new_future()),
        // `wait`/`as_completed` take a single iterable of awaitables (not varargs).
        "wait" => {
            let mut it = args.into_iter();
            let aws = host::iter_vec(&it.next().unwrap_or(Value::Undef))?;
            let timeout = it
                .next()
                .or_else(|| kw_get(&kwargs, "timeout"))
                .and_then(num_f);
            let return_when = kw_get(&kwargs, "return_when")
                .map(|v| async_rt::ReturnWhen::parse(&with_host(|h| h.str_of(&v))))
                .unwrap_or(async_rt::ReturnWhen::AllCompleted);
            async_rt::wait(aws, timeout, return_when)
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

/// `functools.total_ordering(cls)` — native so `cls` stays a native pythonrs
/// class (a CPython round trip returns a Foreign class whose native `__init__`
/// can't set attributes). Marks the class; comparison dispatch derives the
/// missing rich-comparison ops (see `total_ordering_derive`). Requires `__eq__`
/// and at least one of `__lt__`/`__le__`/`__gt__`/`__ge__`, matching CPython.
fn call_total_ordering(args: &[Value]) -> Result<Value, String> {
    let cls = arg0(args)?;
    let name = match with_host(|h| h.get(&cls).cloned()) {
        Some(PyObj::Class(n)) => n,
        _ => return Err(host::type_error("total_ordering: argument must be a class")),
    };
    let roots = ["__lt__", "__le__", "__gt__", "__ge__"];
    let has_root = with_host(|h| roots.iter().any(|m| h.class_lookup(&name, m).is_some()));
    if !has_root {
        return Err(host::type_error(
            "must define at least one ordering operation: < > <= >=",
        ));
    }
    with_host(|h| h.mark_total_ordering(&name));
    Ok(cls)
}

/// Derive `self <op> other` for a `@total_ordering` class that lacks the direct
/// `op` dunder, from the single ordering dunder it does define plus `__eq__`
/// (CPython's `functools._convert` formulas). `None` if `self` is not an instance
/// of a marked class or `op` is the defined root; `Some(NotImplemented)` if the
/// root dunder declines.
fn total_ordering_derive(op: NumOp, selfv: &Value, other: &Value) -> Option<Result<Value, String>> {
    use NumOp::*;
    let class = with_host(|h| match h.get(selfv) {
        Some(PyObj::Instance(i)) if h.is_total_ordering(&i.class) => Some(i.class.clone()),
        _ => None,
    })?;
    let target = match op {
        Lt => "__lt__",
        Le => "__le__",
        Gt => "__gt__",
        Ge => "__ge__",
        _ => return None,
    };
    // The single root ordering dunder the user defined (first found wins; a class
    // with several defined has each resolved directly, so this path is unused).
    let root = ["__lt__", "__le__", "__gt__", "__ge__"]
        .into_iter()
        .find(|m| with_host(|h| h.class_lookup(&class, m).is_some()))?;
    if target == root {
        return None; // resolved directly upstream — nothing to derive
    }
    // r = truthiness of `self.<root>(other)`; a declined root propagates.
    let rv = match host::call_method(selfv, root, vec![other.clone()], vec![]) {
        Ok(v) => v,
        Err(e) => return Some(Err(e)),
    };
    if is_not_implemented(&rv) {
        return Some(Ok(rv));
    }
    let r = with_host(|h| h.truthy(&rv));
    // Only some formulas reference `self == other`; compute it lazily.
    let needs_eq = matches!(
        (root, target),
        ("__lt__", "__gt__")
            | ("__lt__", "__le__")
            | ("__le__", "__ge__")
            | ("__le__", "__lt__")
            | ("__gt__", "__lt__")
            | ("__gt__", "__ge__")
            | ("__ge__", "__le__")
            | ("__ge__", "__gt__")
    );
    let eq = if needs_eq {
        match dispatch_binop(selfv, other, "__eq__", "__eq__") {
            Dunder::Value(v) => with_host(|h| h.truthy(&v)),
            Dunder::Err(e) => return Some(Err(e)),
            Dunder::NotImplemented => identity_eq(selfv, other),
        }
    } else {
        false
    };
    let result = match (root, target) {
        ("__lt__", "__gt__") => !r && !eq,
        ("__lt__", "__le__") => r || eq,
        ("__lt__", "__ge__") => !r,
        ("__le__", "__ge__") => !r || eq,
        ("__le__", "__lt__") => r && !eq,
        ("__le__", "__gt__") => !r,
        ("__gt__", "__lt__") => !r && !eq,
        ("__gt__", "__ge__") => r || eq,
        ("__gt__", "__le__") => !r,
        ("__ge__", "__le__") => !r || eq,
        ("__ge__", "__gt__") => r && !eq,
        ("__ge__", "__lt__") => !r,
        _ => return None,
    };
    Some(Ok(Value::Bool(result)))
}

/// `copy.copy` / `copy.deepcopy` — implemented natively because routing them
/// through the CPython `copy` module marshals the object by value (a deep copy),
/// which loses `copy.copy`'s shared references and can't reconstruct a pythonrs
/// instance from its CPython proxy.
pub fn call_copy(name: &str, args: &[Value]) -> Result<Value, String> {
    let v = arg0(args)?;
    match name {
        "copy" => copy_shallow(&v),
        "deepcopy" => {
            let mut memo = std::collections::HashMap::new();
            copy_deep(&v, &mut memo)
        }
        _ => Err(host::type_error(&format!(
            "module 'copy' has no attribute '{name}'"
        ))),
    }
}

/// A shallow copy: a fresh mutable container/instance sharing the element or
/// attribute references; an immutable value is returned unchanged.
fn copy_shallow(v: &Value) -> Result<Value, String> {
    enum K {
        List(Vec<Value>),
        Dict(IndexMap<PKey, (Value, Value)>, Option<host::DictMeta>),
        Set(IndexMap<PKey, Value>),
        Bytearray(Vec<u8>),
        Instance(Instance),
        Same,
    }
    let k = with_host(|h| match h.get(v) {
        Some(PyObj::List(l)) => K::List(l.clone()),
        Some(PyObj::Dict(d)) => {
            let meta = match v {
                Value::Obj(i) => h.dict_meta.get(i).cloned(),
                _ => None,
            };
            K::Dict(d.clone(), meta)
        }
        Some(PyObj::Set(s)) => K::Set(s.clone()),
        Some(PyObj::Bytearray(b)) => K::Bytearray(b.clone()),
        Some(PyObj::Instance(inst)) => K::Instance(inst.clone()),
        _ => K::Same,
    });
    Ok(with_host(|h| match k {
        K::List(l) => h.new_list(l),
        K::Dict(d, meta) => {
            let nd = h.new_dict(d);
            if let (Some(meta), Value::Obj(i)) = (meta, &nd) {
                h.dict_meta.insert(*i, meta);
            }
            nd
        }
        K::Set(s) => h.new_set(s),
        K::Bytearray(b) => h.alloc(PyObj::Bytearray(b)),
        K::Instance(inst) => {
            let pairs = match h.get(&inst.dict) {
                Some(PyObj::Dict(d)) => d.clone(),
                _ => IndexMap::new(),
            };
            let dict = h.alloc(PyObj::Dict(pairs));
            h.alloc(PyObj::Instance(Instance {
                class: inst.class,
                dict,
                payload: inst.payload,
            }))
        }
        K::Same => v.clone(),
    }))
}

/// A deep, recursive copy. New containers are registered in `memo` (keyed by heap
/// id) before their children are copied, so shared and cyclic references are
/// preserved exactly as CPython's `deepcopy` does.
fn copy_deep(v: &Value, memo: &mut std::collections::HashMap<u32, Value>) -> Result<Value, String> {
    if let Value::Obj(i) = v {
        if let Some(c) = memo.get(i) {
            return Ok(c.clone());
        }
    }
    enum K {
        List(Vec<Value>),
        Tuple(Vec<Value>),
        Dict(Vec<(Value, Value)>, Option<host::DictMeta>),
        Set(Vec<Value>),
        Bytearray(Vec<u8>),
        Instance(String, Vec<(Value, Value)>, Value),
        Same,
    }
    let k = with_host(|h| match h.get(v) {
        Some(PyObj::List(l)) => K::List(l.clone()),
        Some(PyObj::Tuple(l)) => K::Tuple(l.clone()),
        Some(PyObj::Dict(d)) => {
            let meta = match v {
                Value::Obj(i) => h.dict_meta.get(i).cloned(),
                _ => None,
            };
            K::Dict(d.values().cloned().collect(), meta)
        }
        Some(PyObj::Set(s)) => K::Set(s.values().cloned().collect()),
        Some(PyObj::Bytearray(b)) => K::Bytearray(b.clone()),
        Some(PyObj::Instance(inst)) => {
            let pairs = match h.get(&inst.dict) {
                Some(PyObj::Dict(d)) => d.values().cloned().collect(),
                _ => Vec::new(),
            };
            K::Instance(inst.class.clone(), pairs, inst.payload.clone())
        }
        _ => K::Same,
    });
    let id = if let Value::Obj(i) = v {
        Some(*i)
    } else {
        None
    };
    match k {
        K::List(items) => {
            let out = with_host(|h| h.new_list(Vec::new()));
            if let Some(i) = id {
                memo.insert(i, out.clone());
            }
            let mut copied = Vec::with_capacity(items.len());
            for it in &items {
                copied.push(copy_deep(it, memo)?);
            }
            with_host(|h| {
                if let Some(PyObj::List(l)) = h.get_mut(&out) {
                    *l = copied;
                }
            });
            Ok(out)
        }
        K::Tuple(items) => {
            let mut copied = Vec::with_capacity(items.len());
            for it in &items {
                copied.push(copy_deep(it, memo)?);
            }
            let out = with_host(|h| h.new_tuple(copied));
            if let Some(i) = id {
                memo.insert(i, out.clone());
            }
            Ok(out)
        }
        K::Dict(pairs, meta) => {
            let out = with_host(|h| h.new_dict(IndexMap::new()));
            if let Some(i) = id {
                memo.insert(i, out.clone());
            }
            let mut nd: IndexMap<PKey, (Value, Value)> = IndexMap::new();
            for (k, val) in &pairs {
                let ck = copy_deep(k, memo)?;
                let cv = copy_deep(val, memo)?;
                let key = with_host(|h| h.to_key(&ck))?;
                nd.insert(key, (ck, cv));
            }
            with_host(|h| {
                if let Some(PyObj::Dict(d)) = h.get_mut(&out) {
                    *d = nd;
                }
                if let (Some(meta), Value::Obj(i)) = (meta, &out) {
                    h.dict_meta.insert(*i, meta);
                }
            });
            Ok(out)
        }
        K::Set(items) => {
            let mut copied: IndexMap<PKey, Value> = IndexMap::new();
            for it in &items {
                let ci = copy_deep(it, memo)?;
                let key = with_host(|h| h.to_key(&ci))?;
                copied.insert(key, ci);
            }
            let out = with_host(|h| h.new_set(copied));
            if let Some(i) = id {
                memo.insert(i, out.clone());
            }
            Ok(out)
        }
        K::Bytearray(b) => {
            let out = with_host(|h| h.alloc(PyObj::Bytearray(b)));
            if let Some(i) = id {
                memo.insert(i, out.clone());
            }
            Ok(out)
        }
        K::Instance(class, pairs, payload) => {
            let dict = with_host(|h| h.alloc(PyObj::Dict(IndexMap::new())));
            let out = with_host(|h| {
                h.alloc(PyObj::Instance(Instance {
                    class,
                    dict: dict.clone(),
                    payload,
                }))
            });
            if let Some(i) = id {
                memo.insert(i, out.clone());
            }
            let mut nd: IndexMap<PKey, (Value, Value)> = IndexMap::new();
            for (k, val) in &pairs {
                let cv = copy_deep(val, memo)?;
                let key = with_host(|h| h.to_key(k))?;
                nd.insert(key, (k.clone(), cv));
            }
            with_host(|h| {
                if let Some(PyObj::Dict(d)) = h.get_mut(&dict) {
                    *d = nd;
                }
            });
            Ok(out)
        }
        K::Same => Ok(v.clone()),
    }
}

/// Convert an integral `f64` to a pythonrs int: a plain `Value::Int` when it fits
/// `i64`, else an exact `BigInt` (so `math.floor(1e20)` is `10**20`, not the
/// i64-saturated `as i64` cast). Non-finite maps to `0` (callers guard finiteness).
fn f64_to_int(h: &mut host::PyHost, x: f64) -> Value {
    if x >= i64::MIN as f64 && x <= i64::MAX as f64 {
        return Value::Int(x as i64);
    }
    use num_traits::FromPrimitive;
    match num_bigint::BigInt::from_f64(x) {
        Some(b) => h.norm_big(b),
        None => Value::Int(0),
    }
}

/// A builtin method/function's argument-count contract, mirroring the distinct
/// CPython error messages each C calling convention produces.
enum Arity {
    /// `METH_O` — exactly one positional. `qual` is the qualified name shown as
    /// `Type.method()`: "…() takes exactly one argument (N given)".
    ExactlyOne,
    /// `METH_NOARGS`: "Type.method() takes no arguments (N given)".
    NoArgs,
    /// `METH_VARARGS` with a fixed count: "method expected K arguments, got N"
    /// (unqualified; "argument" when K == 1).
    VarExact(usize),
    /// `METH_VARARGS` with optional trailing args: "method expected at most MAX
    /// argument(s), got N", or "…at least MIN…" when too few.
    VarRange(usize, usize),
    /// Keyword-only (e.g. `list.sort`): "method() takes no positional arguments".
    NoPositional,
}

/// Validate `argc` positional args against a method's [`Arity`], returning
/// CPython's exact `TypeError` string on mismatch. `qual` is the `Type.method`
/// qualified name; `name` is the bare method name (used by the VARARGS forms).
fn check_arity(name: &str, qual: &str, spec: Arity, argc: usize) -> Result<(), String> {
    let plural = |k: usize| if k == 1 { "argument" } else { "arguments" };
    match spec {
        Arity::ExactlyOne if argc != 1 => Err(host::type_error(&format!(
            "{qual}() takes exactly one argument ({argc} given)"
        ))),
        Arity::NoArgs if argc != 0 => Err(host::type_error(&format!(
            "{qual}() takes no arguments ({argc} given)"
        ))),
        Arity::VarExact(k) if argc != k => Err(host::type_error(&format!(
            "{name} expected {k} {}, got {argc}",
            plural(k)
        ))),
        Arity::VarRange(_, max) if argc > max => Err(host::type_error(&format!(
            "{name} expected at most {max} {}, got {argc}",
            plural(max)
        ))),
        Arity::VarRange(min, _) if argc < min => Err(host::type_error(&format!(
            "{name} expected at least {min} {}, got {argc}",
            plural(min)
        ))),
        Arity::NoPositional if argc != 0 => {
            Err(host::type_error(&format!("{name}() takes no positional arguments")))
        }
        _ => Ok(()),
    }
}

/// The argument-count contract for a `math` module function, or `None` for the
/// variadic ones (`gcd`, `lcm`) that accept any count.
fn math_arity(name: &str) -> Option<Arity> {
    Some(match name {
        "log" => Arity::VarRange(1, 2),
        "pow" | "atan2" | "copysign" | "fmod" | "remainder" | "ldexp" => Arity::VarExact(2),
        "gcd" | "lcm" | "hypot" => return None,
        // Every other implemented math function is single-argument (METH_O).
        _ => Arity::ExactlyOne,
    })
}

fn call_math(name: &str, args: &[Value]) -> Result<Value, String> {
    if let Some(spec) = math_arity(name) {
        check_arity(name, &format!("math.{name}"), spec, args.len())?;
    }
    let f0 = args.first().and_then(as_f).unwrap_or(0.0);
    match name {
        "sqrt" => Ok(Value::Float(f0.sqrt())),
        "floor" => Ok(with_host(|h| f64_to_int(h, f0.floor()))),
        "ceil" => Ok(with_host(|h| f64_to_int(h, f0.ceil()))),
        "fabs" => Ok(Value::Float(f0.abs())),
        "sin" => Ok(Value::Float(f0.sin())),
        "cos" => Ok(Value::Float(f0.cos())),
        "tan" => Ok(Value::Float(f0.tan())),
        "asin" => Ok(Value::Float(f0.asin())),
        "acos" => Ok(Value::Float(f0.acos())),
        "atan" => Ok(Value::Float(f0.atan())),
        "sinh" => Ok(Value::Float(f0.sinh())),
        "cosh" => Ok(Value::Float(f0.cosh())),
        "tanh" => Ok(Value::Float(f0.tanh())),
        "asinh" => Ok(Value::Float(f0.asinh())),
        "acosh" => Ok(Value::Float(f0.acosh())),
        "atanh" => Ok(Value::Float(f0.atanh())),
        "exp" => Ok(Value::Float(f0.exp())),
        "exp2" => Ok(Value::Float(f0.exp2())),
        "expm1" => Ok(Value::Float(f0.exp_m1())),
        "log2" => Ok(Value::Float(f0.log2())),
        "log10" => Ok(Value::Float(f0.log10())),
        "log1p" => Ok(Value::Float(f0.ln_1p())),
        // Special functions Rust std lacks — pure-Rust libm keeps `math`
        // self-contained (no C libm) on the no-libpython build.
        "lgamma" => Ok(Value::Float(libm::lgamma(f0))),
        "gamma" => Ok(Value::Float(libm::tgamma(f0))),
        "erf" => Ok(Value::Float(libm::erf(f0))),
        "erfc" => Ok(Value::Float(libm::erfc(f0))),
        "cbrt" => Ok(Value::Float(f0.cbrt())),
        "degrees" => Ok(Value::Float(f0.to_degrees())),
        "radians" => Ok(Value::Float(f0.to_radians())),
        "trunc" => Ok(with_host(|h| f64_to_int(h, f0.trunc()))),
        "isnan" => Ok(Value::Bool(f0.is_nan())),
        "isinf" => Ok(Value::Bool(f0.is_infinite())),
        "isfinite" => Ok(Value::Bool(f0.is_finite())),
        "atan2" => {
            let f1 = args.get(1).and_then(as_f).unwrap_or(0.0);
            Ok(Value::Float(f0.atan2(f1)))
        }
        "hypot" => {
            // CPython 3.8+ `hypot(*coords)` — Euclidean norm of any number of args.
            let sumsq: f64 = args.iter().filter_map(as_f).map(|c| c * c).sum();
            Ok(Value::Float(sumsq.sqrt()))
        }
        "copysign" => {
            let f1 = args.get(1).and_then(as_f).unwrap_or(0.0);
            Ok(Value::Float(f0.copysign(f1)))
        }
        "fmod" => {
            let f1 = args.get(1).and_then(as_f).unwrap_or(0.0);
            // C `fmod`: result has the sign of the dividend (unlike Python `%`).
            Ok(Value::Float(f0 % f1))
        }
        "ldexp" => {
            let f1 = args.get(1).and_then(as_f).unwrap_or(0.0);
            Ok(Value::Float(f0 * 2f64.powi(f1 as i32)))
        }
        "isqrt" => {
            // Integer square root: floor(sqrt(n)) for a non-negative int, bignum-safe.
            use num_integer::Roots;
            let n = with_host(|h| h.big_val(&args[0])).ok_or_else(|| {
                host::type_error("'float' object cannot be interpreted as an integer")
            })?;
            if n.sign() == num_bigint::Sign::Minus {
                return Err("ValueError: isqrt() argument must be nonnegative".into());
            }
            Ok(with_host(|h| h.norm_big(n.sqrt())))
        }
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
            // `math.gcd(*integers)` — arbitrary count (CPython 3.9+) and
            // bignum-safe (`as_int` would silently truncate a value beyond i64 to
            // 0, giving the wrong gcd).
            use num_integer::Integer;
            let mut acc = num_bigint::BigInt::from(0);
            for a in args {
                let n = with_host(|h| h.big_val(a)).ok_or_else(|| {
                    host::type_error("'float' object cannot be interpreted as an integer")
                })?;
                acc = acc.gcd(&n);
            }
            Ok(with_host(|h| h.norm_big(acc)))
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
    ("UnicodeEncodeError", "UnicodeError"),
    ("UnicodeTranslateError", "UnicodeError"),
    ("ConnectionError", "OSError"),
    ("BrokenPipeError", "ConnectionError"),
    ("TimeoutError", "OSError"),
    // asyncio exceptions: CancelledError is a BaseException (never swallowed by
    // `except Exception`); InvalidStateError guards double set_result.
    ("CancelledError", "BaseException"),
    ("InvalidStateError", "Exception"),
    ("QueueEmpty", "Exception"),
    ("QueueFull", "Exception"),
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
    // A CPython exception raised over the bridge (e.g. `json.JSONDecodeError`, a
    // `ValueError` subclass): its base-class chain was captured at raise time, so
    // `except ValueError` matches by consulting the recorded `__mro__` names.
    if let Some(bases) = h.foreign_exc_bases.get(exc_class) {
        if bases.iter().any(|b| b == want) {
            return true;
        }
    }
    // A CPython stdlib exception unknown to pythonrs's builtin table (and not a
    // user class) with no recorded base chain: treat it as an `Exception`
    // subclass so the common catch-all `except Exception` still matches. Nearly
    // all exceptions derive from `Exception`, and the non-`Exception`
    // `BaseException` subclasses (`KeyboardInterrupt`, `SystemExit`,
    // `GeneratorExit`) are all builtins in the table.
    if want == "Exception" && !is_exception_class(exc_class) && !h.classes.contains_key(exc_class) {
        return true;
    }
    false
}

/// `isinstance(v, cls)` honoring a metaclass `__instancecheck__` override and a
/// tuple of classes (checked left to right, each with its own override). Falls
/// back to the structural `isinstance` when no override applies.
fn isinstance_dispatch(v: &Value, cls: &Value) -> Result<bool, String> {
    if let Some(PyObj::Tuple(ts)) = with_host(|h| h.get(cls).cloned()) {
        for t in ts {
            if isinstance_dispatch(v, &t)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    // A PEP 604 union (`isinstance(x, int | str)`) tests each member; a bare
    // `None` member matches only `None` itself (as `NoneType`).
    if let Some(PyObj::Union { args }) = with_host(|h| h.get(cls).cloned()) {
        for m in args {
            if matches!(m, Value::Undef) {
                if matches!(v, Value::Undef) {
                    return Ok(true);
                }
            } else if isinstance_dispatch(v, &m)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if let Some(r) = host::metaclass_hook(cls, "__instancecheck__", v.clone()) {
        let res = r?;
        return Ok(with_host(|h| h.truthy(&res)));
    }
    // A CPython class/ABC (`collections.abc.Sequence`, an `enum`/`typing` type):
    // let CPython's `isinstance` decide (a native list crosses as a real `list`,
    // so structural ABC checks succeed).
    #[cfg(feature = "stdlib-ffi")]
    if let Some(cls_id) = with_host(|h| h.foreign_id(cls)) {
        return with_host(|h| crate::ffi::isinstance_foreign(h, v, cls_id));
    }
    Ok(with_host(|h| isinstance(h, v, cls)))
}

/// `issubclass(sub, cls)` honoring a metaclass `__subclasscheck__` override and a
/// tuple of classes. Raises `TypeError` when `sub` is not a class, matching
/// CPython. Falls back to the structural name-based check.
fn issubclass_dispatch(sub: &Value, cls: &Value) -> Result<bool, String> {
    if let Some(PyObj::Tuple(ts)) = with_host(|h| h.get(cls).cloned()) {
        for t in ts {
            if issubclass_dispatch(sub, &t)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if let Some(r) = host::metaclass_hook(cls, "__subclasscheck__", sub.clone()) {
        let res = r?;
        return Ok(with_host(|h| h.truthy(&res)));
    }
    let a = match with_host(|h| callable_name(h, sub)) {
        Some(n) => n,
        None => return Err(host::type_error("issubclass() arg 1 must be a class")),
    };
    let b = with_host(|h| callable_name(h, cls)).unwrap_or_default();
    Ok(with_host(|h| type_isa(h, &a, &b)))
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
            Some(PyObj::Class(_)) | Some(PyObj::NamedTupleType { .. }) => return true,
            // Any type object: a known builtin type/exception, or a type-name
            // builtin produced by `type(x)` (coroutine/generator/iterator/…). A
            // dotted name is an unbound method (`str.upper`) and a BUILTIN_FUNCS
            // name is a function (`len`) -- neither is a type.
            Some(PyObj::Builtin(n)) if is_type_object_name(n) => return true,
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
    // `__contains__` is a real bound method on every builtin container in CPython
    // (`frozenset(kwlist).__contains__` is exactly how the stdlib `keyword.py`
    // builds `iskeyword`). Recognize it explicitly — it is kept OUT of the
    // per-type method lists below (which feed `dir()`), and `call_type_method`
    // routes the call through the membership check.
    if name == "__contains__"
        && matches!(
            typename,
            "str" | "bytes"
                | "bytearray"
                | "list"
                | "tuple"
                | "dict"
                | "set"
                | "frozenset"
                | "range"
                | "deque"
                | "dict_keys"
                | "dict_items"
                | "dict_values"
        )
    {
        return true;
    }
    let list: &[&str] = match typename {
        "str" => STR_METHODS,
        "bytes" => BYTES_METHODS,
        "bytearray" => BYTEARRAY_METHODS,
        "memoryview" => MEMORYVIEW_METHODS,
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
        "dict_keys" | "dict_items" => return name == "isdisjoint",
        "set" | "frozenset" => SET_METHODS,
        "tuple" => TUPLE_METHODS,
        "range" => &["index", "count"],
        "slice" => &["indices"],
        "deque" => DEQUE_METHODS,
        "TextIOWrapper" => FILE_METHODS,
        "int" | "bool" => return INT_METHODS.contains(&name) || INT_DUNDERS.contains(&name),
        "float" => return FLOAT_METHODS.contains(&name) || FLOAT_DUNDERS.contains(&name),
        "complex" => COMPLEX_METHODS,
        "property" => PROPERTY_METHODS,
        "generator" => GENERATOR_METHODS,
        "coroutine" => return GENERATOR_METHODS.contains(&name) || name == "__await__",
        "async_generator" => {
            return matches!(
                name,
                "__aiter__" | "__anext__" | "asend" | "athrow" | "aclose"
            )
        }
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
    "swapcase",
    "title",
    "capitalize",
    "zfill",
    "expandtabs",
    "center",
    "ljust",
    "rjust",
    "translate",
    "maketrans",
    "isalpha",
    "isdigit",
    "isalnum",
    "isspace",
    "isupper",
    "islower",
    "istitle",
    "isascii",
];
const BYTEARRAY_METHODS: &[&str] = &[
    "append",
    "extend",
    "insert",
    "reverse",
    "remove",
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
    "swapcase",
    "title",
    "capitalize",
    "zfill",
    "expandtabs",
    "center",
    "ljust",
    "rjust",
    "translate",
    "maketrans",
    "isalpha",
    "isdigit",
    "isalnum",
    "isspace",
    "isupper",
    "islower",
    "istitle",
    "isascii",
];
const MEMORYVIEW_METHODS: &[&str] = &["tobytes", "hex", "tolist", "release"];
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
const INT_METHODS: &[&str] = &[
    "bit_length",
    "bit_count",
    "to_bytes",
    "as_integer_ratio",
    "is_integer",
    "conjugate",
];
const FLOAT_METHODS: &[&str] = &["is_integer", "as_integer_ratio", "hex", "conjugate"];
const COMPLEX_METHODS: &[&str] = &["conjugate", "__abs__"];

/// The numeric dunder methods `int`/`bool` expose as bound methods
/// (`(5).__add__(2)`, `(-3).__abs__()`, `(7).__floordiv__(2)`). Includes the
/// integer-only bitwise / shift / `__index__` / `__invert__` surface.
const INT_DUNDERS: &[&str] = &[
    "__add__",
    "__radd__",
    "__sub__",
    "__rsub__",
    "__mul__",
    "__rmul__",
    "__truediv__",
    "__rtruediv__",
    "__floordiv__",
    "__rfloordiv__",
    "__mod__",
    "__rmod__",
    "__pow__",
    "__rpow__",
    "__divmod__",
    "__rdivmod__",
    "__and__",
    "__rand__",
    "__or__",
    "__ror__",
    "__xor__",
    "__rxor__",
    "__lshift__",
    "__rlshift__",
    "__rshift__",
    "__rrshift__",
    "__neg__",
    "__pos__",
    "__abs__",
    "__invert__",
    "__eq__",
    "__ne__",
    "__lt__",
    "__le__",
    "__gt__",
    "__ge__",
    "__index__",
    "__int__",
    "__float__",
    "__trunc__",
    "__floor__",
    "__ceil__",
    "__round__",
    "__bool__",
    "__repr__",
    "__str__",
    "__hash__",
];
/// The numeric dunder methods `float` exposes (no bitwise / shift / `__index__`
/// / `__invert__` — `float` has none of those in CPython).
const FLOAT_DUNDERS: &[&str] = &[
    "__add__",
    "__radd__",
    "__sub__",
    "__rsub__",
    "__mul__",
    "__rmul__",
    "__truediv__",
    "__rtruediv__",
    "__floordiv__",
    "__rfloordiv__",
    "__mod__",
    "__rmod__",
    "__pow__",
    "__rpow__",
    "__divmod__",
    "__rdivmod__",
    "__neg__",
    "__pos__",
    "__abs__",
    "__eq__",
    "__ne__",
    "__lt__",
    "__le__",
    "__gt__",
    "__ge__",
    "__int__",
    "__float__",
    "__trunc__",
    "__floor__",
    "__ceil__",
    "__round__",
    "__bool__",
    "__repr__",
    "__str__",
    "__hash__",
];

/// Is `name` a numeric dunder pythonrs exposes for base type `tn`?
fn is_num_dunder(tn: &str, name: &str) -> bool {
    match tn {
        "float" => FLOAT_DUNDERS.contains(&name),
        _ => INT_DUNDERS.contains(&name), // int, bool
    }
}

/// Dispatch a method call on a builtin-typed receiver.
/// namedtuple instance methods (`_asdict`, `_replace`, plus inherited
/// `count`/`index`). A namedtuple's `type_name` is its own name (not "tuple"),
/// so these are dispatched before the by-type-name method match below. Returns
/// `None` when `recv` isn't a namedtuple, so a plain tuple falls through.
fn nt_instance_method(
    recv: &Value,
    name: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Option<Result<Value, String>> {
    let (items, fields, type_name) = with_host(|h| match (h.get(recv), recv) {
        (Some(PyObj::Tuple(items)), Value::Obj(i)) => h
            .nt_meta
            .get(i)
            .map(|m| (items.clone(), m.fields.clone(), m.type_name.clone())),
        _ => None,
    })?;
    match name {
        "_asdict" => Some(Ok(with_host(|h| {
            let mut d: IndexMap<PKey, (Value, Value)> = IndexMap::new();
            for (f, v) in fields.iter().zip(items.iter()) {
                let kv = h.new_str(f.clone());
                d.insert(PKey::Str(f.clone()), (kv, v.clone()));
            }
            h.new_dict(d)
        }))),
        "_replace" => {
            let mut new_items = items.clone();
            for (k, v) in kwargs {
                match fields.iter().position(|f| f == k) {
                    Some(idx) => new_items[idx] = v.clone(),
                    None => {
                        return Some(Err(host::type_error(&format!(
                            "{type_name}() got an unexpected keyword argument '{k}'"
                        ))))
                    }
                }
            }
            Some(host::namedtuple_construct(
                &type_name,
                &fields,
                new_items,
                vec![],
            ))
        }
        "count" | "index" => Some(tuple_method(recv, name, args)),
        _ => None,
    }
}

pub fn call_type_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    if let Some(r) = nt_instance_method(recv, name, &args, &kwargs) {
        return r;
    }
    // `cls.__subclasses__()` — the class's immediate user subclasses.
    if name == "__subclasses__" {
        return Ok(with_host(|h| {
            let cn = match h.get(recv) {
                Some(PyObj::Class(n)) => n.clone(),
                _ => String::new(),
            };
            let subs: Vec<Value> = h
                .subclasses_of(&cn)
                .into_iter()
                .map(|n| h.alloc(PyObj::Class(n)))
                .collect();
            h.new_list(subs)
        }));
    }
    let tn = with_host(|h| h.type_name(recv));
    // Universal container dunder: `c.__contains__(x)` == `x in c`. Routed through
    // the host membership check so a method bound off any builtin container
    // (`frozenset(...).__contains__`, `"abc".__contains__`, …) is callable.
    if name == "__contains__" {
        let item = arg0(&args)?;
        return Ok(Value::Bool(with_host(|h| h.contains(&item, recv))?));
    }
    // Other universal object dunders reached as bound methods (`cache.__len__`,
    // `cache.__getitem__(k)`, `x.__eq__(y)`) — dispatch to the native op.
    match name {
        "__len__" => return Ok(Value::Int(py_len(recv)? as i64)),
        "__getitem__" => {
            let k = arg0(&args)?;
            return with_host(|h| h.get_item(recv, &k));
        }
        "__setitem__" => {
            let k = arg0(&args)?;
            let v = args.get(1).cloned().unwrap_or(Value::Undef);
            with_host(|h| h.set_item(recv, &k, v))?;
            return Ok(Value::Undef);
        }
        "__delitem__" => {
            let k = arg0(&args)?;
            with_host(|h| h.del_item(recv, &k))?;
            return Ok(Value::Undef);
        }
        "__iter__" => return with_host(|h| h.make_iter(recv)),
        "__str__" => return Ok(with_host(|h| {
            let s = h.str_of(recv);
            h.new_str(s)
        })),
        "__repr__" => return Ok(with_host(|h| {
            let s = h.repr_of(recv);
            h.new_str(s)
        })),
        "__bool__" => return Ok(Value::Bool(with_host(|h| h.truthy(recv)))),
        _ => {}
    }
    // Set-like dict views (`dict_keys`/`dict_items`) support `isdisjoint`.
    if matches!(tn.as_str(), "dict_keys" | "dict_items") && name == "isdisjoint" {
        let other = arg0(&args)?;
        let mine = host::iter_vec(recv)?;
        let theirs = host::iter_vec(&other)?;
        let disjoint = !mine
            .iter()
            .any(|e| theirs.iter().any(|o| with_host(|h| h.equal(e, o))));
        return Ok(Value::Bool(disjoint));
    }
    match tn.as_str() {
        // `.format` needs the kwargs (keyword replacement fields); other str
        // methods don't take keywords.
        "str" if name == "format" => {
            let s = with_host(|h| h.as_str(recv)).unwrap_or_default();
            str_dot_format(&s, &args, &kwargs)
        }
        // `str.splitlines(keepends=...)` — fold the keyword into the positional
        // arg `str_method` reads.
        "str" if name == "splitlines" && !kwargs.is_empty() => {
            let a: Vec<Value> = kwargs
                .iter()
                .find(|(k, _)| k == "keepends")
                .map(|(_, v)| v.clone())
                .into_iter()
                .collect();
            str_method(recv, name, &a)
        }
        // `str.encode(encoding=..., errors=...)` — fold keywords into the
        // positional (encoding, errors) order `str_method`'s `encode` expects.
        "str" if name == "encode" && !kwargs.is_empty() => {
            let mut enc = args.first().cloned();
            let mut err = args.get(1).cloned();
            for (k, v) in &kwargs {
                match k.as_str() {
                    "encoding" => enc = Some(v.clone()),
                    "errors" => err = Some(v.clone()),
                    _ => {}
                }
            }
            let mut a2 = vec![enc.unwrap_or_else(|| new_str("utf-8".into()))];
            if let Some(e) = err {
                a2.push(e);
            }
            str_method(recv, name, &a2)
        }
        "str" => str_method(recv, name, &fold_str_kwargs(name, &args, &kwargs)?),
        // `bytes/bytearray.decode(encoding=..., errors=...)` — fold keywords into
        // the positional (encoding, errors) order the decoder expects.
        "bytes" | "bytearray" if name == "decode" && !kwargs.is_empty() => {
            let mut enc = args.first().cloned();
            let mut err = args.get(1).cloned();
            for (k, v) in &kwargs {
                match k.as_str() {
                    "encoding" => enc = Some(v.clone()),
                    "errors" => err = Some(v.clone()),
                    _ => {}
                }
            }
            let mut a2 = vec![enc.unwrap_or_else(|| new_str("utf-8".into()))];
            if let Some(e) = err {
                a2.push(e);
            }
            decode_bytes(&recv_bytes(recv), &a2)
        }
        "bytes" => bytes_method(recv, name, &args),
        "bytearray" => bytearray_method(recv, name, &args),
        "memoryview" => memoryview_method(recv, name, &args),
        "list" => list_method(recv, name, &args, &kwargs),
        "dict" => dict_method(recv, name, &args, &kwargs),
        "Counter" | "defaultdict" | "OrderedDict" => {
            if let Some(r) = collections_dict_method(recv, name, &args, &kwargs, &tn) {
                return r;
            }
            dict_method(recv, name, &args, &kwargs)
        }
        "set" | "frozenset" => set_method(recv, name, &args),
        "tuple" => tuple_method(recv, name, &args),
        "range" => range_method(recv, name, &args),
        "slice" => slice_method(recv, name, &args),
        "deque" => deque_method(recv, name, &args),
        "TextIOWrapper" => file_method(recv, name, &args),
        "functools._lru_cache_wrapper" => lru_wrapper_method(recv, name),
        "int" | "float" | "bool" if is_num_dunder(&tn, name) => num_dunder(recv, name, &args),
        "int" | "float" | "bool" => num_method(recv, name, &args, &kwargs),
        "complex" => complex_method(recv, name),
        "property" => property_method(recv, name, &args),
        "generator" => generator_method(recv, name, &args),
        "coroutine" => coroutine_method(recv, name, &args),
        "async_generator" => async_generator_method(recv, name, args),
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

/// `agen.__aiter__/__anext__` — an async generator's async-iteration protocol.
/// Both return the generator itself: `__aiter__` is the async iterator, and
/// awaiting `__anext__` drives one step (see `async_rt::drive_async_gen`).
fn async_generator_method(recv: &Value, name: &str, args: Vec<Value>) -> Result<Value, String> {
    match name {
        "__aiter__" | "__anext__" => Ok(recv.clone()),
        // `asend`/`athrow`/`aclose` return awaitables driven by the async runtime.
        "asend" | "athrow" | "aclose" => crate::async_rt::async_gen_method(recv, name, args),
        _ => Err(format!(
            "AttributeError: 'async_generator' object has no attribute '{name}'"
        )),
    }
}

/// The abort/error string for an exception object (`Class` or `Class: message`).
fn exc_error_string(exc: &Value) -> String {
    with_host(|h| match h.get(exc) {
        Some(PyObj::Exception { class, args }) => {
            let msg = h.exc_message(class, args);
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
            // Closing an un-started generator/coroutine just marks it closed —
            // CPython does not run the body — and clears the "never awaited"
            // warning for a coroutine created only to read its type.
            if with_host(|h| h.close_unstarted_gen(recv)) {
                return Ok(Value::Undef);
            }
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

/// Prefixes for `str.startswith`/`endswith`: a single str, or every str in a
/// tuple. Mirrors CPython accepting `str | tuple[str, ...]`.
fn str_prefixes(v: &Value) -> Result<Vec<String>, String> {
    if let Some(s) = with_host(|h| h.as_str(v)) {
        return Ok(vec![s]);
    }
    let items = host::iter_vec(v)?;
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        out.push(
            with_host(|h| h.as_str(&it))
                .ok_or_else(|| host::type_error("tuple for startswith must only contain str"))?,
        );
    }
    Ok(out)
}

/// Whitespace split for `str.split(None, maxsplit)` / `rsplit`. Runs of Unicode
/// whitespace (`char::is_whitespace`, matching the `split_whitespace` used
/// elsewhere) separate fields; no empty fields; leading/trailing whitespace is
/// dropped. On hitting `maxsplit` the remainder becomes one field (leading
/// whitespace skipped for forward, trailing preserved; the mirror for reverse).
fn split_ws_str(s: &str, maxsplit: i64, reverse: bool) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    if reverse {
        let mut rest = s;
        let mut splits = 0;
        loop {
            let trimmed = rest.trim_end_matches(char::is_whitespace);
            if trimmed.is_empty() {
                break;
            }
            if maxsplit >= 0 && splits >= maxsplit {
                parts.push(trimmed.to_string());
                break;
            }
            match trimmed.rfind(char::is_whitespace) {
                Some(idx) => {
                    let after = idx + trimmed[idx..].chars().next().unwrap().len_utf8();
                    parts.push(trimmed[after..].to_string());
                    rest = &trimmed[..idx];
                    splits += 1;
                }
                None => {
                    parts.push(trimmed.to_string());
                    break;
                }
            }
        }
        parts.reverse();
    } else {
        let mut rest = s;
        let mut splits = 0;
        loop {
            let trimmed = rest.trim_start_matches(char::is_whitespace);
            if trimmed.is_empty() {
                break;
            }
            if maxsplit >= 0 && splits >= maxsplit {
                parts.push(trimmed.to_string());
                break;
            }
            match trimmed.find(char::is_whitespace) {
                Some(idx) => {
                    parts.push(trimmed[..idx].to_string());
                    rest = &trimmed[idx..];
                    splits += 1;
                }
                None => {
                    parts.push(trimmed.to_string());
                    break;
                }
            }
        }
    }
    parts
}

/// A Unicode line boundary per CPython `str.splitlines`: `\n`, `\r`, `\v`
/// (\x0b), `\f` (\x0c), `\x1c`, `\x1d`, `\x1e`, `\x85` (NEL), ` ` (LINE
/// SEPARATOR), ` ` (PARAGRAPH SEPARATOR). `\r\n` is treated as one boundary
/// by the caller.
fn is_line_boundary(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\u{0b}'
            | '\u{0c}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// CPython `str.splitlines(keepends)`: split at Unicode line boundaries, joining
/// `\r\n` into a single break. A trailing boundary does not yield a final empty
/// line. With `keepends`, the boundary character(s) stay attached to their line.
fn str_splitlines(s: &str, keepends: bool) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        if is_line_boundary(chars[i]) {
            // `\r\n` is one break; the newline extends the boundary run.
            let mut end = i + 1;
            if chars[i] == '\r' && end < chars.len() && chars[end] == '\n' {
                end += 1;
            }
            let slice_end = if keepends { end } else { i };
            out.push(chars[start..slice_end].iter().collect());
            start = end;
            i = end;
        } else {
            i += 1;
        }
    }
    if start < chars.len() {
        out.push(chars[start..].iter().collect());
    }
    out
}

/// The positional index a keyword argument occupies for a `str` method that
/// accepts keywords (`"a b".split(maxsplit=1)` → `maxsplit` is arg 1). Only the
/// argument-clinic methods below take keywords; every other `str` method is
/// `METH_VARARGS`-only and rejects them (see `str_accepts_kwargs`). `None` = an
/// unexpected keyword for that method.
fn str_kwarg_pos(method: &str, kw: &str) -> Option<usize> {
    Some(match (method, kw) {
        ("split" | "rsplit", "sep") => 0,
        ("split" | "rsplit", "maxsplit") => 1,
        ("replace", "old") => 0,
        ("replace", "new") => 1,
        ("replace", "count") => 2,
        ("expandtabs", "tabsize") | ("splitlines", "keepends") => 0,
        _ => return None,
    })
}

/// Whether CPython accepts keyword arguments for this `str` method. Most reject
/// them (`str.center() takes no keyword arguments`); only these argument-clinic
/// methods accept keywords. (`encode`/`splitlines`-with-kwargs are folded by
/// dedicated arms before reaching here; both are listed for completeness.)
fn str_accepts_kwargs(method: &str) -> bool {
    matches!(
        method,
        "split" | "rsplit" | "replace" | "expandtabs" | "splitlines" | "encode"
    )
}

/// Fold keyword arguments into the positional slots `str_method` reads, so a
/// keyword call (`"a b c".split(maxsplit=1)`) behaves like the positional form.
/// A method that does not accept keywords, or an unexpected keyword name, raises
/// the same `TypeError` CPython does. Unfilled interior slots become
/// `Value::Undef` (treated as "not given").
fn fold_str_kwargs(
    method: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Vec<Value>, String> {
    if kwargs.is_empty() {
        return Ok(args.to_vec());
    }
    if !str_accepts_kwargs(method) {
        return Err(format!(
            "TypeError: str.{method}() takes no keyword arguments"
        ));
    }
    let mut out: Vec<Option<Value>> = args.iter().cloned().map(Some).collect();
    for (k, v) in kwargs {
        match str_kwarg_pos(method, k) {
            Some(pos) => {
                if pos >= out.len() {
                    out.resize(pos + 1, None);
                }
                out[pos] = Some(v.clone());
            }
            None => {
                return Err(format!(
                    "TypeError: {method}() got an unexpected keyword argument '{k}'"
                ))
            }
        }
    }
    Ok(out.into_iter().map(|o| o.unwrap_or(Value::Undef)).collect())
}

fn str_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let s = with_host(|h| h.as_str(recv)).unwrap_or_default();
    let sarg = |i: usize| with_host(|h| args.get(i).and_then(|v| h.as_str(v))).unwrap_or_default();
    match name {
        "upper" => Ok(new_str(s.to_uppercase())),
        "lower" => Ok(new_str(s.to_lowercase())),
        // Full Unicode case folding: identical to lowercasing except for the
        // codepoints in the override table (multi-char folds like `ß`->`ss`).
        "casefold" => {
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                match crate::casefold::casefold_override(c) {
                    Some(f) => out.push_str(f),
                    None => out.extend(c.to_lowercase()),
                }
            }
            Ok(new_str(out))
        }
        "strip" => Ok(new_str(strip_str(&s, args, 3))),
        "lstrip" => Ok(new_str(strip_str(&s, args, 1))),
        "rstrip" => Ok(new_str(strip_str(&s, args, 2))),
        "swapcase" => Ok(new_str(
            // Unicode-aware: a cased letter maps to the opposite case (case
            // mapping can be 1→many, e.g. `ß` → `SS`); non-cased chars pass
            // through. ASCII-only mapping left `ï`/`é` unchanged.
            s.chars()
                .flat_map(|c| {
                    if c.is_uppercase() {
                        c.to_lowercase().collect::<Vec<_>>()
                    } else if c.is_lowercase() {
                        c.to_uppercase().collect::<Vec<_>>()
                    } else {
                        vec![c]
                    }
                })
                .collect::<String>(),
        )),
        "capitalize" => {
            // CPython titlecases the first character (`ǳ` → `ǲ`, not `Ǳ`) and
            // lowercases the rest.
            let mut c = s.chars();
            let out = match c.next() {
                Some(f) => to_titlecase(f) + &c.as_str().to_lowercase(),
                None => String::new(),
            };
            Ok(new_str(out))
        }
        "title" => {
            let mut out = String::new();
            let mut prev_cased = false;
            for ch in s.chars() {
                // CPython's `str.title` cases word boundaries by the *cased*
                // property (letters + titlecase-mapped digraphs), not `isalpha`.
                if ch.is_alphabetic() {
                    if prev_cased {
                        out.extend(ch.to_lowercase());
                    } else {
                        out.push_str(&to_titlecase(ch));
                    }
                    prev_cased = true;
                } else {
                    out.push(ch);
                    prev_cased = false;
                }
            }
            Ok(new_str(out))
        }
        "split" | "rsplit" => {
            let reverse = name == "rsplit";
            let maxsplit = args
                .get(1)
                .and_then(|v| with_host(|h| h.as_int(v)))
                .unwrap_or(-1);
            let none_sep = args.is_empty() || matches!(args.first(), Some(Value::Undef));
            let strs: Vec<String> = if none_sep {
                split_ws_str(&s, maxsplit, reverse)
            } else {
                let sep = sarg(0);
                if sep.is_empty() {
                    return Err("ValueError: empty separator".into());
                }
                let cap = if maxsplit >= 0 {
                    (maxsplit as usize).saturating_add(1)
                } else {
                    usize::MAX
                };
                if reverse {
                    let mut v: Vec<String> = s
                        .rsplitn(cap, sep.as_str())
                        .map(|p| p.to_string())
                        .collect();
                    v.reverse();
                    v
                } else {
                    s.splitn(cap, sep.as_str()).map(|p| p.to_string()).collect()
                }
            };
            let parts: Vec<Value> = strs.into_iter().map(new_str).collect();
            Ok(with_host(|h| h.new_list(parts)))
        }
        "splitlines" => {
            let keepends = args
                .first()
                .map(|v| py_bool(v).unwrap_or(false))
                .unwrap_or(false);
            let parts: Vec<Value> = str_splitlines(&s, keepends)
                .into_iter()
                .map(new_str)
                .collect();
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
        "startswith" | "endswith" => {
            let chars: Vec<char> = s.chars().collect();
            let (start, end) = resolve_start_end(chars.len(), args, 1);
            let region: String = chars[start..end.min(chars.len())].iter().collect();
            let prefixes = match args.first() {
                Some(v) if !matches!(v, Value::Undef) => str_prefixes(v)?,
                _ => return Err(host::type_error("startswith first arg must be str")),
            };
            let hit = prefixes.iter().any(|p| {
                if name == "startswith" {
                    region.starts_with(p.as_str())
                } else {
                    region.ends_with(p.as_str())
                }
            });
            Ok(Value::Bool(hit))
        }
        "find" | "rfind" => {
            let needle: Vec<char> = sarg(0).chars().collect();
            let chars: Vec<char> = s.chars().collect();
            let (start, end) = resolve_start_end(chars.len(), args, 1);
            let p = slice_find(&chars, &needle, start, end, name == "rfind");
            Ok(Value::Int(p.map(|x| x as i64).unwrap_or(-1)))
        }
        "index" | "rindex" => {
            let needle: Vec<char> = sarg(0).chars().collect();
            let chars: Vec<char> = s.chars().collect();
            let (start, end) = resolve_start_end(chars.len(), args, 1);
            match slice_find(&chars, &needle, start, end, name == "rindex") {
                Some(p) => Ok(Value::Int(p as i64)),
                None => Err("ValueError: substring not found".into()),
            }
        }
        "count" => {
            let sub: Vec<char> = sarg(0).chars().collect();
            let chars: Vec<char> = s.chars().collect();
            let (start, end) = resolve_start_end(chars.len(), args, 1);
            Ok(Value::Int(count_range(&chars, &sub, start, end) as i64))
        }
        "isdigit" => Ok(Value::Bool(!s.is_empty() && s.chars().all(is_digit_char))),
        "isalpha" => Ok(Value::Bool(
            !s.is_empty() && s.chars().all(|c| c.is_alphabetic()),
        )),
        "isalnum" => Ok(Value::Bool(
            !s.is_empty() && s.chars().all(|c| c.is_alphanumeric()),
        )),
        "isspace" => Ok(Value::Bool(!s.is_empty() && s.chars().all(is_py_space))),
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
            let n = s.chars().count();
            let out = if n < w {
                let pad = "0".repeat(w - n);
                // A leading sign (`+`/`-`) stays in front of the zero padding.
                match s.strip_prefix(['+', '-']) {
                    Some(rest) => format!("{}{pad}{rest}", &s[..1]),
                    None => format!("{pad}{s}"),
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
        "isdecimal" => Ok(Value::Bool(!s.is_empty() && s.chars().all(is_decimal_char))),
        "isidentifier" => Ok(Value::Bool(is_identifier(&s))),
        "istitle" => Ok(Value::Bool(is_titlecased(&s))),
        "isprintable" => Ok(Value::Bool(s.chars().all(host::is_printable_char))),
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
        "isascii" => Ok(Value::Bool(s.chars().all(|c| (c as u32) < 0x80))),
        "encode" => {
            let enc = args
                .first()
                .and_then(|v| with_host(|h| h.as_str(v)))
                .unwrap_or_else(|| "utf-8".into());
            let errors = args
                .get(1)
                .and_then(|v| with_host(|h| h.as_str(v)))
                .unwrap_or_else(|| "strict".into());
            let bytes = encode_str(&s, &enc, &errors)?;
            Ok(with_host(|h| h.alloc(PyObj::Bytes(bytes))))
        }
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
            // CPython `str.center`: `left = marg/2 + (marg & width & 1)`, so the
            // odd byte favors the left when both margin and width are odd.
            let left = total / 2 + (total & w & 1);
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
    type_new_meta(name, bases, ns, "type", vec![])
}

/// `type.__new__(mcls, name, bases, namespace, **kwds)` — like `type_new` but
/// tags the new class's metaclass as `metaclass` (so `type(cls) is mcls`) and
/// fires the class-creation hooks (`__set_name__`, `__init_subclass__`) that
/// live in `type.__new__`. `class_kwargs` are the keywords the metaclass's
/// `__new__` forwarded here (its own were already consumed), so they reach
/// `__init_subclass__` exactly as CPython delivers them.
pub fn type_new_meta(
    name: &Value,
    bases: &Value,
    ns: &Value,
    metaclass: &str,
    class_kwargs: Vec<(String, Value)>,
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
    // The namespace may be a plain dict OR a dict SUBCLASS instance (enum's
    // `_EnumDict`), whose entries live in its builtin-base payload dict.
    let dict_handle = with_host(|h| match h.get(ns) {
        Some(PyObj::Dict(_)) => Some(ns.clone()),
        Some(PyObj::Instance(inst)) if !matches!(inst.payload, Value::Undef) => {
            Some(inst.payload.clone())
        }
        _ => None,
    });
    let namespace: IndexMap<String, Value> = with_host(|h| {
        match dict_handle.as_ref().and_then(|d| h.get(d)) {
            Some(PyObj::Dict(d)) => d
                .values()
                .filter_map(|(k, v)| h.as_str(k).map(|s| (s, v.clone())))
                .collect(),
            _ => IndexMap::new(),
        }
    });
    let cls = with_host(|h| {
        h.register_class_meta(&cname, base_names, namespace.clone(), metaclass)
    });
    // Descriptor naming and PEP 487 both run inside `type.__new__` in CPython —
    // fire them here so a metaclass's `super().__new__(mcls, name, bases,
    // classdict, **kwds)` builds enum members (`_proto_member.__set_name__`),
    // names any other descriptors, and delivers only the still-unconsumed
    // keywords to `__init_subclass__`.
    host::fire_set_name(&cname, &namespace)?;
    host::fire_init_subclass(&cname, class_kwargs)?;
    Ok(cls)
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

/// CPython `str.isdecimal` / `Py_UNICODE_ISDECIMAL`: the Unicode `Nd` category
/// (Decimal_Number) — ASCII `0-9` plus other scripts' decimal digits (Devanagari
/// `०`, Thai `๓`, fullwidth), but NOT superscripts or fractions. Rust's
/// `is_ascii_digit` covered only ASCII.
fn is_decimal_char(c: char) -> bool {
    use unicode_general_category::{get_general_category, GeneralCategory};
    get_general_category(c) == GeneralCategory::DecimalNumber
}

/// CPython `str.isdigit` / `Py_UNICODE_ISDIGIT`: every decimal digit
/// ([`is_decimal_char`]) plus the `Numeric_Type=Digit` characters that are not
/// themselves decimal — superscripts/subscripts (`²`, `₃`), circled/parenthesized
/// digits (`①`, `⑽`), etc. This second set is finite and stable (Unicode's
/// Numeric_Type property, which Rust's std does not expose).
fn is_digit_char(c: char) -> bool {
    is_decimal_char(c)
        || matches!(c,
            '\u{00B2}'..='\u{00B3}' | '\u{00B9}' | '\u{1369}'..='\u{1371}' | '\u{19DA}'
            | '\u{2070}' | '\u{2074}'..='\u{2079}' | '\u{2080}'..='\u{2089}'
            | '\u{2460}'..='\u{2468}' | '\u{2474}'..='\u{247C}' | '\u{2488}'..='\u{2490}'
            | '\u{24EA}' | '\u{24F5}'..='\u{24FD}' | '\u{24FF}' | '\u{2776}'..='\u{277E}'
            | '\u{2780}'..='\u{2788}' | '\u{278A}'..='\u{2792}' | '\u{10A40}'..='\u{10A43}'
            | '\u{10E60}'..='\u{10E68}' | '\u{11052}'..='\u{1105A}' | '\u{1F100}'..='\u{1F10A}')
}

/// The titlecase form of `ch` (CPython's `str.title`/`capitalize` first-letter
/// mapping). Rust's std only exposes uppercase; the Latin digraph ligatures whose
/// titlecase differs from their uppercase (`ǳ` → `ǲ`, not `Ǳ`) are handled
/// explicitly, everything else uppercases.
fn to_titlecase(ch: char) -> String {
    match ch {
        '\u{01C4}' | '\u{01C5}' | '\u{01C6}' => "\u{01C5}".to_string(),
        '\u{01C7}' | '\u{01C8}' | '\u{01C9}' => "\u{01C8}".to_string(),
        '\u{01CA}' | '\u{01CB}' | '\u{01CC}' => "\u{01CB}".to_string(),
        '\u{01F1}' | '\u{01F2}' | '\u{01F3}' => "\u{01F2}".to_string(),
        _ => ch.to_uppercase().collect(),
    }
}

/// CPython `str.isspace` / `Py_UNICODE_ISSPACE`: Rust's `White_Space` set plus
/// the four ASCII information separators U+001C..U+001F, which CPython counts as
/// whitespace (bidirectional category B/S) but Rust does not.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// CPython identifier "continue" chars beyond `XID_Continue`: the
/// `Other_ID_Continue` set (U+00B7, U+0387, U+1369..U+1371, U+19DA) plus the
/// zero-width joiner / non-joiner (U+200C/U+200D), which PEP 3131 permits.
fn is_other_id_continue(c: char) -> bool {
    matches!(
        c,
        '\u{00b7}' | '\u{0387}' | '\u{1369}'..='\u{1371}' | '\u{19da}' | '\u{200c}' | '\u{200d}'
    )
}

/// CPython `str.isidentifier` (approximated with Rust's Unicode categories plus
/// the `Other_ID_Start`/`Other_ID_Continue` chars CPython adds on top).
fn is_identifier(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        Some(c) if c == '_' || c.is_alphabetic() => {}
        _ => return false,
    }
    it.all(|c| c == '_' || c.is_alphanumeric() || is_other_id_continue(c))
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

/// Byte-level `expandtabs`: identical column logic to `expand_tabs` but over raw
/// bytes (b'\t' → spaces to the next stop; b'\n'/b'\r' reset the column).
fn expand_tabs_bytes(bytes: &[u8], tabsize: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut col = 0usize;
    for &b in bytes {
        match b {
            b'\t' => {
                if tabsize == 0 {
                    continue;
                }
                let n = tabsize - (col % tabsize);
                out.extend(std::iter::repeat(b' ').take(n));
                col += n;
            }
            b'\n' | b'\r' => {
                out.push(b);
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

/// Resolve `mapping[key]` for `str.format_map`, using the full subscript
/// protocol: a custom mapping's `__getitem__`, a `defaultdict`/`Counter`
/// `__missing__`, then a plain-dict lookup (KeyError on miss) — exactly what
/// CPython's `PyObject_GetItem` does for the replacement-field name.
fn format_map_get(mapping: &Value, key: &str) -> Result<Value, String> {
    let keyv = with_host(|h| h.new_str(key.to_string()));
    if with_host(|h| matches!(h.get(mapping), Some(PyObj::Instance(_)))) {
        return host::call_method(mapping, "__getitem__", vec![keyv], vec![]);
    }
    if let Some(meta) = host::dict_meta_of(mapping) {
        let missing = with_host(|h| match h.to_key(&keyv) {
            Ok(k) => matches!(h.get(mapping), Some(PyObj::Dict(d)) if !d.contains_key(&k)),
            Err(_) => false,
        });
        if missing {
            match meta.kind {
                host::DictKind::Counter => return Ok(Value::Int(0)),
                host::DictKind::DefaultDict => {
                    if let Some(factory) = meta.factory {
                        let default = host::invoke(&factory, vec![], vec![])?;
                        with_host(|h| h.set_item(mapping, &keyv, default.clone()))?;
                        return Ok(default);
                    }
                }
                host::DictKind::OrderedDict => {}
            }
        }
    }
    with_host(|h| h.get_item(mapping, &keyv))
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
                let val = format_map_get(mapping, &fname)?;
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

/// Shared automatic/manual field-numbering state for one `str.format` call.
/// `mode` is `None` until the first `{}`/`{0}` fixes the numbering discipline;
/// mixing the two afterwards is a `ValueError`, exactly as in CPython.
#[derive(Default)]
struct FieldNum {
    counter: usize,
    /// `Some(false)` = automatic (`{}`), `Some(true)` = manual (`{0}`).
    manual: Option<bool>,
}

/// Resolve a `str.format` field name (`arg_name` plus `.attr` / `[index]`
/// accessor chain) against positional `args`, `kwargs`, and the shared
/// automatic/manual field-numbering state `st`.
fn resolve_format_arg(
    name: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
    st: &mut FieldNum,
) -> Result<Value, String> {
    let nchars: Vec<char> = name.chars().collect();
    // Base arg_name ends at the first `.` or `[`.
    let mut i = 0;
    while i < nchars.len() && nchars[i] != '.' && nchars[i] != '[' {
        i += 1;
    }
    let base: String = nchars[..i].iter().collect();
    let mut val = if base.is_empty() {
        if st.manual == Some(true) {
            return Err(
                "ValueError: cannot switch from manual field specification to \
                        automatic field numbering"
                    .into(),
            );
        }
        st.manual = Some(false);
        let v = args
            .get(st.counter)
            .cloned()
            .ok_or_else(|| format!("IndexError: Replacement index {} out of range", st.counter))?;
        st.counter += 1;
        v
    } else if let Ok(n) = base.parse::<usize>() {
        if st.manual == Some(false) {
            return Err(
                "ValueError: cannot switch from automatic field numbering to \
                        manual field specification"
                    .into(),
            );
        }
        st.manual = Some(true);
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
/// level, per CPython), formatting each with its default `str()` and threading
/// the shared automatic/manual field-numbering state.
fn substitute_nested_spec(
    spec: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
    st: &mut FieldNum,
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
                let val = resolve_format_arg(&fname, args, kwargs, st)?;
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
    let mut auto = FieldNum::default();
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

/// The argument-count contract for a `list` method (positional args only; `sort`
/// takes keyword-only `key`/`reverse`). `None` for names this dispatcher does not
/// treat as fixed-arity.
fn list_arity(name: &str) -> Option<Arity> {
    Some(match name {
        "append" | "extend" | "remove" | "count" => Arity::ExactlyOne,
        "clear" | "reverse" | "copy" => Arity::NoArgs,
        "insert" => Arity::VarExact(2),
        "pop" => Arity::VarRange(0, 1),
        "index" => Arity::VarRange(1, 3),
        "sort" => Arity::NoPositional,
        _ => return None,
    })
}

fn list_method(
    recv: &Value,
    name: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value, String> {
    if let Some(spec) = list_arity(name) {
        check_arity(name, &format!("list.{name}"), spec, args.len())?;
    }
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
            // Locate via the rich `==` dunder (outside the borrow) so a user
            // `__eq__` is honored, then remove the first match.
            let elems = with_host(|h| match h.get(recv) {
                Some(PyObj::List(l)) => l.clone(),
                _ => Vec::new(),
            });
            let mut pos = None;
            for (i, e) in elems.iter().enumerate() {
                if elem_equal(e, &v)? {
                    pos = Some(i);
                    break;
                }
            }
            match pos {
                Some(p) => {
                    with_host(|h| {
                        if let Some(PyObj::List(l)) = h.get_mut(recv) {
                            l.remove(p);
                        }
                    });
                    Ok(Value::Undef)
                }
                None => Err("ValueError: list.remove(x): x not in list".into()),
            }
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
            // `list.index(x[, start[, stop]])` — search the half-open `[start, stop)`
            // window; negative bounds normalize against `len` (clamped, like a slice).
            let v = arg0(args)?;
            let n = with_host(|h| match h.get(recv) {
                Some(PyObj::List(l)) => l.len() as i64,
                _ => 0,
            });
            let mut start = match args.get(1) {
                Some(a) => with_host(|h| h.as_int(a)).unwrap_or(0),
                None => 0,
            };
            let mut stop = match args.get(2) {
                Some(a) => with_host(|h| h.as_int(a)).unwrap_or(n),
                None => n,
            };
            if start < 0 {
                start += n;
                if start < 0 {
                    start = 0;
                }
            }
            if stop < 0 {
                stop += n;
            }
            if stop > n {
                stop = n;
            }
            // Compare via the rich `==` dunder (outside the borrow) so a user
            // `__eq__` is honored; the element is the forward operand.
            let elems = with_host(|h| match h.get(recv) {
                Some(PyObj::List(l)) => l.clone(),
                _ => Vec::new(),
            });
            let mut i = start;
            while i < stop {
                if elem_equal(&elems[i as usize], &v)? {
                    return Ok(Value::Int(i));
                }
                i += 1;
            }
            Err("ValueError: list.index(x): x not in list".into())
        }
        "count" => {
            let v = arg0(args)?;
            let elems = with_host(|h| match h.get(recv) {
                Some(PyObj::List(l)) => l.clone(),
                _ => Vec::new(),
            });
            let mut n = 0i64;
            for e in &elems {
                if elem_equal(e, &v)? {
                    n += 1;
                }
            }
            Ok(Value::Int(n))
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

/// Argument-count contract for a `dict` method (positional args; `get`/`pop`/
/// `setdefault`/`update` also accept keywords in some CPython paths, but the
/// positional cap still applies).
fn dict_arity(name: &str) -> Option<Arity> {
    Some(match name {
        "keys" | "values" | "items" | "clear" | "copy" | "popitem" => Arity::NoArgs,
        "get" | "pop" | "setdefault" => Arity::VarRange(1, 2),
        "update" => Arity::VarRange(0, 1),
        _ => return None,
    })
}

fn dict_method(
    recv: &Value,
    name: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value, String> {
    if let Some(spec) = dict_arity(name) {
        check_arity(name, &format!("dict.{name}"), spec, args.len())?;
    }
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
                    None => Err(with_host(|h| h.key_error(&kv))),
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
                None => Err("KeyError: popitem(): dictionary is empty".into()),
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

/// Argument-count contract for a `set`/`frozenset` method. The variadic algebra
/// methods (`union`, `intersection`, …) accept any count and are omitted.
fn set_arity(name: &str) -> Option<Arity> {
    Some(match name {
        "add" | "discard" | "remove" => Arity::ExactlyOne,
        "clear" | "copy" | "pop" => Arity::NoArgs,
        _ => return None,
    })
}

fn set_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    // Real `set` only: a `frozenset` mutator (`add`/`discard`/…) must raise the
    // AttributeError below first, so arity is not checked for it here.
    if !with_host(|h| h.is_frozenset(recv)) {
        if let Some(spec) = set_arity(name) {
            check_arity(name, &format!("set.{name}"), spec, args.len())?;
        }
    }
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
                return Err(with_host(|h| h.key_error(&v)));
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
        // `union`/`intersection`/`difference` are variadic (`s.union(*others)`);
        // `symmetric_difference` takes exactly one argument.
        "union" => set_variadic(recv, args, host::binop::BITOR),
        "intersection" => set_variadic(recv, args, host::binop::BITAND),
        "symmetric_difference" => set_binop(recv, args, host::binop::BITXOR),
        "difference" => {
            if args.is_empty() {
                return set_method(recv, "copy", &[]);
            }
            let mut acc = recv.clone();
            for a in args {
                let other_set = if with_host(|h| h.setlike(a).is_some()) {
                    a.clone()
                } else {
                    call_builtin_function("set", vec![a.clone()], vec![])?
                };
                acc = with_host(|h| h.arith(NumOp::Sub, &acc, &other_set))?;
            }
            Ok(acc)
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
        // Variadic: `s.update(*others)` folds in every iterable argument.
        "update" => {
            for a in args {
                let items = host::iter_vec(a)?;
                for it in items {
                    let k = with_host(|h| h.to_key(&it))?;
                    with_host(|h| {
                        if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                            host::set_put(s, k, it);
                        }
                    });
                }
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
        // Both variadic: apply each argument's key set in sequence.
        "intersection_update" => {
            for a in args {
                let other = iter_keys(a)?;
                with_host(|h| {
                    if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                        s.retain(|k, _| other.contains(k));
                    }
                });
            }
            Ok(Value::Undef)
        }
        "difference_update" => {
            for a in args {
                let other = iter_keys(a)?;
                with_host(|h| {
                    if let Some(PyObj::Set(s)) = h.get_mut(recv) {
                        s.retain(|k, _| !other.contains(k));
                    }
                });
            }
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

/// Variadic set fold (`union`/`intersection`): apply `tag` between the receiver
/// and every argument in turn, coercing non-set arguments to sets. With no
/// arguments it returns a copy of the receiver (preserving set/frozenset type).
fn set_variadic(recv: &Value, args: &[Value], tag: i64) -> Result<Value, String> {
    if args.is_empty() {
        return set_method(recv, "copy", &[]);
    }
    let mut acc = recv.clone();
    for a in args {
        let other_set = if with_host(|h| h.setlike(a).is_some()) {
            a.clone()
        } else {
            call_builtin_function("set", vec![a.clone()], vec![])?
        };
        acc = with_host(|h| h.binop(tag, &acc, &other_set))?;
    }
    Ok(acc)
}

/// Resolve a slice bound that is an instance with `__index__` to a plain int,
/// leaving ints, `None`, and everything else untouched. Used to normalize a
/// slice before it drives a builtin-sequence read/assign/delete, and by
/// `slice.indices()`.
fn resolve_slice_bound(v: &Value) -> Result<Value, String> {
    match index_dunder(v)? {
        Some(iv) => Ok(iv),
        None => Ok(v.clone()),
    }
}

/// If `idx` is a `slice` whose start/stop/step include an instance with
/// `__index__`, return an equivalent slice with those bounds resolved to plain
/// ints (so `slice_bounds` sees integers). Non-slices and slices with no
/// instance bound are returned unchanged (identity preserved). This is applied
/// ONLY on the builtin-sequence path — a user `__getitem__` receives the slice
/// with its original bound objects, exactly as CPython delivers it.
fn normalize_slice_bounds(idx: &Value) -> Result<Value, String> {
    let parts = with_host(|h| match h.get(idx) {
        Some(PyObj::Slice { lo, hi, step }) => Some((lo.clone(), hi.clone(), step.clone())),
        _ => None,
    });
    let (lo, hi, step) = match parts {
        Some(t) => t,
        None => return Ok(idx.clone()),
    };
    let needs = with_host(|h| {
        [&lo, &hi, &step]
            .iter()
            .any(|b| matches!(h.get(b), Some(PyObj::Instance(_))))
    });
    if !needs {
        return Ok(idx.clone());
    }
    let lo = resolve_slice_bound(&lo)?;
    let hi = resolve_slice_bound(&hi)?;
    let step = resolve_slice_bound(&step)?;
    Ok(with_host(|h| h.alloc(PyObj::Slice { lo, hi, step })))
}

/// `slice.indices(len)` — the `(start, stop, step)` triple CPython computes via
/// `PySlice_GetIndicesEx`: step defaults to 1 (0 is a `ValueError`), a negative
/// length is a `ValueError`, and start/stop/step/len each honor `__index__`.
fn slice_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    if name != "indices" {
        return Err(host::type_error(&format!(
            "'slice' object has no attribute '{name}'"
        )));
    }
    if args.len() != 1 {
        return Err(host::type_error(&format!(
            "slice.indices() takes exactly one argument ({} given)",
            args.len()
        )));
    }
    let (lo, hi, step) = with_host(|h| match h.get(recv) {
        Some(PyObj::Slice { lo, hi, step }) => (lo.clone(), hi.clone(), step.clone()),
        _ => (Value::Undef, Value::Undef, Value::Undef),
    });
    // Length must be a non-negative int (via `__index__`).
    let nv = resolve_slice_bound(&args[0])?;
    let n = with_host(|h| h.as_int(&nv))
        .ok_or_else(|| host::type_error("'slice' object cannot be interpreted as an integer"))?;
    if n < 0 {
        return Err("ValueError: length should not be negative".into());
    }
    // Step defaults to 1; must be non-zero.
    let step_i = if matches!(step, Value::Undef) {
        1
    } else {
        let sv = resolve_slice_bound(&step)?;
        let s = with_host(|h| h.as_int(&sv))
            .ok_or_else(|| host::type_error("slice indices must be integers or None"))?;
        if s == 0 {
            return Err("ValueError: slice step cannot be zero".into());
        }
        s
    };
    let lo = resolve_slice_bound(&lo)?;
    let hi = resolve_slice_bound(&hi)?;
    let (start, stop) = with_host(|h| h.slice_adjust(&lo, &hi, step_i, n));
    Ok(with_host(|h| {
        h.new_tuple(vec![
            Value::Int(start),
            Value::Int(stop),
            Value::Int(step_i),
        ])
    }))
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
        "count" if args.len() != 1 => Err(host::type_error(&format!(
            "tuple.count() takes exactly one argument ({} given)",
            args.len()
        ))),
        "index" if args.len() > 3 => Err(host::type_error(&format!(
            "index expected at most 3 arguments, got {}",
            args.len()
        ))),
        "index" if args.is_empty() => Err(host::type_error(
            "index expected at least 1 argument, got 0",
        )),
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

/// Is `v` an `int`-like value (`int`, `bool`, or a bignum `int`)?
fn is_int_like(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::Bool(_))
        || with_host(|h| matches!(h.get(v), Some(PyObj::BigInt(_))))
}
/// Is `v` a numeric value `float` arithmetic accepts (`int`-like or `float`)?
fn is_num_like(v: &Value) -> bool {
    is_int_like(v) || matches!(v, Value::Float(_))
}
/// The `int` value of a numeric receiver: a `bool` normalizes to `0`/`1`
/// (`True.__index__()` is `1`, not `True`); everything else is unchanged.
fn to_int_value(recv: &Value) -> Value {
    match recv {
        Value::Bool(b) => Value::Int(*b as i64),
        other => other.clone(),
    }
}

/// Dispatch a numeric dunder method exposed as a bound method on `int`/`float`/
/// `bool` (`(5).__add__(2)`, `(-3).__abs__()`, `(2.0).__round__()`). Binary
/// dunders return the `NotImplemented` singleton for operand types the base
/// type declines — `int` combines only with `int`-likes, `float` with any
/// number — mirroring CPython, and delegate the actual computation to the same
/// host arithmetic the operators use. The caller guarantees `name` is a dunder
/// valid for the receiver's type (via [`is_num_dunder`]).
fn num_dunder(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    use host::binop as bo;
    let recv_float = matches!(recv, Value::Float(_));
    let ni = || Ok(with_host(|h| h.alloc(PyObj::NotImplemented)));
    let b = args.first().cloned().unwrap_or(Value::Undef);
    // Operand acceptable for a binary dunder on this receiver type?
    let accepts = |v: &Value| {
        if recv_float {
            is_num_like(v)
        } else {
            is_int_like(v)
        }
    };

    match name {
        // --- unary / conversion (operand ignored) ---
        "__abs__" => call_builtin_function("abs", vec![recv.clone()], vec![]),
        // Unary minus as `0 - self` keeps the receiver's type (int→int, float→float).
        "__neg__" => with_host(|h| h.arith(NumOp::Sub, &Value::Int(0), recv)),
        "__pos__" => Ok(to_int_value(recv)),
        // `~x == -(x + 1)`; integers only (float has no `__invert__`).
        "__invert__" => {
            let x = with_host(|h| h.big_val(recv)).unwrap_or_default();
            Ok(with_host(|h| {
                h.norm_big(-(x + num_bigint::BigInt::from(1)))
            }))
        }
        "__index__" => Ok(to_int_value(recv)),
        // `int(...)` truncates a float toward zero and normalizes bool→int.
        "__int__" | "__trunc__" => call_builtin_function("int", vec![recv.clone()], vec![]),
        "__float__" => call_builtin_function("float", vec![recv.clone()], vec![]),
        "__floor__" => match recv {
            Value::Float(f) => call_builtin_function("math.floor", vec![Value::Float(*f)], vec![]),
            _ => Ok(to_int_value(recv)),
        },
        "__ceil__" => match recv {
            Value::Float(f) => call_builtin_function("math.ceil", vec![Value::Float(*f)], vec![]),
            _ => Ok(to_int_value(recv)),
        },
        "__round__" => {
            let mut a = vec![recv.clone()];
            if let Some(nd) = args.first() {
                if !matches!(nd, Value::Undef) {
                    a.push(nd.clone());
                }
            }
            call_builtin_function("round", a, vec![])
        }
        "__bool__" => Ok(Value::Bool(with_host(|h| h.truthy(recv)))),
        "__repr__" => Ok(with_host(|h| {
            let s = h.repr_of(recv);
            h.new_str(s)
        })),
        "__str__" => Ok(with_host(|h| {
            let s = h.str_of(recv);
            h.new_str(s)
        })),
        "__hash__" => {
            let k = with_host(|h| h.to_key(recv))?;
            Ok(Value::Int(hash_key(&k)))
        }

        // --- comparison (declined operand → NotImplemented) ---
        "__eq__" | "__ne__" => {
            if !accepts(&b) {
                return ni();
            }
            let eq = with_host(|h| h.equal(recv, &b));
            Ok(Value::Bool(if name == "__eq__" { eq } else { !eq }))
        }
        "__lt__" | "__le__" | "__gt__" | "__ge__" => {
            if !accepts(&b) {
                return ni();
            }
            let op = match name {
                "__lt__" => NumOp::Lt,
                "__le__" => NumOp::Le,
                "__gt__" => NumOp::Gt,
                _ => NumOp::Ge,
            };
            with_host(|h| h.compare(op, recv, &b))
        }

        // --- forward binary arithmetic (self OP other) ---
        "__add__" | "__sub__" | "__mul__" => {
            if !accepts(&b) {
                return ni();
            }
            let op = match name {
                "__add__" => NumOp::Add,
                "__sub__" => NumOp::Sub,
                _ => NumOp::Mul,
            };
            with_host(|h| h.arith(op, recv, &b))
        }
        "__truediv__" | "__floordiv__" | "__mod__" | "__pow__" | "__and__" | "__or__"
        | "__xor__" | "__lshift__" | "__rshift__" => {
            if !accepts(&b) {
                return ni();
            }
            with_host(|h| h.binop(binop_dunder_tag(name), recv, &b))
        }
        "__divmod__" => {
            if !accepts(&b) {
                return ni();
            }
            let q = with_host(|h| h.binop(bo::FLOORDIV, recv, &b))?;
            let r = with_host(|h| h.binop(bo::MOD, recv, &b))?;
            Ok(with_host(|h| h.new_tuple(vec![q, r])))
        }

        // --- reflected binary arithmetic (other OP self) ---
        "__radd__" | "__rsub__" | "__rmul__" => {
            if !accepts(&b) {
                return ni();
            }
            let op = match name {
                "__radd__" => NumOp::Add,
                "__rsub__" => NumOp::Sub,
                _ => NumOp::Mul,
            };
            with_host(|h| h.arith(op, &b, recv))
        }
        "__rtruediv__" | "__rfloordiv__" | "__rmod__" | "__rpow__" | "__rand__" | "__ror__"
        | "__rxor__" | "__rlshift__" | "__rrshift__" => {
            if !accepts(&b) {
                return ni();
            }
            // Reflected name `__rNAME__` → forward `__NAME__` (drop the `r`).
            let fwd = format!("__{}", &name[3..]);
            with_host(|h| h.binop(binop_dunder_tag(&fwd), &b, recv))
        }
        "__rdivmod__" => {
            if !accepts(&b) {
                return ni();
            }
            let q = with_host(|h| h.binop(bo::FLOORDIV, &b, recv))?;
            let r = with_host(|h| h.binop(bo::MOD, &b, recv))?;
            Ok(with_host(|h| h.new_tuple(vec![q, r])))
        }
        _ => Err(format!("AttributeError: object has no attribute '{name}'")),
    }
}

/// The `host::binop` tag for a forward binary numeric dunder name.
fn binop_dunder_tag(name: &str) -> i64 {
    use host::binop as bo;
    match name {
        "__truediv__" => bo::DIV,
        "__floordiv__" => bo::FLOORDIV,
        "__mod__" => bo::MOD,
        "__pow__" => bo::POW,
        "__and__" => bo::BITAND,
        "__or__" => bo::BITOR,
        "__xor__" => bo::BITXOR,
        "__lshift__" => bo::SHL,
        "__rshift__" => bo::SHR,
        _ => bo::DIV,
    }
}

fn num_method(
    recv: &Value,
    name: &str,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value, String> {
    match name {
        "bit_length" => {
            // Magnitude bit count; faithful for both native `i64` and bignum ints.
            let bits = with_host(|h| h.big_val(recv))
                .map(|b| b.bits())
                .unwrap_or(0);
            Ok(Value::Int(bits as i64))
        }
        "bit_count" => {
            // Number of ones in the binary representation of abs(self).
            let ones: u32 = with_host(|h| h.big_val(recv))
                .map(|b| b.to_bytes_le().1.iter().map(|byte| byte.count_ones()).sum())
                .unwrap_or(0);
            Ok(Value::Int(ones as i64))
        }
        "to_bytes" => int_to_bytes(recv, args, kwargs),
        "is_integer" => Ok(Value::Bool(match recv {
            Value::Float(f) => f.fract() == 0.0,
            Value::Int(_) | Value::Bool(_) => true,
            _ => false,
        })),
        "as_integer_ratio" => {
            let (num, den) = match recv {
                Value::Float(f) => float_as_integer_ratio(*f)?,
                // int/bool: ratio is (self, 1).
                _ => (
                    with_host(|h| h.big_val(recv)).unwrap_or_default(),
                    num_bigint::BigInt::from(1),
                ),
            };
            let n = with_host(|h| h.norm_big(num));
            let d = with_host(|h| h.norm_big(den));
            Ok(with_host(|h| h.new_tuple(vec![n, d])))
        }
        "hex" => match recv {
            Value::Float(f) => Ok(new_str(float_hex(*f))),
            _ => Err(format!("AttributeError: object has no attribute '{name}'")),
        },
        "conjugate" => Ok(recv.clone()),
        _ => Err(format!("AttributeError: object has no attribute '{name}'")),
    }
}

/// `float.hex()` — the exact hexadecimal string of a float, e.g. `3.14` ->
/// `0x1.91eb851eb851fp+1`. Faithful to CPython's `float.__hex__`: 13 fraction
/// digits for finite values, a bare `0x0.0p+0` for zero, and `inf`/`nan` words.
fn float_hex(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-inf".into() } else { "inf".into() };
    }
    let bits = f.to_bits();
    let sign = if bits >> 63 == 1 { "-" } else { "" };
    let raw_exp = ((bits >> 52) & 0x7ff) as i64;
    let frac = bits & 0x000f_ffff_ffff_ffff;
    if raw_exp == 0 && frac == 0 {
        return format!("{sign}0x0.0p+0");
    }
    let (lead, exp) = if raw_exp == 0 {
        (0, -1022) // subnormal
    } else {
        (1, raw_exp - 1023)
    };
    // 52-bit fraction as exactly 13 lowercase hex digits.
    let digits = format!("{frac:013x}");
    let sign_char = if exp >= 0 { '+' } else { '-' };
    format!("{sign}0x{lead}.{digits}p{sign_char}{}", exp.abs())
}

/// `int.to_bytes(length=1, byteorder='big', *, signed=False)`. Faithful to
/// CPython: unsigned negatives and values too large for `length` raise
/// `OverflowError`; signed values use two's complement sign extension.
fn int_to_bytes(recv: &Value, args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let length = match args.first().cloned().or_else(|| kw_get(kwargs, "length")) {
        Some(v) => {
            let n = with_host(|h| h.as_int(&v))
                .ok_or_else(|| host::type_error("'length' must be an integer"))?;
            if n < 0 {
                return Err("ValueError: length argument must be non-negative".into());
            }
            n as usize
        }
        None => 1,
    };
    let byteorder = match args.get(1).cloned().or_else(|| kw_get(kwargs, "byteorder")) {
        Some(v) => with_host(|h| h.as_str(&v)).unwrap_or_default(),
        None => "big".into(),
    };
    let little = match byteorder.as_str() {
        "big" => false,
        "little" => true,
        _ => return Err("ValueError: byteorder must be either 'little' or 'big'".into()),
    };
    let signed = kw_get(kwargs, "signed")
        .map(|v| py_bool(&v).unwrap_or(false))
        .unwrap_or(false);
    let val = with_host(|h| h.big_val(recv)).unwrap_or_default();
    let mut be: Vec<u8> = if signed {
        let minimal = val.to_signed_bytes_be();
        if minimal.len() > length {
            return Err("OverflowError: int too big to convert".into());
        }
        // Sign-extend: 0xFF for negatives, 0x00 otherwise.
        let pad = if val.sign() == num_bigint::Sign::Minus {
            0xFF
        } else {
            0x00
        };
        let mut v = vec![pad; length - minimal.len()];
        v.extend_from_slice(&minimal);
        v
    } else {
        if val.sign() == num_bigint::Sign::Minus {
            return Err("OverflowError: can't convert negative int to unsigned".into());
        }
        // Magnitude, big-endian, leading zeros stripped (empty for 0).
        let minimal = val.to_bytes_be().1;
        let minimal: &[u8] = if minimal == [0] { &[] } else { &minimal };
        if minimal.len() > length {
            return Err("OverflowError: int too big to convert".into());
        }
        let mut v = vec![0u8; length - minimal.len()];
        v.extend_from_slice(minimal);
        v
    };
    if little {
        be.reverse();
    }
    Ok(with_host(|h| h.alloc(PyObj::Bytes(be))))
}

/// `float.fromhex(s)` — parse a hexadecimal float string (the inverse of
/// `float.hex`). Accepts an optional sign, an optional `0x`, a hex mantissa with
/// optional fraction, an optional `p<exp>` binary exponent, and the `inf`/`nan`
/// words. The value `mantissa * 2^exp` is formed exactly as a big rational and
/// rounded once to the nearest `f64`, matching CPython's correct rounding.
fn float_fromhex(s: &str) -> Result<f64, String> {
    let err = || format!("ValueError: invalid hexadecimal floating-point string: {s}");
    let t = s.trim();
    let low = t.to_ascii_lowercase();
    let (neg, rest) = match low.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, low.strip_prefix('+').unwrap_or(low.as_str())),
    };
    match rest {
        "inf" | "infinity" => {
            return Ok(if neg {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            })
        }
        "nan" => return Ok(f64::NAN),
        _ => {}
    }
    let body = rest.strip_prefix("0x").unwrap_or(rest);
    let (mant, exp_str) = match body.split_once('p') {
        Some((m, e)) => (m, Some(e)),
        None => (body, None),
    };
    let (int_part, frac_part) = match mant.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mant, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(err());
    }
    // Accumulate all hex digits into one big integer, tracking fraction width.
    let mut m = num_bigint::BigInt::from(0);
    for c in int_part.chars().chain(frac_part.chars()) {
        let d = c.to_digit(16).ok_or_else(err)?;
        m = m * 16 + d;
    }
    let p: i64 = match exp_str {
        Some(e) => e.parse().map_err(|_| err())?,
        None => 0,
    };
    // value = m * 2^(p - 4*frac_digits).
    let e = p - 4 * frac_part.len() as i64;
    let val = big_scaled_to_f64(&m, e);
    Ok(if neg { -val } else { val })
}

/// Round `m * 2^e` to the nearest `f64` (ties to even). Scaling an exactly-
/// rounded mantissa by a power of two is itself exact in the normal range, so
/// the single rounding happens when the big mantissa is narrowed to 53 bits.
fn big_scaled_to_f64(m: &num_bigint::BigInt, e: i64) -> f64 {
    use num_traits::ToPrimitive;
    if m.sign() == num_bigint::Sign::NoSign {
        return 0.0;
    }
    // Narrow the mantissa to at most 54 significant bits (53 + a round bit),
    // folding the discarded low bits into a sticky bit so the final `to_f64`
    // rounds to nearest-even correctly rather than double-rounding.
    let bits = m.bits() as i64;
    let mut m = m.clone();
    let mut e = e;
    let keep = 54;
    if bits > keep {
        let drop = bits - keep;
        let mask = (num_bigint::BigInt::from(1) << (drop as usize)) - 1;
        let low: num_bigint::BigInt = &m & &mask;
        m >>= drop as usize;
        // Sticky: OR a 1 into the lowest kept bit if any dropped bit was set,
        // preserving round-to-nearest-even semantics through the final scale.
        if low.sign() != num_bigint::Sign::NoSign
            && (&m & num_bigint::BigInt::from(1)).sign() == num_bigint::Sign::NoSign
        {
            m += 1;
        }
        e += drop;
    }
    let base = m.to_f64().unwrap_or(f64::INFINITY);
    // Scale by 2^e via ldexp-style exact power-of-two multiplication.
    ldexp(base, e)
}

/// `base * 2^exp` with `f64` semantics (over/underflow saturate as in CPython).
fn ldexp(base: f64, exp: i64) -> f64 {
    if base == 0.0 || !base.is_finite() {
        return base;
    }
    let mut b = base;
    let mut e = exp;
    // Step in chunks the exponent range can represent exactly (2^±1000).
    while e > 1000 {
        b *= 2f64.powi(1000);
        e -= 1000;
    }
    while e < -1000 {
        b *= 2f64.powi(-1000);
        e += 1000;
    }
    b * 2f64.powi(e as i32)
}

/// `int.from_bytes(bytes, byteorder='big', *, signed=False)` — build an int from
/// a bytes-like object or an iterable of ints. Faithful to CPython.
fn int_from_bytes(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    let src = args
        .first()
        .ok_or_else(|| host::type_error("from_bytes() missing required argument 'bytes'"))?;
    let mut bytes: Vec<u8> = match with_host(|h| match h.get(src) {
        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Some(b.clone()),
        _ => None,
    }) {
        Some(b) => b,
        None => {
            // Any iterable of ints in range(0, 256).
            let items = host::iter_vec(src)?;
            let mut v = Vec::with_capacity(items.len());
            for it in items {
                let n = with_host(|h| h.as_int(&it))
                    .ok_or_else(|| host::type_error("'bytes' must be an iterable of integers"))?;
                if !(0..=255).contains(&n) {
                    return Err("ValueError: bytes must be in range(0, 256)".into());
                }
                v.push(n as u8);
            }
            v
        }
    };
    let byteorder = match args.get(1).cloned().or_else(|| kw_get(kwargs, "byteorder")) {
        Some(v) => with_host(|h| h.as_str(&v)).unwrap_or_default(),
        None => "big".into(),
    };
    let little = match byteorder.as_str() {
        "big" => false,
        "little" => true,
        _ => return Err("ValueError: byteorder must be either 'little' or 'big'".into()),
    };
    let signed = kw_get(kwargs, "signed")
        .map(|v| py_bool(&v).unwrap_or(false))
        .unwrap_or(false);
    if little {
        bytes.reverse(); // normalize to big-endian for BigInt construction
    }
    let big = if signed {
        num_bigint::BigInt::from_signed_bytes_be(&bytes)
    } else {
        num_bigint::BigInt::from_bytes_be(num_bigint::Sign::Plus, &bytes)
    };
    Ok(with_host(|h| h.norm_big(big)))
}

/// `float.as_integer_ratio()` — the exact numerator/denominator whose quotient is
/// `f`. Raises on non-finite values, matching CPython.
fn float_as_integer_ratio(f: f64) -> Result<(num_bigint::BigInt, num_bigint::BigInt), String> {
    use num_bigint::BigInt;
    use num_integer::Integer;
    if f.is_nan() {
        return Err("ValueError: cannot convert NaN to integer ratio".into());
    }
    if f.is_infinite() {
        return Err("OverflowError: cannot convert Infinity to integer ratio".into());
    }
    if f == 0.0 {
        return Ok((BigInt::from(0), BigInt::from(1)));
    }
    let bits = f.to_bits();
    let sign: i8 = if bits >> 63 == 1 { -1 } else { 1 };
    let raw_exp = ((bits >> 52) & 0x7ff) as i64;
    let raw_mant = bits & 0x000f_ffff_ffff_ffff;
    // Reconstruct value == mant * 2^exp with mant an integer.
    let (mant, exp) = if raw_exp == 0 {
        (raw_mant, -1074) // subnormal
    } else {
        (raw_mant | 0x0010_0000_0000_0000, raw_exp - 1075)
    };
    let mut num = BigInt::from(mant) * sign;
    let mut den = BigInt::from(1);
    if exp >= 0 {
        num <<= exp as usize;
    } else {
        den <<= (-exp) as usize;
    }
    let g = num.gcd(&den);
    Ok((num / &g, den / &g))
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
        "__abs__" => with_host(|h| match h.get(recv) {
            Some(PyObj::Complex(r, i)) => Ok(Value::Float((r * r + i * i).sqrt())),
            _ => Err(host::type_error(
                "descriptor '__abs__' requires a 'complex'",
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
        Some(PyObj::Memoryview { .. }) => Some(h.mv_bytes(v)),
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
fn slice_find<T: PartialEq>(
    hay: &[T],
    needle: &[T],
    start: usize,
    end: usize,
    reverse: bool,
) -> Option<usize> {
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
fn count_range<T: PartialEq>(hay: &[T], needle: &[T], start: usize, end: usize) -> usize {
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
            match slice_find(hay, sep, 0, end, true) {
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
            match slice_find(hay, sep, start, hay.len(), false) {
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

/// Normalize a codec name the way CPython's alias table does for the encodings
/// we implement: lowercase, strip `-`/`_`/spaces. `UTF-8` / `utf_8` / `U8` all
/// collapse to `utf8`.
fn norm_codec(enc: &str) -> String {
    enc.to_lowercase().replace(['-', '_', ' '], "")
}

/// Apply an encode error handler to a single un-encodable code point, appending
/// its replacement bytes to `out`. Returns `Err` for `strict` (the caller turns
/// this into the `UnicodeEncodeError`). `codec` names the encoding for the error
/// text. Handlers: `strict`, `ignore`, `replace` (`?`), `backslashreplace`
/// (`\xHH`/`\uHHHH`/`\UHHHHHHHH`), `xmlcharrefreplace` (`&#NNN;`), `namereplace`
/// (`\N{NAME}`, falling back to `backslashreplace` when the char is unnamed).
fn encode_error(out: &mut Vec<u8>, c: char, errors: &str, codec: &str) -> Result<(), String> {
    match errors {
        "ignore" => Ok(()),
        "replace" => {
            out.push(b'?');
            Ok(())
        }
        "backslashreplace" => {
            let n = c as u32;
            let esc = if n <= 0xff {
                format!("\\x{n:02x}")
            } else if n <= 0xffff {
                format!("\\u{n:04x}")
            } else {
                format!("\\U{n:08x}")
            };
            out.extend_from_slice(esc.as_bytes());
            Ok(())
        }
        "xmlcharrefreplace" => {
            out.extend_from_slice(format!("&#{};", c as u32).as_bytes());
            Ok(())
        }
        "namereplace" => {
            match unicode_names2::name(c) {
                Some(name) => out.extend_from_slice(format!("\\N{{{name}}}").as_bytes()),
                None => return encode_error(out, c, "backslashreplace", codec),
            }
            Ok(())
        }
        _ => Err(format!(
            "UnicodeEncodeError: '{codec}' codec can't encode character '\\x{:x}'",
            c as u32
        )),
    }
}

/// Apply a decode error handler to one undecodable byte-run (`bad`), appending
/// the replacement to `out`. Matches CPython: `ignore` skips, `replace` emits a
/// single U+FFFD, `backslashreplace` emits `\xHH` per byte; `strict` raises
/// `UnicodeDecodeError`; the encode-only `namereplace`/`xmlcharrefreplace` raise
/// `TypeError` (CPython's "can't handle UnicodeDecodeError in error callback");
/// an unrecognized name raises `LookupError`. The caller invokes this once per
/// maximal error subpart, so `replace`'s U+FFFD count matches CPython.
fn decode_error(out: &mut String, bad: &[u8], errors: &str, codec: &str) -> Result<(), String> {
    match errors {
        "ignore" => Ok(()),
        "replace" => {
            out.push('\u{fffd}');
            Ok(())
        }
        "backslashreplace" => {
            for &b in bad {
                out.push_str(&format!("\\x{b:02x}"));
            }
            Ok(())
        }
        "strict" => Err(format!(
            "UnicodeDecodeError: '{codec}' codec can't decode byte"
        )),
        "namereplace" | "xmlcharrefreplace" => Err(
            "TypeError: don't know how to handle UnicodeDecodeError in error callback".to_string(),
        ),
        _ => Err(format!(
            "LookupError: unknown error handler name '{errors}'"
        )),
    }
}

/// CPython `str.encode(encoding, errors)`. Supports `utf-8` (default), `ascii`,
/// `latin-1`/`iso-8859-1`, and the `utf-16`/`utf-32` families (bare names emit a
/// BOM in native little-endian order; the explicit `-le`/`-be` names don't). The
/// error handler is only ever engaged by `ascii`/`latin-1` (a `str` holds only
/// valid scalar values, so every char round-trips through the Unicode codecs).
fn encode_str(s: &str, encoding: &str, errors: &str) -> Result<Vec<u8>, String> {
    let norm = norm_codec(encoding);
    match norm.as_str() {
        "utf8" | "u8" | "utf" | "cp65001" => Ok(s.as_bytes().to_vec()),
        "ascii" | "usascii" | "646" => {
            let mut out = Vec::with_capacity(s.len());
            for c in s.chars() {
                if (c as u32) < 0x80 {
                    out.push(c as u8);
                } else {
                    encode_error(&mut out, c, errors, "ascii")?;
                }
            }
            Ok(out)
        }
        "latin1" | "latin" | "iso88591" | "8859" | "cp819" | "l1" => {
            let mut out = Vec::with_capacity(s.len());
            for c in s.chars() {
                if (c as u32) <= 0xff {
                    out.push(c as u8);
                } else {
                    encode_error(&mut out, c, errors, "latin-1")?;
                }
            }
            Ok(out)
        }
        "utf16"
        | "utf16le"
        | "utf16be"
        | "u16"
        | "unicodelittleunmarked"
        | "unicodebigunmarked" => {
            let be = norm.ends_with("be") || norm == "unicodebigunmarked";
            let bom = matches!(norm.as_str(), "utf16" | "u16");
            let mut out = Vec::with_capacity(s.len() * 2 + 2);
            let push = |u: u16, out: &mut Vec<u8>| {
                let b = if be { u.to_be_bytes() } else { u.to_le_bytes() };
                out.extend_from_slice(&b);
            };
            if bom {
                push(0xfeff, &mut out);
            }
            let mut buf = [0u16; 2];
            for c in s.chars() {
                for &u in c.encode_utf16(&mut buf).iter() {
                    push(u, &mut out);
                }
            }
            Ok(out)
        }
        "utf32" | "utf32le" | "utf32be" | "u32" => {
            let be = norm.ends_with("be");
            let bom = matches!(norm.as_str(), "utf32" | "u32");
            let mut out = Vec::with_capacity(s.len() * 4 + 4);
            let push = |u: u32, out: &mut Vec<u8>| {
                let b = if be { u.to_be_bytes() } else { u.to_le_bytes() };
                out.extend_from_slice(&b);
            };
            if bom {
                push(0xfeff, &mut out);
            }
            for c in s.chars() {
                push(c as u32, &mut out);
            }
            Ok(out)
        }
        _ => Err(format!("LookupError: unknown encoding: {encoding}")),
    }
}

/// Decode bytes to a `str`. `utf-8` (default) and `latin-1` / `ascii` are
/// recognized. `errors` is honored for `strict` (default), `ignore`, and
/// `replace`; any other handler name falls back to strict.
fn decode_bytes(bytes: &[u8], args: &[Value]) -> Result<Value, String> {
    let enc = args
        .first()
        .and_then(|v| with_host(|h| h.as_str(v)))
        .unwrap_or_else(|| "utf-8".into());
    let errors = args
        .get(1)
        .and_then(|v| with_host(|h| h.as_str(v)))
        .unwrap_or_else(|| "strict".into());
    let norm = norm_codec(&enc);
    let s = match norm.as_str() {
        "latin1" | "latin" | "iso88591" | "l1" | "cp1252" | "8859" | "cp819" => {
            // Every byte maps to U+00..U+FF; no error handler is ever engaged.
            bytes.iter().map(|&b| b as char).collect::<String>()
        }
        "utf16" | "utf16le" | "utf16be" | "u16" => decode_utf16(bytes, &norm, &errors)?,
        "utf32" | "utf32le" | "utf32be" | "u32" => decode_utf32(bytes, &norm, &errors)?,
        "ascii" | "usascii" | "646" => {
            let mut out = String::with_capacity(bytes.len());
            for &b in bytes {
                if b < 0x80 {
                    out.push(b as char);
                } else {
                    decode_error(&mut out, &[b], &errors, "ascii")?;
                }
            }
            out
        }
        _ => utf8_decode_errors(bytes, &errors)?,
    };
    Ok(new_str(s))
}

/// Decode UTF-16. Bare `utf-16` consumes a leading BOM to pick endianness
/// (defaulting to little-endian, CPython's native order on the LE hosts this
/// targets); `utf-16-le`/`utf-16-be` are fixed-endian and never strip a BOM.
/// Odd trailing bytes and unpaired surrogates engage the error handler.
fn decode_utf16(bytes: &[u8], norm: &str, errors: &str) -> Result<String, String> {
    let mut be = norm.ends_with("be");
    let mut start = 0;
    if norm == "utf16" || norm == "u16" {
        if bytes.starts_with(&[0xff, 0xfe]) {
            start = 2;
        } else if bytes.starts_with(&[0xfe, 0xff]) {
            be = true;
            start = 2;
        }
    }
    let units: Vec<u16> = bytes[start..]
        .chunks_exact(2)
        .map(|c| {
            if be {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .collect();
    let trailing = (bytes.len() - start) % 2 != 0;
    let mut out = String::with_capacity(units.len());
    for r in char::decode_utf16(units) {
        match r {
            Ok(c) => out.push(c),
            Err(e) => {
                let u = e.unpaired_surrogate();
                let b = if be { u.to_be_bytes() } else { u.to_le_bytes() };
                decode_error(&mut out, &b, errors, "utf-16")?;
            }
        }
    }
    if trailing {
        decode_error(&mut out, &bytes[bytes.len() - 1..], errors, "utf-16")?;
    }
    Ok(out)
}

/// Decode UTF-32. Bare `utf-32` consumes a leading BOM (default little-endian);
/// `utf-32-le`/`utf-32-be` are fixed-endian. Out-of-range / surrogate / truncated
/// words engage the error handler.
fn decode_utf32(bytes: &[u8], norm: &str, errors: &str) -> Result<String, String> {
    let mut be = norm.ends_with("be");
    let mut start = 0;
    if norm == "utf32" || norm == "u32" {
        if bytes.starts_with(&[0xff, 0xfe, 0x00, 0x00]) {
            start = 4;
        } else if bytes.starts_with(&[0x00, 0x00, 0xfe, 0xff]) {
            be = true;
            start = 4;
        }
    }
    let body = &bytes[start..];
    let mut out = String::with_capacity(body.len() / 4);
    for c in body.chunks(4) {
        if c.len() < 4 {
            decode_error(&mut out, c, errors, "utf-32")?;
            break;
        }
        let n = if be {
            u32::from_be_bytes([c[0], c[1], c[2], c[3]])
        } else {
            u32::from_le_bytes([c[0], c[1], c[2], c[3]])
        };
        match char::from_u32(n) {
            Some(ch) => out.push(ch),
            None => decode_error(&mut out, c, errors, "utf-32")?,
        }
    }
    Ok(out)
}

/// UTF-8 decode with a CPython-compatible error handler. Invalid sequences are
/// handled per the "maximal subpart" rule (matching `std::str::from_utf8`'s
/// error offsets, which follow the same Unicode standard as CPython's decoder).
fn utf8_decode_errors(bytes: &[u8], errors: &str) -> Result<String, String> {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // SAFETY of unwrap: `rest[..valid]` is valid UTF-8 by definition.
                out.push_str(std::str::from_utf8(&rest[..valid]).unwrap());
                // `error_len() == None` means an incomplete sequence at the end:
                // CPython replaces/ignores it as a single unit.
                let skip = e.error_len().unwrap_or(rest.len() - valid);
                decode_error(&mut out, &rest[valid..valid + skip], errors, "utf-8")?;
                rest = &rest[valid + skip..];
                if rest.is_empty() {
                    break;
                }
            }
        }
    }
    Ok(out)
}

fn bytes_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    bytes_common_method(recv, false, name, args)
}

/// `memoryview` methods over the view's live bytes (`m.tobytes()`, `m.hex()`,
/// `m.tolist()`). `hex` reuses the `bytes` implementation (so `sep` /
/// `bytes_per_sep` work); `release` is a no-op in this owned-heap model.
fn memoryview_method(recv: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let bytes = with_host(|h| h.mv_bytes(recv));
    match name {
        "tobytes" => Ok(with_host(|h| h.alloc(PyObj::Bytes(bytes)))),
        "tolist" => Ok(with_host(|h| {
            let items = bytes.iter().map(|&b| Value::Int(b as i64)).collect();
            h.new_list(items)
        })),
        "hex" => {
            let tmp = with_host(|h| h.alloc(PyObj::Bytes(bytes)));
            bytes_method(&tmp, "hex", args)
        }
        "release" => Ok(Value::Undef),
        _ => Err(format!(
            "AttributeError: 'memoryview' object has no attribute '{name}'"
        )),
    }
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
        "insert" => {
            let idx_v = arg0(args)?;
            let val_v = args.get(1).cloned().unwrap_or(Value::Undef);
            let idx = with_host(|h| h.as_int(&idx_v)).unwrap_or(0);
            let n = with_host(|h| h.as_int(&val_v))
                .ok_or_else(|| host::type_error("an integer is required"))?;
            if !(0..=255).contains(&n) {
                return Err("ValueError: byte must be in range(0, 256)".into());
            }
            with_host(|h| {
                if let Some(PyObj::Bytearray(b)) = h.get_mut(recv) {
                    // CPython clamps the index like list.insert (negative from
                    // the end, out-of-range to [0, len]).
                    let len = b.len() as i64;
                    let i = (if idx < 0 { idx + len } else { idx }).clamp(0, len) as usize;
                    b.insert(i, n as u8);
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
        "reverse" => {
            with_host(|h| {
                if let Some(PyObj::Bytearray(b)) = h.get_mut(recv) {
                    b.reverse();
                }
            });
            Ok(Value::Undef)
        }
        "remove" => {
            let a0 = arg0(args)?;
            let n = with_host(|h| h.as_int(&a0)).unwrap_or(-1);
            let removed = with_host(|h| {
                if let Some(PyObj::Bytearray(b)) = h.get_mut(recv) {
                    if let Some(pos) = b.iter().position(|&x| x as i64 == n) {
                        b.remove(pos);
                        return true;
                    }
                }
                false
            });
            if removed {
                Ok(Value::Undef)
            } else {
                Err("ValueError: value not found in bytearray".into())
            }
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
        "hex" => {
            // `hex(sep=None, bytes_per_sep=1)`: with a separator, insert it every
            // `|bytes_per_sep|` bytes — grouping from the RIGHT for a positive
            // count, from the LEFT for a negative one (CPython's rule).
            match args.first() {
                None => Ok(new_str(bytes.iter().map(|b| format!("{b:02x}")).collect())),
                Some(sep_v) => {
                    let sep = with_host(|h| h.as_str(sep_v))
                        .ok_or_else(|| host::type_error("sep must be str or bytes"))?;
                    let group = args
                        .get(1)
                        .and_then(|v| with_host(|h| h.as_int(v)))
                        .unwrap_or(1);
                    if group == 0 {
                        return Err("ValueError: bytes_per_sep must not be zero".into());
                    }
                    let g = group.unsigned_abs() as usize;
                    let n = bytes.len();
                    let mut out = String::with_capacity(n * 2 + n);
                    for (i, b) in bytes.iter().enumerate() {
                        if i > 0 {
                            let boundary = if group > 0 {
                                (n - i) % g == 0
                            } else {
                                i % g == 0
                            };
                            if boundary {
                                out.push_str(&sep);
                            }
                        }
                        out.push_str(&format!("{b:02x}"));
                    }
                    Ok(new_str(out))
                }
            }
        }
        // `fromhex` is a classmethod but is also reachable through an instance.
        "fromhex" => Ok(mk_bytes(is_ba, bytes_fromhex(args)?)),
        "upper" => Ok(mk_bytes(is_ba, bytes.to_ascii_uppercase())),
        "lower" => Ok(mk_bytes(is_ba, bytes.to_ascii_lowercase())),
        "find" | "rfind" => {
            let sub = find_needle()?;
            let (start, end) = resolve_start_end(bytes.len(), args, 1);
            let p = slice_find(&bytes, &sub, start, end, name == "rfind");
            Ok(Value::Int(p.map(|x| x as i64).unwrap_or(-1)))
        }
        "index" | "rindex" => {
            let sub = find_needle()?;
            let (start, end) = resolve_start_end(bytes.len(), args, 1);
            match slice_find(&bytes, &sub, start, end, name == "rindex") {
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
            let (head, mid, tail) = match slice_find(&bytes, &sep, 0, bytes.len(), reverse) {
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
        // ASCII case swap: upper↔lower for a-z/A-Z, other bytes unchanged.
        "swapcase" => {
            let out: Vec<u8> = bytes
                .iter()
                .map(|&b| {
                    if b.is_ascii_uppercase() {
                        b.to_ascii_lowercase()
                    } else if b.is_ascii_lowercase() {
                        b.to_ascii_uppercase()
                    } else {
                        b
                    }
                })
                .collect();
            Ok(mk_bytes(is_ba, out))
        }
        // ASCII title-case: first ASCII letter of each run uppercased, rest lowered.
        "title" => {
            let mut out = Vec::with_capacity(bytes.len());
            let mut prev_alpha = false;
            for &b in &bytes {
                if b.is_ascii_alphabetic() {
                    out.push(if prev_alpha {
                        b.to_ascii_lowercase()
                    } else {
                        b.to_ascii_uppercase()
                    });
                    prev_alpha = true;
                } else {
                    out.push(b);
                    prev_alpha = false;
                }
            }
            Ok(mk_bytes(is_ba, out))
        }
        // ASCII capitalize: first byte uppercased, remaining bytes lowercased.
        "capitalize" => {
            let out: Vec<u8> = bytes
                .iter()
                .enumerate()
                .map(|(i, &b)| {
                    if i == 0 {
                        b.to_ascii_uppercase()
                    } else {
                        b.to_ascii_lowercase()
                    }
                })
                .collect();
            Ok(mk_bytes(is_ba, out))
        }
        // Left-pad with b'0' to `width`, keeping a leading b'+'/b'-' sign in front.
        "zfill" => {
            let w = args
                .first()
                .and_then(|v| with_host(|h| h.as_int(v)))
                .unwrap_or(0)
                .max(0) as usize;
            let out = if bytes.len() < w {
                let padn = w - bytes.len();
                let mut out = Vec::with_capacity(w);
                let mut rest = bytes.as_slice();
                if let Some((&first, tail)) = bytes.split_first() {
                    if first == b'+' || first == b'-' {
                        out.push(first);
                        rest = tail;
                    }
                }
                out.extend(std::iter::repeat(b'0').take(padn));
                out.extend_from_slice(rest);
                out
            } else {
                bytes.clone()
            };
            Ok(mk_bytes(is_ba, out))
        }
        // Expand b'\t' to the next `tabsize` stop; b'\n'/b'\r' reset the column.
        "expandtabs" => {
            let tabsize = args
                .first()
                .and_then(|v| with_host(|h| h.as_int(v)))
                .unwrap_or(8)
                .max(0) as usize;
            Ok(mk_bytes(is_ba, expand_tabs_bytes(&bytes, tabsize)))
        }
        "center" => Ok(mk_bytes(is_ba, pad_bytes(&bytes, args, 'c')?)),
        "ljust" => Ok(mk_bytes(is_ba, pad_bytes(&bytes, args, 'l')?)),
        "rjust" => Ok(mk_bytes(is_ba, pad_bytes(&bytes, args, 'r')?)),
        "translate" => Ok(mk_bytes(is_ba, bytes_translate(&bytes, args)?)),
        // Reachable via an instance (`b''.maketrans(...)`); always returns bytes.
        "maketrans" => bytes_maketrans(args),
        "isalpha" => Ok(Value::Bool(
            !bytes.is_empty() && bytes.iter().all(|b| b.is_ascii_alphabetic()),
        )),
        "isdigit" => Ok(Value::Bool(
            !bytes.is_empty() && bytes.iter().all(|b| b.is_ascii_digit()),
        )),
        "isalnum" => Ok(Value::Bool(
            !bytes.is_empty() && bytes.iter().all(|b| b.is_ascii_alphanumeric()),
        )),
        "isspace" => Ok(Value::Bool(
            !bytes.is_empty() && bytes.iter().all(|&b| is_ascii_ws(b)),
        )),
        "isupper" => Ok(Value::Bool(
            bytes.iter().any(|b| b.is_ascii_alphabetic())
                && !bytes.iter().any(|b| b.is_ascii_lowercase()),
        )),
        "islower" => Ok(Value::Bool(
            bytes.iter().any(|b| b.is_ascii_alphabetic())
                && !bytes.iter().any(|b| b.is_ascii_uppercase()),
        )),
        "istitle" => Ok(Value::Bool(is_bytes_titlecased(&bytes))),
        "isascii" => Ok(Value::Bool(bytes.iter().all(|&b| b < 0x80))),
        _ => Err(format!(
            "AttributeError: '{tname}' object has no attribute '{name}'"
        )),
    }
}

/// `bytes.center/ljust/rjust(width[, fillbyte])`. `fillbyte` is a length-1
/// bytes-like (default a space); pads to `width` bytes.
fn pad_bytes(bytes: &[u8], args: &[Value], mode: char) -> Result<Vec<u8>, String> {
    let w = args
        .first()
        .and_then(|v| with_host(|h| h.as_int(v)))
        .unwrap_or(0)
        .max(0) as usize;
    let fill: u8 = match args.get(1) {
        None | Some(Value::Undef) => b' ',
        Some(v) => {
            let fb = as_bytes_object(v).ok_or_else(|| {
                host::type_error(&format!(
                    "{}() argument 2 must be a byte string of length 1, not {}",
                    match mode {
                        'l' => "ljust",
                        'r' => "rjust",
                        _ => "center",
                    },
                    with_host(|h| h.type_name(v))
                ))
            })?;
            if fb.len() != 1 {
                return Err(host::type_error(&format!(
                    "{}(): argument 2 must be a byte string of length 1, not a bytes object of length {}",
                    match mode {
                        'l' => "ljust",
                        'r' => "rjust",
                        _ => "center",
                    },
                    fb.len()
                )));
            }
            fb[0]
        }
    };
    if bytes.len() >= w {
        return Ok(bytes.to_vec());
    }
    let total = w - bytes.len();
    let mut out = Vec::with_capacity(w);
    match mode {
        'l' => {
            out.extend_from_slice(bytes);
            out.extend(std::iter::repeat(fill).take(total));
        }
        'r' => {
            out.extend(std::iter::repeat(fill).take(total));
            out.extend_from_slice(bytes);
        }
        _ => {
            // CPython `center`: the extra byte on an odd margin favors the left
            // when `margin` and `width` are both odd (`marg/2 + (marg & width & 1)`).
            let left = total / 2 + (total & w & 1);
            let right = total - left;
            out.extend(std::iter::repeat(fill).take(left));
            out.extend_from_slice(bytes);
            out.extend(std::iter::repeat(fill).take(right));
        }
    }
    Ok(out)
}

/// `bytes.translate(table[, delete])`. `table` is a 256-byte remap table (or
/// `None` for identity); `delete` names bytes to drop. Deletion is tested
/// against the original byte, then the table is applied.
fn bytes_translate(bytes: &[u8], args: &[Value]) -> Result<Vec<u8>, String> {
    let table: Option<Vec<u8>> = match args.first() {
        None | Some(Value::Undef) => None,
        Some(v) => {
            let t = as_bytes_object(v)
                .ok_or_else(|| host::type_error("a bytes-like object is required"))?;
            if t.len() != 256 {
                return Err("ValueError: translation table must be 256 characters long".into());
            }
            Some(t)
        }
    };
    let delete: Vec<u8> = match args.get(1) {
        None | Some(Value::Undef) => Vec::new(),
        Some(v) => {
            as_bytes_object(v).ok_or_else(|| host::type_error("a bytes-like object is required"))?
        }
    };
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if delete.contains(&b) {
            continue;
        }
        out.push(match &table {
            Some(t) => t[b as usize],
            None => b,
        });
    }
    Ok(out)
}

/// `bytes.maketrans(from, to)` — a 256-byte translation table mapping each byte
/// of `from` to the corresponding byte of `to`; always returns `bytes`.
fn bytes_maketrans(args: &[Value]) -> Result<Value, String> {
    let frm = args
        .first()
        .and_then(as_bytes_object)
        .ok_or_else(|| host::type_error("a bytes-like object is required"))?;
    let to = args
        .get(1)
        .and_then(as_bytes_object)
        .ok_or_else(|| host::type_error("a bytes-like object is required"))?;
    if frm.len() != to.len() {
        return Err("ValueError: maketrans arguments must have same length".into());
    }
    let mut table: Vec<u8> = (0..=255).collect();
    for (f, t) in frm.iter().zip(to.iter()) {
        table[*f as usize] = *t;
    }
    Ok(with_host(|h| h.alloc(PyObj::Bytes(table))))
}

/// ASCII `bytes.istitle()`: at least one cased byte, uppercase only at run
/// starts, lowercase only after a cased byte.
fn is_bytes_titlecased(bytes: &[u8]) -> bool {
    let mut cased = false;
    let mut prev_cased = false;
    for &b in bytes {
        if b.is_ascii_uppercase() {
            if prev_cased {
                return false;
            }
            prev_cased = true;
            cased = true;
        } else if b.is_ascii_lowercase() {
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
    kwargs: &[(String, Value)],
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
        ("OrderedDict", "move_to_end") => Some(ordered_move_to_end(recv, args, kwargs)),
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
/// Counter multiset operators — `+`/`-`/`&`/`|`, keeping only positive counts
/// (CPython semantics; result order is the first operand's keys then the
/// second's). Returns `None` unless both operands are `Counter`s, so plain
/// dict/set `|`/`&` and int arithmetic fall through.
fn counter_binop(a: &Value, b: &Value, op: char) -> Option<Result<Value, String>> {
    let is_counter =
        |v: &Value| host::dict_meta_of(v).map(|m| m.kind) == Some(host::DictKind::Counter);
    if !(is_counter(a) && is_counter(b)) {
        return None;
    }
    let dump = |v: &Value| -> Vec<(PKey, Value, i64)> {
        with_host(|h| match h.get(v) {
            Some(PyObj::Dict(m)) => m
                .iter()
                .map(|(k, (kv, cnt))| (k.clone(), kv.clone(), h.as_int(cnt).unwrap_or(0)))
                .collect(),
            _ => Vec::new(),
        })
    };
    let da = dump(a);
    let db = dump(b);
    let get = |d: &[(PKey, Value, i64)], k: &PKey| {
        d.iter()
            .find(|(kk, _, _)| kk == k)
            .map(|(_, _, c)| *c)
            .unwrap_or(0)
    };
    // Union of keys: first operand's order, then second-only keys.
    let mut keys: Vec<(PKey, Value)> = da
        .iter()
        .map(|(k, kv, _)| (k.clone(), kv.clone()))
        .collect();
    for (k, kv, _) in &db {
        if !da.iter().any(|(kk, _, _)| kk == k) {
            keys.push((k.clone(), kv.clone()));
        }
    }
    let mut out: IndexMap<PKey, (Value, Value)> = IndexMap::new();
    for (k, kv) in keys {
        let (x, y) = (get(&da, &k), get(&db, &k));
        let val = match op {
            '+' => x + y,
            '-' => x - y,
            '&' => x.min(y),
            '|' => x.max(y),
            _ => return None,
        };
        if val > 0 {
            out.insert(k, (kv, Value::Int(val)));
        }
    }
    Some(Ok(host::alloc_dict_subtype(
        out,
        host::DictKind::Counter,
        None,
    )))
}

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

/// `OrderedDict.move_to_end(key, last=True)`. `last` may be positional or keyword.
fn ordered_move_to_end(
    recv: &Value,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value, String> {
    let kv = arg0(args)?;
    let key = with_host(|h| h.to_key(&kv))?;
    let last = args
        .get(1)
        .or_else(|| kwargs.iter().find(|(k, _)| k == "last").map(|(_, v)| v))
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
        return Err(with_host(|h| h.key_error(&kv)));
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
            // `deque([iterable[, maxlen]])` — both are also acceptable by keyword
            // (`deque([1,2,3], maxlen=4)`), so consult kwargs when a positional is
            // absent.
            let kw = |name: &str| {
                kwargs
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            };
            let iterable = match args.first() {
                Some(v) if !matches!(v, Value::Undef) => Some(v.clone()),
                _ => kw("iterable"),
            };
            let mut items: std::collections::VecDeque<Value> = match iterable {
                Some(v) if !matches!(v, Value::Undef) => {
                    std::collections::VecDeque::from(host::iter_vec(&v)?)
                }
                _ => std::collections::VecDeque::new(),
            };
            let maxlen_val = match args.get(1) {
                Some(v) if !matches!(v, Value::Undef) => Some(v.clone()),
                _ => kw("maxlen"),
            };
            let maxlen = match maxlen_val {
                Some(v) if !matches!(v, Value::Undef) => {
                    with_host(|h| h.as_int(&v)).map(|n| n.max(0) as usize)
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
/// CPython's spelling of a non-finite float inside a numeric format type:
/// lowercase `nan`/`inf`/`-inf` for `f`/`e`/`g`/`%`, uppercase for `F`/`E`/`G`.
/// `None` for a finite value. The returned body still flows through the shared
/// sign/width/zero-fill logic (so `{inf:08.2f}` → `00000inf`, like CPython).
fn fmt_nonfinite(f: f64, upper: bool) -> Option<String> {
    if f.is_nan() {
        Some(if upper { "NAN" } else { "nan" }.to_string())
    } else if f.is_infinite() {
        let s = if upper { "INF" } else { "inf" };
        Some(if f < 0.0 {
            format!("-{s}")
        } else {
            s.to_string()
        })
    } else {
        None
    }
}

pub fn apply_format_spec(s: &str, v: &Value, spec: &str) -> Result<String, String> {
    if spec.is_empty() {
        return Ok(s.to_string());
    }
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let mut fill = ' ';
    let mut align = '\0';
    // Whether alignment was written explicitly (`<`/`>`/`^`/`=`), as opposed to
    // being implied by the `0` flag — CPython rejects an explicit `=` on strings
    // but accepts the `0`-implied form.
    let mut align_explicit = false;
    // [[fill]align]
    if chars.len() >= 2 && matches!(chars[1], '<' | '>' | '^' | '=') {
        fill = chars[0];
        align = chars[1];
        align_explicit = true;
        i = 2;
    } else if !chars.is_empty() && matches!(chars[0], '<' | '>' | '^' | '=') {
        align = chars[0];
        align_explicit = true;
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
    // Grouping option: `,` (thousands) or `_` (underscore). CPython places this
    // between the width and the `.precision`; both are parsed here so `_x` etc.
    // never collide with the trailing type char.
    let group: Option<char> = match chars.get(i).copied() {
        Some(c @ (',' | '_')) => {
            i += 1;
            Some(c)
        }
        _ => None,
    };
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
    // An int-like value (`int`/`bool`/bignum) with no explicit presentation type
    // formats as a decimal integer, so `format(False, "5")` is `    0` (the int
    // value) rather than the string `"False"`.
    let ty = if ty == '\0' && is_int_like(v) {
        'd'
    } else {
        ty
    };

    validate_format_spec(v, ty, sign, alt, group, prec, align, align_explicit)?;

    // Render body by type.
    let body =
        match ty {
            'd' => match with_host(|h| h.big_val(v)) {
                Some(n) => n.to_string(),
                None => s.to_string(),
            },
            'f' | 'F' => {
                let f = as_f(v).unwrap_or(0.0);
                fmt_nonfinite(f, ty == 'F')
                    .unwrap_or_else(|| format!("{:.*}", prec.unwrap_or(6), f))
            }
            'e' | 'E' => {
                let f = as_f(v).unwrap_or(0.0);
                fmt_nonfinite(f, ty == 'E')
                    .unwrap_or_else(|| crate::host::fmt_sci(f, prec.unwrap_or(6), ty == 'E'))
            }
            'g' | 'G' => {
                let f = as_f(v).unwrap_or(0.0);
                fmt_nonfinite(f, ty == 'G')
                    .unwrap_or_else(|| crate::host::fmt_g(f, prec.unwrap_or(6), ty == 'G', alt))
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
                let f = as_f(v).unwrap_or(0.0);
                match fmt_nonfinite(f, false) {
                    Some(s) => format!("{s}%"),
                    None => format!("{:.*}%", prec.unwrap_or(6), f * 100.0),
                }
            }
            _ => {
                let mut body = s.to_string();
                if let Some(p) = prec {
                    if matches!(v, Value::Str(_)) || is_str(v) {
                        body = body.chars().take(p).collect();
                    } else if let Some(f) = as_f(v) {
                        // A float with a precision but no presentation type uses
                        // CPython's "general" float format, NOT fixed-point.
                        body = fmt_nonfinite(f, false)
                            .unwrap_or_else(|| crate::host::fmt_none_float(f, p));
                    }
                }
                body
            }
        };

    // `as_f` narrows to f64 and returns `None` for a bignum, so it can't be the
    // sole numeric test: an int above `i64::MAX` is still numeric and must keep
    // grouping, the `+`/space sign flag, and `0`-fill interleave. `is_int_like`
    // covers the `PyObj::BigInt` case that `as_f` drops.
    let numeric = as_f(v).is_some() || is_int_like(v);
    // Decompose the numeric body into sign / radix-prefix / integer-digits /
    // trailing (fraction or exponent), so grouping and sign-aware zero-fill can
    // operate on just the integer digits — exactly like CPython.
    let mut sign_str = String::new();
    let mut rest = body.as_str();
    if let Some(r) = rest.strip_prefix('-') {
        sign_str.push('-');
        rest = r;
    } else if numeric && matches!(sign, '+' | ' ') {
        // A `+`/space sign flag adds a leading marker to a non-negative value.
        sign_str.push(sign);
    }
    let mut prefix = String::new();
    if rest.len() >= 2 && rest.as_bytes()[0] == b'0' {
        let c1 = rest.as_bytes()[1] as char;
        if matches!(c1, 'x' | 'X' | 'o' | 'O' | 'b' | 'B') {
            prefix = rest[..2].to_string();
            rest = &rest[2..];
        }
    }
    // Split off the fraction / exponent so grouping touches only the integer
    // digits. `e`/`E` are exponent markers ONLY for the float-exp types — for a
    // hex value like `3e517` they are ordinary digits.
    let split: &[char] = if matches!(ty, 'e' | 'E' | 'g' | 'G') {
        &['.', 'e', 'E']
    } else {
        &['.']
    };
    let (intpart, tail): (&str, &str) = match rest.find(split) {
        Some(p) => (&rest[..p], &rest[p..]),
        None => (rest, ""),
    };

    // Grouping size: `_` groups hex/oct/bin by 4; `,` and decimal/float `_`
    // group by 3. Non-numeric bodies never group.
    let group_size = match (group, ty) {
        (Some(_), _) if !numeric => 0,
        (Some('_'), 'x' | 'X' | 'o' | 'b') => 4,
        (Some(_), _) => 3,
        (None, _) => 0,
    };
    let sep = group.unwrap_or(',');

    // Sign-aware zero-fill only interleaves separators when the fill is '0' and
    // the alignment is '=' (the `0` flag). Any other fill/align groups the
    // natural digits first, then pads as an opaque block.
    let zero_interleave = align == '=' && fill == '0';

    if numeric && group_size > 0 && zero_interleave {
        // Grow the integer-digit count until sign + prefix + grouped(n) + tail
        // reaches the width; leading zeros pad the magnitude, then group.
        let fixed = sign_str.chars().count() + prefix.chars().count() + tail.chars().count();
        let mut n = intpart.chars().count().max(1);
        while fixed + grouped_len(n, group_size) < width {
            n += 1;
        }
        let padded = format!("{:0>n$}", intpart, n = n);
        let grouped = insert_grouping(&padded, sep, group_size);
        return Ok(format!("{sign_str}{prefix}{grouped}{tail}"));
    }

    // Group the natural integer digits (no interleaved padding).
    let intpart_g = if numeric && group_size > 0 {
        insert_grouping(intpart, sep, group_size)
    } else {
        intpart.to_string()
    };
    let body = format!("{sign_str}{prefix}{intpart_g}{tail}");

    let len = body.chars().count();
    if len >= width {
        return Ok(body);
    }
    let pad = width - len;
    Ok(match align {
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
        // A non-numeric body only reaches `=` via the `0` flag (explicit `=` on a
        // string is rejected in validation); CPython keeps the string's default
        // left alignment there, so `{:05s}` of "hi" → "hi000".
        '=' if !numeric => format!("{body}{}", fill.to_string().repeat(pad)),
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
            if numeric {
                format!("{}{body}", fill.to_string().repeat(pad))
            } else {
                format!("{body}{}", fill.to_string().repeat(pad))
            }
        }
    })
}

/// Validate a parsed format spec against the value's type, matching CPython's
/// `str`/`int`/`float` `__format__` errors. Parity is on success-ness (a raised
/// `ValueError`/`OverflowError`/`TypeError`), so only the error CONDITION must
/// match CPython — the exact message text is best-effort.
#[allow(clippy::too_many_arguments)]
fn validate_format_spec(
    v: &Value,
    ty: char,
    sign: char,
    alt: bool,
    group: Option<char>,
    prec: Option<usize>,
    align: char,
    align_explicit: bool,
) -> Result<(), String> {
    let is_float = matches!(v, Value::Float(_));
    let is_int = !is_float && with_host(|h| h.big_val(v)).is_some();
    let is_num = is_float || is_int;
    let is_string = !is_num && is_str(v);
    let tyname = || with_host(|h| h.type_name(v));

    // Non-numeric, non-string object with a (non-empty) spec has no custom
    // __format__ here: object.__format__ rejects any format string.
    if !is_num && !is_string {
        return Err(format!(
            "TypeError: unsupported format string passed to {}.__format__",
            tyname()
        ));
    }

    // String value: only '' and 's' types; sign / '#' / explicit '=' / grouping
    // are all rejected.
    if is_string {
        if ty != '\0' && ty != 's' {
            return Err(format!(
                "ValueError: Unknown format code '{ty}' for object of type 'str'"
            ));
        }
        if sign == ' ' {
            return Err("ValueError: Space not allowed in string format specifier".into());
        }
        if sign != '\0' {
            return Err("ValueError: Sign not allowed in string format specifier".into());
        }
        if alt {
            return Err(
                "ValueError: Alternate form (#) not allowed in string format specifier".into(),
            );
        }
        if align_explicit && align == '=' {
            return Err("ValueError: '=' alignment not allowed in string format specifier".into());
        }
        if let Some(g) = group {
            return Err(format!("ValueError: Cannot specify '{g}' with 's'."));
        }
        return Ok(());
    }

    // 's' requires a string value.
    if ty == 's' {
        return Err(format!(
            "ValueError: Unknown format code 's' for object of type '{}'",
            tyname()
        ));
    }

    // Integer-only presentation types reject a float value.
    if is_float && matches!(ty, 'b' | 'o' | 'x' | 'X' | 'c' | 'd') {
        return Err(format!(
            "ValueError: Unknown format code '{ty}' for object of type 'float'"
        ));
    }

    // Grouping-vs-type: ',' is illegal with the radix / char / locale types;
    // '_' is illegal only with 'c'.
    match group {
        Some(',') if matches!(ty, 'x' | 'X' | 'o' | 'b' | 'c' | 'n') => {
            return Err("ValueError: Cannot specify ',' with '?'.".replace('?', &ty.to_string()));
        }
        Some('_') if ty == 'c' => {
            return Err("ValueError: Cannot specify '_' with 'c'.".into());
        }
        _ => {}
    }

    // Precision is not allowed when the effective type is an integer type.
    if is_int && prec.is_some() && matches!(ty, '\0' | 'b' | 'o' | 'x' | 'X' | 'c' | 'd') {
        return Err("ValueError: Precision not allowed in integer format specifier".into());
    }

    // 'c' codepoint must be in range(0x110000).
    if ty == 'c' && is_int {
        if let Some(n) = with_host(|h| h.big_val(v)) {
            use num_bigint::BigInt;
            if n < BigInt::from(0) || n > BigInt::from(0x10_FFFFu32) {
                return Err("OverflowError: %c arg not in range(0x110000)".into());
            }
        }
    }

    Ok(())
}

/// Length of `n` digits after inserting a group separator every `size` digits
/// from the right: `n + (n-1)/size`.
fn grouped_len(n: usize, size: usize) -> usize {
    if n == 0 {
        0
    } else {
        n + (n - 1) / size
    }
}

/// Insert `sep` every `size` characters of `digits`, counting from the right
/// (e.g. `insert_grouping("1234567", ',', 3)` → `"1,234,567"`).
fn insert_grouping(digits: &str, sep: char, size: usize) -> String {
    let cs: Vec<char> = digits.chars().collect();
    let n = cs.len();
    if n == 0 || size == 0 {
        return digits.to_string();
    }
    let mut out = String::with_capacity(n + n / size);
    for (idx, c) in cs.iter().enumerate() {
        if idx > 0 && (n - idx) % size == 0 {
            out.push(sep);
        }
        out.push(*c);
    }
    out
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
