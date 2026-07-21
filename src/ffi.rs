//! CPython stdlib FFI bridge (feature `stdlib-ffi`).
//!
//! pythonrs does not reimplement the standard library. When this feature is on,
//! `import <stdlib>` delegates to an embedded libpython over pyo3, so user code
//! gets the *real* CPython stdlib — pure `.py` modules **and** the C accelerators
//! (`_sre`, `_hashlib`, `_datetime`, `_json`, …). User code still runs on fusevm;
//! only the imported stdlib objects live on the CPython side.
//!
//! A stdlib object that pythonrs can represent by value (int/float/bool/None/str/
//! bytes/bytearray/list/tuple/dict/set/frozenset/range/complex/`deque`) is
//! marshaled across the boundary in both directions. Everything else (compiled
//! regex, `datetime`, sockets, file objects, iterators, …) stays on the CPython
//! side behind a [`PyObj::Foreign`] handle: an index
//! into the side-table below. Attribute access, calls, indexing, iteration,
//! `len`, `str`/`repr`, and membership on a `Foreign` route back through here;
//! pyo3 owns the refcounts and the GIL.
//!
//! A by-value mutable-container argument (`list`/`bytearray`/`deque`) is copied
//! into a fresh CPython object, so an in-place stdlib mutator (`heapq.heapify`,
//! `random.shuffle`, `struct.pack_into`) would otherwise lose its effect; after
//! the call the bridge re-reads that object and overwrites the pythonrs heap slot
//! in place (see `writeback_mutated_args`) so the mutation — and aliases to the
//! same object — reflect it. Write-back marshals by value only and never
//! allocates a `Foreign`, so it does not grow the side-table.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDict, PyFloat, PyFrozenSet, PyInt, PyList, PySet, PyString,
    PyTuple,
};
use pyo3::IntoPyObjectExt;

use crate::host::{with_host, PyHost, PyObj};
use fusevm::Value;

/// Side-table of live CPython objects, indexed by the `u32` carried in a
/// `PyObj::Foreign`. Entries are never freed for the process lifetime — stdlib
/// objects (modules, compiled patterns) are effectively permanent, and pyo3's
/// `Py<PyAny>` keeps each alive across GIL drops.
static TABLE: OnceLock<Mutex<Vec<Py<PyAny>>>> = OnceLock::new();

fn table() -> &'static Mutex<Vec<Py<PyAny>>> {
    TABLE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Resolve the CPython prefix to hand to `PYTHONHOME`, or `None` to let the
/// linked libpython locate its own stdlib (the system-CPython path).
///
/// Order: `PYTHONRS_STDLIB` env → bundled `<exe_dir>/../lib/python3.14` → system.
fn resolve_home() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PYTHONRS_STDLIB") {
        return Some(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // The stdlib is `<prefix>/lib/python3.14`; `PYTHONHOME` wants the
            // prefix, so a bundled tree next to the binary makes home `<exe>/..`.
            if dir.join("../lib/python3.14").is_dir() {
                return dir.parent().map(PathBuf::from);
            }
        }
    }
    None
}

/// Initialize the embedded interpreter once, after pinning `PYTHONHOME` so the
/// stdlib resolves to the intended (bundled or system) tree. Idempotent.
pub fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Some(home) = resolve_home() {
            // Set before the interpreter starts — CPython reads it at init only.
            std::env::set_var("PYTHONHOME", home);
        }
        pyo3::prepare_freethreaded_python();
    });
}

/// Store a CPython object in the side-table and hand back its `Foreign` id.
fn store(obj: Py<PyAny>) -> u32 {
    let mut t = table().lock().expect("ffi table poisoned");
    t.push(obj);
    (t.len() - 1) as u32
}

/// Number of live entries in the side-table. Diagnostic only (used by the
/// bridge's own tests to assert bounded growth).
pub fn table_len() -> usize {
    table().lock().map(|t| t.len()).unwrap_or(0)
}

/// A fresh owned handle to the side-table object `id`, bound to `py`.
fn fetch<'py>(py: Python<'py>, id: u32) -> Result<Bound<'py, PyAny>, String> {
    let t = table().lock().expect("ffi table poisoned");
    match t.get(id as usize) {
        Some(obj) => Ok(obj.clone_ref(py).into_bound(py)),
        None => Err(format!("ffi: invalid foreign handle {id}")),
    }
}

/// Import `name` (possibly dotted, e.g. `os.path`) via CPython's own importer and
/// return a `Foreign` handle to the module object.
pub fn import(name: &str) -> Result<u32, String> {
    init();
    Python::with_gil(|py| match py.import(name) {
        Ok(module) => Ok(store(module.into_any().unbind())),
        Err(e) => Err(e.to_string()),
    })
}

/// Whether two `Foreign` handles point at the *same* CPython object (`is`
/// identity). Enum members and other CPython singletons compare equal under `is`
/// even when fetched into distinct handles.
pub fn same_object(a: u32, b: u32) -> bool {
    Python::with_gil(|py| match (fetch(py, a), fetch(py, b)) {
        (Ok(x), Ok(y)) => x.is(&y),
        _ => false,
    })
}

/// Create a class with foreign (CPython) bases via CPython's own class machinery
/// (`class C(enum.Enum): A = 1` → `EnumType`). `types.new_class` computes the
/// metaclass, fires `__prepare__`, and the body populates the prepared namespace
/// one key at a time — so a metaclass namespace like Enum's `_EnumDict` records
/// each member through `__setitem__`. Returns a `Foreign` handle to the class.
pub fn build_foreign_class(
    name: &str,
    bases: &[Value],
    members: &[(String, Value)],
) -> Result<Value, String> {
    init();
    Python::with_gil(|py| {
        // Marshal bases + members under a short host borrow; the metaclass call
        // runs with none held (a method body may re-enter fusevm).
        let (bases_tuple, members_dict): (Bound<PyAny>, Bound<PyAny>) =
            with_host(|h| -> Result<_, String> {
                let base_objs: Vec<Bound<PyAny>> = bases
                    .iter()
                    .map(|b| value_to_py(h, py, b))
                    .collect::<Result<_, _>>()?;
                let bases_tuple = PyTuple::new(py, &base_objs).map_err(|e| e.to_string())?;
                let members_dict = PyDict::new(py);
                for (k, v) in members {
                    let pv = value_to_py(h, py, v)?;
                    members_dict
                        .set_item(k.as_str(), pv)
                        .map_err(|e| e.to_string())?;
                }
                Ok((bases_tuple.into_any(), members_dict.into_any()))
            })?;
        let helper = make_class_helper(py)?;
        let cls = helper
            .call1((name, bases_tuple, members_dict))
            .map_err(|e| e.to_string())?;
        let id = store(cls.unbind());
        Ok(with_host(|h| h.alloc(PyObj::Foreign(id))))
    })
}

/// The cached `_make(name, bases, members)` helper (built via `types.new_class`),
/// which populates the metaclass-prepared namespace one key at a time so
/// `_EnumDict.__setitem__` — and any other metaclass namespace — sees each key.
fn make_class_helper(py: Python) -> Result<Bound<PyAny>, String> {
    static MAKE_CLASS: OnceLock<Py<PyAny>> = OnceLock::new();
    if let Some(f) = MAKE_CLASS.get() {
        return Ok(f.bind(py).clone());
    }
    let code = cr#"
import types
def _make(name, bases, members):
    def body(ns):
        for k in members:
            ns[k] = members[k]
    return types.new_class(name, tuple(bases), {}, body)
"#;
    let module = PyModule::from_code(py, code, c"_pyrs_class.py", c"_pyrs_class")
        .map_err(|e| e.to_string())?;
    let f = module.getattr("_make").map_err(|e| e.to_string())?;
    let _ = MAKE_CLASS.set(f.clone().unbind());
    Ok(f)
}

// ── marshaling: pythonrs Value ↔ CPython object ──────────────────────────────

/// pythonrs `Value` → CPython object. By value for the representable types;
/// a `Foreign` handle passes the underlying CPython object straight through.
fn value_to_py<'py>(
    host: &PyHost,
    py: Python<'py>,
    v: &Value,
) -> Result<Bound<'py, PyAny>, String> {
    let conv = |b: Result<Bound<'py, PyAny>, PyErr>| b.map_err(|e| e.to_string());
    match v {
        Value::Undef => Ok(py.None().into_bound(py)),
        Value::Bool(b) => conv(b.into_bound_py_any(py)),
        Value::Int(n) => conv(n.into_bound_py_any(py)),
        Value::Float(f) => conv(f.into_bound_py_any(py)),
        Value::Str(s) => conv(s.as_str().into_bound_py_any(py)),
        Value::Obj(_) => match host.get(v) {
            Some(PyObj::Str(s)) => conv(s.as_str().into_bound_py_any(py)),
            Some(PyObj::Bytes(b)) => Ok(PyBytes::new(py, b).into_any()),
            // A `bytearray` is mutable, so it crosses as a CPython `bytearray`
            // (not immutable `bytes`) — an in-place stdlib mutator such as
            // `struct.pack_into` writes into it, and the write-back after the call
            // reflects that back into the pythonrs object.
            Some(PyObj::Bytearray(b)) => Ok(PyByteArray::new(py, b).into_any()),
            Some(PyObj::BigInt(b)) => {
                // pyo3 has no num-bigint bridge enabled; round-trip through the
                // decimal string, which CPython's `int` parses into an exact int.
                let int_ctor = py
                    .import("builtins")
                    .and_then(|m| m.getattr("int"))
                    .map_err(|e| e.to_string())?;
                int_ctor.call1((b.to_string(),)).map_err(|e| e.to_string())
            }
            Some(PyObj::List(items)) => {
                let elems = marshal_seq(host, py, items)?;
                Ok(PyList::new(py, elems)
                    .map_err(|e| e.to_string())?
                    .into_any())
            }
            Some(PyObj::Tuple(items)) => {
                let elems = marshal_seq(host, py, items)?;
                Ok(PyTuple::new(py, elems)
                    .map_err(|e| e.to_string())?
                    .into_any())
            }
            Some(PyObj::Set(s)) => {
                let elems = marshal_seq(host, py, &s.values().cloned().collect::<Vec<_>>())?;
                Ok(PySet::new(py, &elems)
                    .map_err(|e| e.to_string())?
                    .into_any())
            }
            Some(PyObj::Frozenset(s)) => {
                let elems = marshal_seq(host, py, &s.values().cloned().collect::<Vec<_>>())?;
                Ok(PyFrozenSet::new(py, &elems)
                    .map_err(|e| e.to_string())?
                    .into_any())
            }
            Some(PyObj::Range { start, stop, step }) => {
                let range = py
                    .import("builtins")
                    .and_then(|m| m.getattr("range"))
                    .map_err(|e| e.to_string())?;
                range
                    .call1((*start, *stop, *step))
                    .map_err(|e| e.to_string())
            }
            Some(PyObj::Complex(re, im)) => {
                let cplx = py
                    .import("builtins")
                    .and_then(|m| m.getattr("complex"))
                    .map_err(|e| e.to_string())?;
                cplx.call1((*re, *im)).map_err(|e| e.to_string())
            }
            Some(PyObj::Deque { items, maxlen }) => {
                let elems = marshal_seq(host, py, &items.iter().cloned().collect::<Vec<_>>())?;
                let pylist = PyList::new(py, elems).map_err(|e| e.to_string())?;
                let deque = py
                    .import("collections")
                    .and_then(|m| m.getattr("deque"))
                    .map_err(|e| e.to_string())?;
                match maxlen {
                    Some(n) => deque.call1((pylist, *n)),
                    None => deque.call1((pylist,)),
                }
                .map_err(|e| e.to_string())
            }
            Some(PyObj::Dict(d)) => {
                let dict = PyDict::new(py);
                for (k, val) in d.values() {
                    let pk = value_to_py(host, py, k)?;
                    let pv = value_to_py(host, py, val)?;
                    dict.set_item(pk, pv).map_err(|e| e.to_string())?;
                }
                Ok(dict.into_any())
            }
            Some(PyObj::Foreign(id)) => fetch(py, *id),
            // A pythonrs lazy iterator (generator / zip / map / filter /
            // enumerate / composite iterator) passed into a CPython call
            // (`itertools.takewhile(pred, gen())`, `"".join(gen())`, …) is wrapped
            // as a CPython iterator whose `__next__` drives fusevm one step at a
            // time — so an infinite generator is never materialized.
            Some(
                PyObj::Generator { .. }
                | PyObj::Iter(_)
                | PyObj::Zip { .. }
                | PyObj::MapObj { .. }
                | PyObj::FilterObj { .. }
                | PyObj::EnumerateObj { .. }
                | PyObj::CallIter { .. },
            ) => {
                let it = PyrsIterator { target: v.clone() };
                Py::new(py, it)
                    .map(|p| p.into_any().into_bound(py))
                    .map_err(|e| e.to_string())
            }
            // A pythonrs callable (lambda / def / builtin / bound method / partial
            // / lru_cache) passed as a callback (`functools.reduce(f, …)`,
            // `sorted(key=f)`, …) is wrapped so CPython can call back into fusevm.
            Some(
                PyObj::Func(_)
                | PyObj::Builtin(_)
                | PyObj::BoundMethod { .. }
                | PyObj::Partial { .. }
                | PyObj::LruCache { .. }
                | PyObj::StaticMethod(_)
                | PyObj::ClassMethod(_),
            ) => {
                let cb = PyrsCallable { target: v.clone() };
                Py::new(py, cb)
                    .map(|p| p.into_any().into_bound(py))
                    .map_err(|e| e.to_string())
            }
            // A native pythonrs class passed into a CPython call (`@dataclass`,
            // `dataclasses.fields(Cls)`): build a CPython mirror over `object`
            // with the class namespace — methods cross as `PyrsCallable`
            // descriptors (they bind `self`), `__annotations__`/class-vars by
            // value — so the decorator can read the fields and add methods.
            Some(PyObj::Class(cname)) => {
                let members: Vec<(String, Value)> = host
                    .classes
                    .get(cname)
                    .map(|c| c.ns.iter().map(|(k, val)| (k.clone(), val.clone())).collect())
                    .unwrap_or_default();
                let ns_dict = PyDict::new(py);
                for (k, val) in &members {
                    let pv = value_to_py(host, py, val)?;
                    ns_dict.set_item(k.as_str(), pv).map_err(|e| e.to_string())?;
                }
                if !ns_dict.contains("__module__").unwrap_or(false) {
                    let _ = ns_dict.set_item("__module__", "__main__");
                }
                let _ = ns_dict.set_item("__qualname__", cname.as_str());
                let obj_base = py
                    .import("builtins")
                    .and_then(|m| m.getattr("object"))
                    .map_err(|e| e.to_string())?;
                let bases = PyTuple::new(py, &[obj_base]).map_err(|e| e.to_string())?;
                let helper = make_class_helper(py)?;
                helper
                    .call1((cname.as_str(), bases, ns_dict))
                    .map_err(|e| e.to_string())
            }
            _ => Err(crate::host::type_error(&format!(
                "cannot pass '{}' to a CPython stdlib call",
                host.type_name(v)
            ))),
        },
        _ => Err(crate::host::type_error(
            "unsupported value for CPython call",
        )),
    }
}

fn marshal_seq<'py>(
    host: &PyHost,
    py: Python<'py>,
    items: &[Value],
) -> Result<Vec<Bound<'py, PyAny>>, String> {
    items.iter().map(|it| value_to_py(host, py, it)).collect()
}

/// CPython object → pythonrs `Value`. Only the *exact* representable types come
/// back by value; a subclass (namedtuple, `OrderedDict`, `Counter`, `IntEnum`, a
/// `str` subclass, …) stays a `Foreign` handle so its CPython repr/behavior is
/// preserved. Anything unrepresentable is likewise kept as `Foreign`.
fn py_to_value(host: &mut PyHost, py: Python, obj: &Bound<PyAny>) -> Result<Value, String> {
    if obj.is_none() {
        return Ok(Value::Undef);
    }
    if obj.is_exact_instance_of::<PyBool>() {
        return Ok(Value::Bool(
            obj.extract::<bool>().map_err(|e| e.to_string())?,
        ));
    }
    if obj.is_exact_instance_of::<PyInt>() {
        return Ok(match obj.extract::<i64>() {
            Ok(n) => Value::Int(n),
            // Out of i64 range → arbitrary-precision, parsed from the decimal repr.
            Err(_) => {
                let s = obj.str().map_err(|e| e.to_string())?.to_string();
                match s.parse::<num_bigint::BigInt>() {
                    Ok(b) => host.alloc(PyObj::BigInt(b)),
                    Err(_) => return Err(format!("ffi: cannot marshal int '{s}'")),
                }
            }
        });
    }
    if obj.is_exact_instance_of::<PyFloat>() {
        return Ok(Value::Float(
            obj.extract::<f64>().map_err(|e| e.to_string())?,
        ));
    }
    if obj.is_exact_instance_of::<pyo3::types::PyString>() {
        return Ok(host.new_str(obj.extract::<String>().map_err(|e| e.to_string())?));
    }
    if obj.is_exact_instance_of::<PyBytes>() {
        let b = obj.downcast::<PyBytes>().map_err(|e| e.to_string())?;
        return Ok(host.alloc(PyObj::Bytes(b.as_bytes().to_vec())));
    }
    if obj.is_exact_instance_of::<PyList>() {
        let list = obj.downcast::<PyList>().map_err(|e| e.to_string())?;
        let items = unmarshal_seq(host, py, list.iter())?;
        return Ok(host.new_list(items));
    }
    if obj.is_exact_instance_of::<PyTuple>() {
        let tup = obj.downcast::<PyTuple>().map_err(|e| e.to_string())?;
        let items = unmarshal_seq(host, py, tup.iter())?;
        return Ok(host.new_tuple(items));
    }
    if obj.is_exact_instance_of::<PyDict>() {
        let dict = obj.downcast::<PyDict>().map_err(|e| e.to_string())?;
        let mut map = indexmap::IndexMap::new();
        for (k, v) in dict.iter() {
            let kv = py_to_value(host, py, &k)?;
            let vv = py_to_value(host, py, &v)?;
            let key = host.to_key(&kv)?;
            map.insert(key, (kv, vv));
        }
        return Ok(host.new_dict(map));
    }
    if obj.is_exact_instance_of::<PySet>() || obj.is_exact_instance_of::<PyFrozenSet>() {
        let mut map = indexmap::IndexMap::new();
        for it in obj.try_iter().map_err(|e| e.to_string())? {
            let iv = py_to_value(host, py, &it.map_err(|e| e.to_string())?)?;
            let key = host.to_key(&iv)?;
            map.insert(key, iv);
        }
        return Ok(host.new_set(map));
    }
    // Anything else stays on the CPython side behind a Foreign handle.
    Ok(host.alloc(PyObj::Foreign(store(obj.clone().unbind()))))
}

fn unmarshal_seq<'py, I>(host: &mut PyHost, py: Python, items: I) -> Result<Vec<Value>, String>
where
    I: Iterator<Item = Bound<'py, PyAny>>,
{
    items.map(|it| py_to_value(host, py, &it)).collect()
}

// ── operations routed on a Foreign handle ────────────────────────────────────

/// `foreign.name` — attribute access (submodules, functions, constants, …).
pub fn get_attr(host: &mut PyHost, id: u32, name: &str) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let attr = obj.getattr(name).map_err(|e| e.to_string())?;
        py_to_value(host, py, &attr)
    })
}

/// `foreign(*args, **kwargs)` — call the foreign object.
///
/// The host borrow is dropped for the duration of the CPython call so a pythonrs
/// callback (a `PyrsCallable` passed as an argument) can re-enter the host.
pub fn call(id: u32, args: Vec<Value>, kwargs: Vec<(String, Value)>) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        invoke_bound(py, &obj, &args, &kwargs)
    })
}

/// `foreign.name(*args, **kwargs)` — call a method on the foreign object.
pub fn call_method(
    id: u32,
    name: &str,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let method = obj.getattr(name).map_err(|e| e.to_string())?;
        invoke_bound(py, &method, &args, &kwargs)
    })
}

/// Marshal args (host borrow held only here, no user code runs), make the CPython
/// call (no host borrow — reverse callbacks are free to run), then marshal the
/// result back (fresh host borrow).
fn invoke_bound(
    py: Python,
    callable: &Bound<PyAny>,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value, String> {
    let (arg_tuple, kw) = with_host(|h| build_call_args(h, py, args, kwargs))?;
    let result = callable
        .call(&arg_tuple, kw.as_ref())
        .map_err(|e| e.to_string())?;
    with_host(|h| {
        // Reflect any in-place mutation the stdlib call made to a by-value
        // mutable-container argument (`heapq.heapify(lst)`, `random.shuffle(lst)`,
        // `struct.pack_into(fmt, buf, …)`) back into the pythonrs object.
        writeback_mutated_args(h, py, args, &arg_tuple);
        py_to_value(h, py, &result)
    })
}

/// The pythonrs mutable-container kinds whose in-place mutation by a CPython
/// stdlib call must be copied back after the call. Immutable arguments
/// (`str`/`tuple`/`frozenset`/`bytes`/scalars), `Foreign` handles (which are the
/// *same* CPython object — mutations are already visible), and callables never
/// need write-back.
#[derive(Clone, Copy)]
enum MutKind {
    List,
    Bytearray,
    Deque(Option<usize>),
}

/// For each positional argument that was marshaled by value as a mutable
/// container, re-read the (possibly mutated) CPython object and overwrite the
/// existing pythonrs heap slot *in place*, so aliases to the same object observe
/// the mutation too. Best-effort: a container whose contents don't round-trip to
/// representable values (a `Foreign` element) is left untouched rather than
/// re-wrapped — that would allocate a fresh handle and is never what an in-place
/// mutator produces in practice.
fn writeback_mutated_args(
    host: &mut PyHost,
    py: Python,
    args: &[Value],
    arg_tuple: &Bound<PyTuple>,
) {
    for (i, orig) in args.iter().enumerate() {
        let kind = match host.get(orig) {
            Some(PyObj::List(_)) => MutKind::List,
            Some(PyObj::Bytearray(_)) => MutKind::Bytearray,
            Some(PyObj::Deque { maxlen, .. }) => MutKind::Deque(*maxlen),
            _ => continue,
        };
        let Ok(cpy) = arg_tuple.get_item(i) else {
            continue;
        };
        if let Some(obj) = rebuild_mutable(host, py, &cpy, kind) {
            if let Some(slot) = host.get_mut(orig) {
                *slot = obj;
            }
        }
    }
}

/// Rebuild the pythonrs `PyObj` for a mutable container from its CPython object
/// after an in-place mutation. Returns `None` (skip write-back) if any element is
/// not representable by value.
fn rebuild_mutable(
    host: &mut PyHost,
    py: Python,
    cpy: &Bound<PyAny>,
    kind: MutKind,
) -> Option<PyObj> {
    match kind {
        MutKind::Bytearray => {
            let ba = cpy.downcast::<PyByteArray>().ok()?;
            Some(PyObj::Bytearray(ba.to_vec()))
        }
        MutKind::List => {
            let items = pure_seq(host, py, cpy)?;
            Some(PyObj::List(items))
        }
        MutKind::Deque(maxlen) => {
            let items = pure_seq(host, py, cpy)?;
            Some(PyObj::Deque {
                items: items.into_iter().collect(),
                maxlen,
            })
        }
    }
}

/// Iterate a CPython container and marshal every element by value, yielding
/// `None` if any element is not representable (so the caller skips write-back).
fn pure_seq(host: &mut PyHost, py: Python, cpy: &Bound<PyAny>) -> Option<Vec<Value>> {
    let it = cpy.try_iter().ok()?;
    let mut out = Vec::new();
    for item in it {
        out.push(pure_value(host, py, &item.ok()?)?);
    }
    Some(out)
}

/// A CPython object → pythonrs `Value` *without* the `Foreign` fallback: returns
/// `None` for anything not representable by value. Used only by write-back, whose
/// contract is "reflect an in-place mutation losslessly, or leave the object
/// alone" — never allocate a new `Foreign` handle (that would leak on every call
/// and change identity). `py_to_value` is the authoritative marshaler and keeps
/// unrepresentable results as `Foreign`; the two contracts differ, so they stay
/// separate functions.
fn pure_value(host: &mut PyHost, py: Python, obj: &Bound<PyAny>) -> Option<Value> {
    if obj.is_none() {
        return Some(Value::Undef);
    }
    if obj.is_exact_instance_of::<PyBool>() {
        return obj.extract::<bool>().ok().map(Value::Bool);
    }
    if obj.is_exact_instance_of::<PyInt>() {
        return match obj.extract::<i64>() {
            Ok(n) => Some(Value::Int(n)),
            Err(_) => {
                let s = obj.str().ok()?.to_string();
                s.parse::<num_bigint::BigInt>()
                    .ok()
                    .map(|b| host.alloc(PyObj::BigInt(b)))
            }
        };
    }
    if obj.is_exact_instance_of::<PyFloat>() {
        return obj.extract::<f64>().ok().map(Value::Float);
    }
    if obj.is_exact_instance_of::<PyString>() {
        return obj.extract::<String>().ok().map(|s| host.new_str(s));
    }
    if obj.is_exact_instance_of::<PyBytes>() {
        let b = obj.downcast::<PyBytes>().ok()?;
        return Some(host.alloc(PyObj::Bytes(b.as_bytes().to_vec())));
    }
    if obj.is_exact_instance_of::<PyByteArray>() {
        let b = obj.downcast::<PyByteArray>().ok()?;
        return Some(host.alloc(PyObj::Bytearray(b.to_vec())));
    }
    if obj.is_exact_instance_of::<PyList>() {
        let items = pure_seq(host, py, obj)?;
        return Some(host.new_list(items));
    }
    if obj.is_exact_instance_of::<PyTuple>() {
        let items = pure_seq(host, py, obj)?;
        return Some(host.new_tuple(items));
    }
    None
}

#[allow(clippy::type_complexity)]
fn build_call_args<'py>(
    host: &PyHost,
    py: Python<'py>,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<(Bound<'py, PyTuple>, Option<Bound<'py, PyDict>>), String> {
    let py_args = marshal_seq(host, py, args)?;
    let arg_tuple = PyTuple::new(py, py_args).map_err(|e| e.to_string())?;
    let kw = if kwargs.is_empty() {
        None
    } else {
        let d = PyDict::new(py);
        for (k, v) in kwargs {
            let pv = value_to_py(host, py, v)?;
            d.set_item(k.as_str(), pv).map_err(|e| e.to_string())?;
        }
        Some(d)
    };
    Ok((arg_tuple, kw))
}

// A fusevm-side callable (lambda / def / builtin / …) exposed to CPython so it
// can be used as a stdlib callback. `__call__` marshals the CPython arguments to
// pythonrs values, runs the callable on fusevm (no host borrow held here), and
// marshals the result back. (Plain `//`, not `///`: a doc comment would become
// the pyclass `__doc__` and leak as every wrapped callable's `__doc__`.)
// `dict` gives each proxy a `__dict__`, so CPython code can set attributes on it
// (`functools.update_wrapper` does `setattr(wrapper, '__module__', …)` and
// `wrapper.__dict__.update(...)`). Attributes it doesn't set fall through to
// `__getattr__`, which delegates the wrapped callable's dunders.
#[pyclass(dict)]
struct PyrsCallable {
    target: Value,
}

#[pymethods]
impl PyrsCallable {
    /// Delegate a missing attribute (function dunder like `__name__` /
    /// `__qualname__` / `__module__`) to the wrapped fusevm callable, so
    /// `functools.update_wrapper` can copy them off it. `__getattr__` runs only
    /// after normal lookup (including the instance `__dict__`) misses, so a
    /// wraps-assigned attribute wins over the delegate. A dunder the target
    /// lacks becomes `AttributeError` (which `update_wrapper` silently skips).
    fn __getattr__(&self, py: Python, name: String) -> PyResult<Py<PyAny>> {
        match with_host(|h| h.get_attr(&self.target, &name)) {
            Ok(v) => with_host(|h| value_to_py(h, py, &v))
                .map(|b| b.unbind())
                .map_err(pyo3::exceptions::PyRuntimeError::new_err),
            Err(e) => Err(pyo3::exceptions::PyAttributeError::new_err(e)),
        }
    }

    /// Descriptor protocol: a pythonrs function stored in a CPython-built class
    /// (an `enum`/`dataclass`/`NamedTuple` method) binds `self` on instance
    /// access, and — because it now has `__get__` — CPython recognizes it as a
    /// method rather than a plain attribute (Enum's `_EnumDict` would otherwise
    /// make it a member). Class access (`obj is None`) yields the unbound proxy.
    fn __get__<'py>(
        slf: Bound<'py, Self>,
        py: Python<'py>,
        obj: Option<Bound<'py, PyAny>>,
        _owner: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        match obj {
            Some(instance) if !instance.is_none() => py
                .import("types")?
                .getattr("MethodType")?
                .call1((slf, instance)),
            _ => Ok(slf.into_any()),
        }
    }

    #[pyo3(signature = (*args, **kwargs))]
    fn __call__(
        &self,
        py: Python,
        args: &Bound<PyTuple>,
        kwargs: Option<&Bound<PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        let to_pyerr = |e: String| pyo3::exceptions::PyRuntimeError::new_err(e);
        // Marshal CPython args → pythonrs values (host borrow window).
        let rs_args: Vec<Value> = with_host(|h| {
            args.iter()
                .map(|a| py_to_value(h, py, &a))
                .collect::<Result<_, _>>()
        })
        .map_err(to_pyerr)?;
        let rs_kwargs: Vec<(String, Value)> = match kwargs {
            None => Vec::new(),
            Some(d) => with_host(|h| {
                d.iter()
                    .map(|(k, v)| {
                        let key = k.str().map_err(|e| e.to_string())?.to_string();
                        Ok((key, py_to_value(h, py, &v)?))
                    })
                    .collect::<Result<_, String>>()
            })
            .map_err(to_pyerr)?,
        };
        // Run the fusevm callable with NO host borrow held (invoke re-enters it).
        let result = crate::host::invoke(&self.target, rs_args, rs_kwargs).map_err(to_pyerr)?;
        // Marshal the result back to a CPython object (host borrow window).
        with_host(|h| value_to_py(h, py, &result))
            .map(|b| b.unbind())
            .map_err(to_pyerr)
    }
}

// A CPython iterator backed by a pythonrs lazy iterator (generator / zip / map /
// filter / enumerate / composite). `__next__` advances fusevm one step with NO
// host borrow held (`iter_step` manages its own borrows and may re-enter
// pythonrs), so CPython can consume `itertools.takewhile(pred, gen())` and the
// like without materializing an (possibly infinite) source. Plain `//` (not
// `///`) so the doc text doesn't become a leaking `__doc__`.
#[pyclass]
struct PyrsIterator {
    target: Value,
}

#[pymethods]
impl PyrsIterator {
    fn __iter__(slf: PyRef<Self>) -> PyRef<Self> {
        slf
    }

    fn __next__(&self, py: Python) -> PyResult<Option<Py<PyAny>>> {
        let to_pyerr = |e: String| pyo3::exceptions::PyRuntimeError::new_err(e);
        match crate::host::iter_step(&self.target).map_err(to_pyerr)? {
            // `None` from `__next__` raises `StopIteration` in pyo3.
            None => Ok(None),
            Some(v) => with_host(|h| value_to_py(h, py, &v))
                .map(|b| Some(b.unbind()))
                .map_err(to_pyerr),
        }
    }
}

/// `foreign[idx]`.
pub fn get_item(host: &mut PyHost, id: u32, idx: &Value) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let key = value_to_py(host, py, idx)?;
        let item = obj.get_item(key).map_err(|e| e.to_string())?;
        py_to_value(host, py, &item)
    })
}

/// `iter(foreign)` — returns a `Foreign` iterator handle.
pub fn make_iter(host: &mut PyHost, id: u32) -> Result<Value, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let it = obj.try_iter().map_err(|e| e.to_string())?;
        Ok(host.alloc(PyObj::Foreign(store(it.into_any().unbind()))))
    })
}

/// `next(foreign)` — `None` on `StopIteration`. Caller holds the host borrow, so
/// only safe for iterators that never re-enter pythonrs during `next()` (a plain
/// CPython container). Callback-driving iterators must use [`iter_next_cb`].
pub fn iter_next(host: &mut PyHost, id: u32) -> Result<Option<Value>, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let mut it = obj.try_iter().map_err(|e| e.to_string())?;
        match it.next() {
            None => Ok(None),
            Some(Ok(item)) => Ok(Some(py_to_value(host, py, &item)?)),
            Some(Err(e)) => Err(e.to_string()),
        }
    })
}

/// `next(foreign)` for the borrow-free iteration path (`host::iter_step` /
/// `host::iter_vec`). The caller must NOT hold the host borrow: advancing a lazy
/// CPython iterator (`itertools.starmap`/`dropwhile`/`takewhile`/`filterfalse`
/// over a pythonrs callable) runs that callable, which re-enters the host. The
/// advance therefore happens with no borrow held; the result is marshaled under
/// a fresh short borrow, exactly like `invoke_bound`.
pub fn iter_next_cb(id: u32) -> Result<Option<Value>, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let mut it = obj.try_iter().map_err(|e| e.to_string())?;
        match it.next() {
            None => Ok(None),
            Some(Ok(item)) => Ok(Some(with_host(|h| py_to_value(h, py, &item))?)),
            Some(Err(e)) => Err(e.to_string()),
        }
    })
}

/// `item in foreign`.
pub fn contains(host: &mut PyHost, id: u32, item: &Value) -> Result<bool, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let needle = value_to_py(host, py, item)?;
        obj.contains(needle).map_err(|e| e.to_string())
    })
}

/// A binary/comparison operator (`+ - * / // % ** @ & | ^ << >>`,
/// `== != < <= > >=`) where at least one operand is a `Foreign` CPython object.
///
/// `func` is the corresponding `operator`-module attribute (`add`, `truediv`,
/// `mod`, `and_`, `lshift`, `lt`, `eq`, …). Both operands are marshaled to CPython
/// (a native operand crosses by value via the in-marshaler; a `Foreign` passes its
/// underlying object straight through), the real CPython operation runs, and the
/// result marshals back — by value when representable, else a fresh `Foreign`
/// (so `date + timedelta` → a CPython `date`, `Decimal + Decimal` → an exact
/// `Decimal`, `datetime < datetime` → a `bool`). A `TypeError`/`NotImplemented`
/// from CPython surfaces as a pythonrs error string, never a bridge panic.
pub fn binary_op(host: &mut PyHost, func: &str, a: &Value, b: &Value) -> Result<Value, String> {
    Python::with_gil(|py| {
        let pa = value_to_py(host, py, a)?;
        let pb = value_to_py(host, py, b)?;
        let op = py
            .import("operator")
            .and_then(|m| m.getattr(func))
            .map_err(|e| e.to_string())?;
        let res = op.call1((pa, pb)).map_err(|e| e.to_string())?;
        py_to_value(host, py, &res)
    })
}

/// `float(foreign)` — run CPython's own `float()` on the object so `__float__`
/// (`Fraction`, `Decimal`, `numpy` scalars, …) and `__index__` are honored. A
/// `TypeError` (no conversion) surfaces as a pythonrs error string.
pub fn to_float(id: u32) -> Result<f64, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        let f = py
            .import("builtins")
            .and_then(|b| b.getattr("float"))
            .and_then(|f| f.call1((obj,)))
            .map_err(|e| e.to_string())?;
        f.extract::<f64>().map_err(|e| e.to_string())
    })
}

/// [`binary_op`] for the borrow-free path (`numeric_hook`): the caller must NOT
/// hold the host borrow. The operator runs in CPython with no borrow held, so an
/// operand whose comparison/arithmetic calls back into pythonrs (a
/// `functools.cmp_to_key` wrapper's `__lt__` invoking the user cmp function) can
/// re-enter the host. Args and result are marshaled under fresh short borrows.
pub fn binary_op_cb(func: &str, a: &Value, b: &Value) -> Result<Value, String> {
    Python::with_gil(|py| {
        let (pa, pb) = with_host(|h| -> Result<_, String> {
            Ok((value_to_py(h, py, a)?, value_to_py(h, py, b)?))
        })?;
        let op = py
            .import("operator")
            .and_then(|m| m.getattr(func))
            .map_err(|e| e.to_string())?;
        let res = op.call1((pa, pb)).map_err(|e| e.to_string())?;
        with_host(|h| py_to_value(h, py, &res))
    })
}

/// A unary operator on a `Foreign` CPython object: negation (`-x` → `neg`), unary
/// plus (`+x` → `pos`), bitwise invert (`~x` → `invert`), or `abs(x)` (`abs`).
/// `func` is the `operator`-module attribute; the CPython result marshals back the
/// same way as [`binary_op`].
pub fn unary_op(host: &mut PyHost, func: &str, v: &Value) -> Result<Value, String> {
    Python::with_gil(|py| {
        let pv = value_to_py(host, py, v)?;
        let op = py
            .import("operator")
            .and_then(|m| m.getattr(func))
            .map_err(|e| e.to_string())?;
        let res = op.call1((pv,)).map_err(|e| e.to_string())?;
        py_to_value(host, py, &res)
    })
}

/// `len(foreign)`.
pub fn len(id: u32) -> Result<usize, String> {
    Python::with_gil(|py| {
        let obj = fetch(py, id)?;
        obj.len().map_err(|e| e.to_string())
    })
}

/// `str(foreign)`.
pub fn str_of(id: u32) -> String {
    Python::with_gil(
        |py| match fetch(py, id).and_then(|o| o.str().map_err(|e| e.to_string())) {
            Ok(s) => s.to_string(),
            Err(e) => e,
        },
    )
}

/// `repr(foreign)`.
pub fn repr_of(id: u32) -> String {
    Python::with_gil(
        |py| match fetch(py, id).and_then(|o| o.repr().map_err(|e| e.to_string())) {
            Ok(s) => s.to_string(),
            Err(e) => e,
        },
    )
}

/// `bool(foreign)`.
pub fn truthy(id: u32) -> bool {
    Python::with_gil(|py| {
        fetch(py, id)
            .ok()
            .and_then(|o| o.is_truthy().ok())
            .unwrap_or(true)
    })
}

/// The CPython type name of a foreign object (`module`, `re.Pattern`, …).
pub fn type_name(id: u32) -> String {
    Python::with_gil(|py| match fetch(py, id) {
        Ok(obj) => obj
            .get_type()
            .name()
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "object".into()),
        Err(_) => "object".into(),
    })
}
